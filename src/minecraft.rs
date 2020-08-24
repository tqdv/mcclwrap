//! Handle minecraft process and console
use crate::slang::*;
use tokio::select;

use crate::{ready, util};
use tokio::sync::broadcast;

use crate::filters::{OurInputFilter, OutputFilter, SharedOutputFilters};
use crate::expiroset::ExpiroSet;
use std::pin::Pin;
use std::future::Future;
use std::collections::HashMap;
use tokio::process::{Command, Child, ChildStdin, ChildStdout};

use std::convert::TryInto as _;

pub(crate) mod error {
	use thiserror::Error;

	#[derive(Error, Debug)]
	pub(crate) enum StartMinecraft {
		#[error("Failed to capture minecraft input")]
		Stdin,
		#[error("Failed to capture minecraft output")]
		Stdout,
		#[error("Failed to start minecraft")]
		Spawn(#[source] tokio::io::Error),
	}	
}

/// Get the minecraft process and its filehandles
pub(crate) fn get_minecraft (mc_command :&str, mc_args :&[&str])
	-> Result<(Child, ChildStdin, ChildStdout, Pid), error::StartMinecraft>
{
	use std::process::Stdio;

	let mut child = terror! {
		Command::new(mc_command)
		.args(mc_args)
		.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
		.spawn()
		=> error::StartMinecraft::Spawn
	};

	let stdin = terror! { child.stdin.take() => |_| error::StartMinecraft::Stdin };
	let stdout = terror! { child.stdout.take() => |_| error::StartMinecraft::Stdout };
	let pid = Pid::from_raw(child.id().try_into().unwrap());
	
	Ok((child, stdin, stdout, pid))
}

/// Spawn a task that runs minecraft in the background and return its handle
pub(crate) fn run_minecraft (mc :Child, mut tx_stop :mpsc::Sender<()>)
	-> tokio::task::JoinHandle<()>
{
	tokio::spawn(async move {
		// Run server in background
		if let Err(e) = mc.await {
			eprintln!("🎁 Server process unexpectedly stopped. {}", e);
		}

		// Ignore error which happens if the channel buffer is full (which is fine),
		// or if the receiver dropped (which can't be helped)
		let _ = tx_stop.try_send(());
	})
}

// Spawn tasks to handle minecraft console output and input requests
pub(crate) fn handle_minecraft_io <I, O> (input :I, output :O)
	-> (mpsc::Sender<ClientRequest>, SharedOutputFilters, ready::Receiver, ready::Receiver)
	where
		I : 'static + Send + Sync + Unpin + AsyncWrite,
		O : 'static + Send + Unpin + AsyncRead,
{
	// User commands
	let (tx_req, rx_req) = mpsc::channel::<ClientRequest>(64);

	// Input filters (for command guards)
	let input_filters = ExpiroSet::new();
	// Output filters
	let (output_filters, rx_mc_ready, rx_mc_close);
	{ // Initialize output filters with server status filters
		let mut my_output_filters = Vec::new();
		let (done_filter, rx_ready) = OutputFilter::new(Regex::new(
			r"^Done \([^(]+\)!"
		).unwrap()).build();
		let (close_filter, rx_close) = OutputFilter::new(Regex::new(
			r"^Closing Server$"
		).unwrap()).build();
		my_output_filters.push(done_filter);
		my_output_filters.push(close_filter);

		// Close output_filters
		output_filters = Arc::new(Mutex::new(my_output_filters));
		rx_mc_ready = rx_ready;
		rx_mc_close = rx_close;
	}

	let mut req_handler = RequestHandler::new(input, input_filters, output_filters.clone());
	
	// Process input requests (ie. user commands)
	tokio::spawn(async move { req_handler.process(rx_req).await });
	
	// TODO way to request output channel

	// TODO add listeners that say:
	//   println!("🎁 Server is ready");
	//   println!("🎁 Server is closing soon");

	let (tx_ready, rx_ready) = ready::channel();
	let (tx_closed, rx_closed) = ready::channel();
	
	// Process output
	tokio::spawn(handle_minecraft_output(output, output_filters.clone()));
	
	// TODO return rx_console, rx_raw_console
	(tx_req, output_filters, rx_ready, rx_closed)
}

// === Output handling ===

async fn handle_minecraft_output (
	output :impl 'static + Send + Unpin + AsyncRead,
	output_filters :SharedOutputFilters)
{
	// Matches leading terminal escapes
	let unwanted_escapes = Regex::new(r"(?x)
		^
		(?: > \.+         # Prompt
		| \r              # Prompt deleter
		| \x1B\[ K        # idem
		| \x1B\[ \?1h
		| \x1B\[ \?2004h
		| \x1B =
		) +").unwrap();
	// Matches the timestamp (and captures the leading ansi color escape)
	let remove_timestamp = Regex::new(r"(?x)
		^
		((?: \x1B \[ .*? m )*)  # ANSI color code
		\[ .*? \]: \            # Timestamp with trailing space
		").unwrap();
		
	// Console output channels: message-only or raw
	let (tx_console, rx_console) = broadcast::channel::<String>(16);
	let (tx_raw_console, rx_raw_console) = broadcast::channel::<String>(16);
	
	let mut output = BufReader::new(output).lines();
	while let Some(line) = output.next().await {
		let line = {
			let mut l :String = line.expect("io or utf error"); /* FIXME */

			// Remove interactive prompt related characters
			if let Some(found) = unwanted_escapes.find(&l) {
				l = l[found.end()..].to_string();
			};
			l };

		// Print minecraft console to output
		// NB: \x1B[m resets colouring at the end of the line.
		//     Minecraft always reenables the color on the next line
		println!("💻 {}\x1B[m", line);
		
		// Chop of the timestamp for tx_console (but keep the leading ansi color escape)
		let message = remove_timestamp.replace(&line, "$1");
		if message != line {
			// We actually removed the timestamp
			mut_scope!{ message,
				let mut message = message.into_owned(); // The (coloured) message

				// WARN this can lead to having a message ending in \x1B[m\x1B[m
				//      when Minecraft also resets the color
				message.push_str("\x1B[m");
			}

			// let message :String = {
			// 	let mut m = message.into_owned(); // The (coloured) message
			//
			// 	// WARN this can lead to having a message ending in \x1B[m\x1B[m
			// 	//      when Minecraft also resets the color
			// 	m.push_str("\x1B[m");
			// 	m };

			{ // LOCK run the output filters
				let output_filters = &mut *output_filters.lock().unwrap();
				// Run each filter one by one, and remove those that are completed
				output_filters.map_retain(|filter| run_output_filter(filter, &message));
			}
			
			// ignore error which happens when there are *currently* no receivers
			let _ = tx_console.send(message.to_string());
		}

		// ignore error which happens when there are *currently* no receivers
		let _ = tx_raw_console.send(line);
	}
}

/** Run a single output filter on a message, returns whether to keep checking it.
If there are no listeners, the filter is dropped */
fn run_output_filter (filter :&mut OutputFilter, message :&str) -> bool {
	// Check match
	if !filter.regex.is_match(&message) {
		// It didn't match, update max fail count
		if let Some(tries) = &mut filter.fail {
			// TODO add option: chat messages don't count towards failed matches

			*tries -= 1;
			if *tries < 0 {
				// No more tries, removing it
				return false;
			}
		}
		return true;
	};
	
	// Send line to client
	if let Err(_) = filter.chan.send(message.to_string()) {
		// There are currently no receivers (which means that the client has stopped listening)
		return false;
	}
	
	// Update match count
	if let Some(c) = &mut filter.count {
		*c -= 1;
		*c != 0 // Remove if the client doesn't expect more lines
	} else {
		true
	}
}

// {{{ === Client Requests ===

pub(crate) enum ConsoleCommandKind {
	Slash,
	GetLine(OutputFilter),
}

pub(crate) enum ClientCommand {
	// Separate variant to access the command string generically
	ConsoleCommand{
		slash :String,
		kind :ConsoleCommandKind,
	},
	StartGuard(OurInputFilter),
	RenewGuard(OurInputFilter),
	RemoveGuard(OurInputFilter),
}

/// Client commands that can be queued (because of input guards)
pub(crate) struct ClientRequest {
	pub(crate) done :ready::Sender,
	pub(crate) command :ClientCommand,
	pub(crate) client_id : u32, // For input filters
}

// }}}


// Handles input requests and writes to minecraft console
struct RequestHandler<I :AsyncWrite + Unpin> {
	input :I,
	output_filters :SharedOutputFilters,
	input_filters :ExpiroSet<OurInputFilter>,
	/// Stores the requests blocked by an input filter
	queued_requests :HashMap<OurInputFilter, Vec<ClientRequest>>
}

/* Send+Sync makes the async work somehow */
impl<I :Unpin + AsyncWrite + Send + Sync> RequestHandler<I> {
	fn new (input :I, input_filters :ExpiroSet<OurInputFilter>, output_filters :SharedOutputFilters)
	-> RequestHandler<I> {
		RequestHandler {
			input,
			input_filters,
			output_filters,
			queued_requests: HashMap::new(),
		}
	}

	/// Entry point
	async fn process (&mut self, mut rx_req :mpsc::Receiver<ClientRequest>) {
		loop {
			select! {
				// Check incoming requests
				request = rx_req.recv() => match request {
					Some(request) => self.process_request(request).await,
					None => break, // CHECKME is this enough ?
				},
				// Check guard timeouts
				expired = self.input_filters.next() => match expired {
					// A guard has expired, process the delayed requests
					Some(Ok(expired)) => self.retry_requests_blocked_by(&expired).await,
					Some(Err(e)) => {
						if e.is_shutdown() { break; /* Catastrophic timer failure */ }
						// Otherwise, ignore error
					},
					None => (), // No items in queue, do nothing
				},
			}
		}
	}

	/// Process a single request
	async fn process_request (&mut self, request :ClientRequest) {
		use ClientCommand::*;
		use ConsoleCommandKind::*;

		// Check input filters first
		let request = tear! { self.check_input_filters(request) => tear::gut };

		// The request should be processed now
		match request.command {
			ConsoleCommand { slash, kind: commandkind } => {
				match commandkind {
					Slash => {
						// Write command to console
						self.send_command(slash).await;
					},
					GetLine(filter) => {
						{ // LOCK Enable output filter
							let mut output_filters = self.output_filters.lock().unwrap();
							output_filters.push(filter);
						}

						// Write command to console
						self.send_command(slash).await;
					},
				}
			}
			StartGuard(filter) => {
				self.input_filters.insert(filter, crate::GUARD_TIMEOUT);
			},
			RenewGuard(filter) => {
				self.input_filters.reset(&filter, crate::GUARD_TIMEOUT);
			},
			RemoveGuard(filter) => {
				self.input_filters.remove(&filter);
				self.retry_requests_blocked_by(&filter).await;
			},
			_ => unimplemented!(),
		}

		// Signal request completion to the client
		request.done.ready();
	}

	/** Check the request against the input filters. Returns the request if it passed them
	(otherwise, consume the request and queue it) */
	fn check_input_filters(&mut self, request: ClientRequest) -> Option<ClientRequest> {
		// input_filters only apply on console commands
		if let ClientCommand::ConsoleCommand { slash, .. } = &request.command {
			// match the regexes against the command without leading ascii hspace
			let slash = util::strip_leading_ascii_hspace(&slash).to_string();

			for filter in self.input_filters.iter() {
				if filter.regex.is_match(&slash) {
					// Allow commands from the same client as the filter
					if filter.client_id == request.client_id {
						break;
					}

					// Blocked, queue the request
					let queue = self.queued_requests.entry(filter.clone()).or_default();
					queue.push(request);
					return None; // Done processing
				}
			}
		}
		Some(request) // Give it back
	}

	/// Send a command, making sure the string ends with a newline
	async fn send_command(&mut self, command: String) {
		mut_scope! { command,
			if !command.ends_with("\n") { command.push_str("\n"); }
		}

		// DEBUG
		print!("✏️  {}", command);

		if let Err(_) = self.input.write_all(command.as_bytes()).await {
			eprintln!("🎁 Failed to write to minecraft console");
		}
	}

	/// Retries all the requests blocked by a filter
	fn retry_requests_blocked_by<'a> (&'a mut self, filter :&'a OurInputFilter)
	-> Pin<Box<dyn 'a + Send + Future<Output = ()>>> {
		// let myself = Pin::new(self); // FIXME I had to use Pin<&'a mut Self>, why is it not complaining now ?
		// let myself = self;
		Box::pin(async move {
			if let Some(queued_requests) = self.queued_requests.remove(filter) {
				for r in queued_requests {
					self.process_request(r).await
				}
			}
		})
	}
}
