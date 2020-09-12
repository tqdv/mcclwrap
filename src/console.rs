//! Terminal console io

use crate::slang::*;
use crate::ready;
use crate::minecraft::{AttendantCommand, AttendantRequest};
use crate::secretary::{self, AttendantId};
use std::fmt;
use std::task::Poll;
use tokio::task::yield_now;
use linefeed::{Interface, ReadResult, DefaultTerminal, Signal as lfSignal};
use nix::sys::signal::Signal as nixSignal;
use futures::future::poll_fn;
use std::io::Write as _;

const INPUT_TIMEOUT :Duration = Duration::from_millis(10);

/// Shorthand for the shared interface type
type TermInterface = Arc<Interface<DefaultTerminal>>;

lazy_static! {
	/// Static `Option<Arc<linefeed::Interface>>` used in Console and console::out()
	static ref INTERFACE :Option<TermInterface> = Interface::new(crate::OUR_NAME).ok()
		.map(|iface| { iface.set_prompt("> "); iface})
		.map(Arc::new);
}

/// A struct just to add a method on it
struct ConsoleAttendant {
	tx_req :mpsc::Sender<AttendantRequest>,
	client_id: AttendantId,
}

/// State for the user console
pub(crate) struct Console {
	interface: Option<TermInterface>,
	attendant: ConsoleAttendant,
	rx_closed: ready::Receiver,
}

impl ConsoleAttendant {
	/// Send the user's command. Doesn't consume self because of ownership rules in ::handle_stdio
	async fn send_command (&mut self, line :String) {
		let (tx, rx) = ready::channel();
		let slash = util::strip_leading_ascii_hspace(&line).to_string();
		let req = AttendantRequest {
			client_id: self.client_id,
			done: tx,
			command: AttendantCommand::SendCommand(slash),
		};

		if let Err(_) = self.tx_req.send(req).await {
			eprintln!("🎁 Minecraft console input isn't handled anymore");
		}
		if let None = rx.await {
			eprintln!("🎁 Failed to process standard input command"); // FIXME ?
		}
	}
}

impl Console {
	/// Create a new console state given a minecraft request channel
	pub fn new (tx_req :mpsc::Sender<AttendantRequest>, rx_closed :ready::Receiver) -> Self {
		Self {
			interface: INTERFACE.as_ref().map(Arc::clone),
			attendant: ConsoleAttendant {
				tx_req,
				client_id: secretary::get_attendant_id(),
			},
			rx_closed,
		}
	}

	/// Spawn a task that processes user input asynchronously
	pub fn handle_stdio	(mut self) -> tokio::task::JoinHandle<()> {
		tokio::spawn(async move {
			match &self.interface {
				Some(interface) => self.linefeed_loop().await,
				None => {
					let mut input = BufReader::new(tokio::io::stdin()).lines();

					while let Some(line) = input.next().await {
						let line = line.expect("io or unicode error"); // FIXME
						self.attendant.send_command(line).await;
					}
				},
			}
		})
	}

	/// IO loop when we use linefeed::Interface
	async fn linefeed_loop(&mut self) {
		let interface = self.interface.as_ref().unwrap();

		loop {
			let next_line = poll_fn(|cx| {
				cx.waker().wake_by_ref();
				match interface.read_line_step(Some(INPUT_TIMEOUT)) {
					Ok(Some(v)) => Poll::Ready(Ok(v)),
					Ok(None) => Poll::Pending,
					Err(e) => Poll::Ready(Err(e)),
				}
			});

			select! {
				line = next_line => match line {
					Ok(ReadResult::Input(line)) => {
						if line.trim().is_empty() {
							return;
						}
						interface.add_history(line.clone());
						self.attendant.send_command(line).await;
					},
					Ok(ReadResult::Signal(lfSignal::Interrupt)) => {
						println!("\nuh\noh\n");
						print!("^C");
						nix::sys::signal::raise(nixSignal::SIGINT);
					},
					Err(e) => unimplemented!(),
					Ok(_) => (),
				},
				_ = &mut self.rx_closed => {
					break;
				},
			}
		}

		// Remove last prompt
		interface.cancel_read_line();
	}
}

/// Abstraction over the output struct (either linefeed::Interface or io::Stdout)
pub(crate) struct TermOutput (Option<TermInterface>);

impl TermOutput {
	/// For write!()
	pub fn write_fmt (&self, args :fmt::Arguments) -> std::io::Result<()> {
		match &self.0 {
			Some(interface) => interface.write_fmt(args),
			None => std::io::stdout().write_fmt(args),
		}
	}
}

/// Get output to use with write!()
pub(crate) fn out () -> TermOutput {
	TermOutput(INTERFACE.as_ref().map(Arc::clone))
}
