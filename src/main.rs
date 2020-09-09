#[macro_use] mod slang;
mod ready;
mod expiroset;
mod secretary;
mod minecraft;
mod filters;
mod util; mod exitcode;

// use crate slang
use crate::slang::*;

// import crate symbols
use minecraft::{get_minecraft, run_minecraft, handle_minecraft_io, AttendantRequest};
use util::{sigint_process, sigkill_process};
use exitcode::ExitCode;

// Import other symbols
use tokio::join;

use std::path::PathBuf;
use std::future::Future;
use tokio::net::UnixListener;
use tokio::time::delay_for;

mod error {
	use thiserror::Error;

	use crate::minecraft::MinecraftError;
	use std::path::PathBuf;

	#[derive(Error, Debug)]
	pub(crate) enum ProgramFailure {
		#[error(transparent)]
		Minecraft(#[from] MinecraftError),
		#[error("Failed to bind to socket {0}")]
		CreateSocket(PathBuf, #[source] tokio::io::Error),
	}
}
use error::ProgramFailure;

const _OUR_NAME :&str = "mcclwrap";
const SOCKET_PATH :&str  = "test.sock";
const GUARD_TIMEOUT_SECS :u64 = 15;     // How long an input guard lasts by default

const RT_SHUTDOWN_DELAY :Duration = Duration::from_millis(1000);
const FORCE_SHUTDOWN_DELAY :Duration = Duration::from_millis(1000);

// === Other listeners ===

// TODO make it look better and handle tab completion
fn handle_stdin	(mut tx_req :mpsc::Sender<AttendantRequest>) -> tokio::task::JoinHandle<()> {
	use minecraft::AttendantCommand::SendCommand;

	let client_id = secretary::get_attendant_id();
	tokio::spawn(async move { 
		let mut stdin = BufReader::new(io::stdin()).lines();
		while let Some(line) = stdin.next().await {
			let line = line.expect("io or unicode error");

			let (tx, rx) = ready::channel();
			let slash = util::strip_leading_ascii_hspace(&line).to_string();
			let req = AttendantRequest {
				client_id,
				done: tx,
				command: SendCommand(slash),
			};

			if let Err(_) = tx_req.send(req).await {
				eprintln!("🎁 Minecraft console input isn't handled anymore");
			}
			if let None = rx.await {
				eprintln!("🎁 Failed to process standard input command"); // FIXME ?
			}
		}
	})
}

// === Cleanup and Shutdown ===

// Returns the signal receiver channel
fn get_signal_channel () -> mpsc::Receiver<()> {
	use tokio::signal::unix::{signal, SignalKind};

	let (mut tx, rx) = mpsc::channel::<()>(2);
	
	tokio::spawn(async move {
		// Create signal streams
		let sigints = signal(SignalKind::interrupt()).unwrap();
		let sigterms = signal(SignalKind::terminate()).unwrap();
		
		let mut sigs = sigints.merge(sigterms);
		
		// Send them into the channel
		while let Some(_) = sigs.next().await {
			if let Err(_) = tx.send(()).await {
				eprintln!("🎁 Signals are no longer handled");
			}
		}
	});
	
	rx
}

async fn get_cleanup_task (
	p :PathBuf, mc_pid :Pid,
	mc_handle :tokio::task::JoinHandle<()>, rx_closed :ready::Receiver,
	)
{
	// == Task definitions ==
	
	// Remove socket file
	let remove_socket_file = tokio::spawn(async move {
		// DEV make it really slow
		// delay_for(Duration::from_secs(2)).await;
		
		if p.exists() {
			if let Err(_) = std::fs::remove_file(&p) {
				eprintln!("🎁 Failed to remove socket file");
			}
		}
	});
	
	// Wait for server to exit
	let wait_for_mc = tokio::spawn(async move {
		// Kill the server if it's closed but doesn't exit
		let kill_server_if_closed = async move {
			rx_closed.await;
			delay_for(Duration::from_secs(1)).await;
			match sigkill_process(mc_pid) {
				Ok(_) => println!("🎁 Sent SIGKILL to server after it closed"),
				Err(e) => eprintln!("🎁 Failed to send SIGKILL to server! {}", e),
			};
		};
		
		select! {
			_ = kill_server_if_closed => (),
			_ = mc_handle => (),
		}
	});
	
	// TODO drop socket listener
	
	// == Task execution ==
	
	if let (Ok(_), Ok(_)) = join!(remove_socket_file, wait_for_mc) {
	} else {
		eprintln!("🎁 A cleanup task panicked! Beware of leftover files and zombie processes.");
	}
}

// Handles stop signals:
async fn handle_stop_signals (
	cleanup_task :impl Future, mc_pid :Pid,
	rx_closed :ready::Receiver,
	mut rx_sig :mpsc::Receiver<()>, mut rx_stop :mpsc::Receiver<()>,
) {
	// Wait for a signal to trigger cleanup
	select! {
		_ = rx_closed => (),
		_ = rx_sig.recv() => (),
		_ = rx_stop.recv() => {
			match sigint_process(mc_pid) {
				Ok(_) => println!("🎁 Sent SIGINT to the server"),
				Err(e) => eprintln!("🎁 Failed to send SIGINT to the server! {}", e),
			};
		},
	}
	
	println!("🎁 Cleaning up before shutting down…");
	// A second signal will trigger fast shutdown
	let shutdown_now = tokio::spawn(async move {
		while let Some(_) = rx_sig.recv().await {
			break;
		}
		
		delay_for(FORCE_SHUTDOWN_DELAY).await;
	});
	
	// Finish if the cleaning finished or it was forced to exit
	select! {
		_ = cleanup_task => (),
		_ = shutdown_now => eprintln!("🎁 Cleanup didn't complete in time! Beware of leftover files or zombie processes"),
	};
}

// === Main functions ===

async fn async_main () -> Result<(), error::ProgramFailure> {
	let socket_path = PathBuf::from(SOCKET_PATH);
	
	let (tx_stop, rx_stop) = mpsc::channel::<()>(2);
	
	// Create the socket
	let listener = terror! {
		UnixListener::bind(&socket_path)
		=> |e| ProgramFailure::CreateSocket(socket_path, e)
	};
	
	// Get minecraft process
	let use_color = ["-Dterminal.jline=false", "-Dterminal.ansi=true"];
	let java_args = ["-jar", "paper-131.jar", "-nogui"];
	let (mc, mc_in, mc_out, mc_pid) = terror! {
		get_minecraft("java", &[&use_color[..], &java_args[..]].concat())
	};
	
	// Run minecraft in the background
	let mc_handle = run_minecraft(mc, tx_stop);
	
	// Setup listeners for SIGINT/SIGTERM, minecraft console, socket clients, and stdin
	let rx_sig = get_signal_channel(); // CHECK why is this here ?
	let (tx_req, rx_ready, rx_closed) = handle_minecraft_io(mc_in, mc_out);
	secretary::listen_on_socket(listener, rx_ready.clone(), tx_req.clone());
	handle_stdin(tx_req);
	
	// TODO start timers

	// Prepare cleanup task
	let cleanup_task = get_cleanup_task(
		socket_path, mc_pid, mc_handle, rx_closed.clone()
	);

	tokio::spawn(async {
		let mut interval = tokio::time::interval(Duration::from_secs(3));
		loop {
			interval.tick().await;
			println!("DEBUG tick");
		}
	});
	
	println!("🎁 Initialization complete");

	// Block main thread until we're told to stop
	handle_stop_signals(
		cleanup_task, mc_pid, rx_closed, rx_sig, rx_stop
	).await;
	delay_for(Duration::from_secs(10)).await;
	
	Ok(())
}

fn real_main () -> ExitCode {
	let mut rt = tear! {
		tokio::runtime::Runtime::new()
		=> |_| { eprintln!("🎁 Failed to start runtime"); exitcode::PANIC }
	};
	
	// Start async_main
	let r = rt.block_on(async_main());
	let failed = r.is_err();
	if let Err(e) = r {
		eprintln!("🎁 Error: {}", e);
	};
	
	println!("🎁 Shutting down runtime");
	rt.shutdown_timeout(RT_SHUTDOWN_DELAY);
	
	println!("🎁 Bye");
	
	if failed {
		exitcode::ERROR
	} else {
		exitcode::OK
	}
}

fn main () {
	let code = real_main();
	std::process::exit(code);
}
