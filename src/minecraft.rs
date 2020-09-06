//! Handle minecraft process and console
use crate::slang::*;
use tokio::select;

use crate::ready;
use tokio::sync::broadcast;

use crate::filters::{OurInputFilter, OutputFilter};
use crate::expiroset::ExpiroSet;
use std::{pin::Pin, future::Future};
use std::collections::{HashMap, VecDeque};
use tokio::process::{Command, Child, ChildStdin, ChildStdout};
use tokio::task::yield_now;

use std::convert::TryInto as _;

pub(crate) mod error {
	use thiserror::Error;

	#[derive(Error, Debug)]
	pub(crate) enum MinecraftError {
		#[error("Failed to capture minecraft input")]
		NoStdin,
		#[error("Failed to capture minecraft output")]
		NoStdout,
		#[error("Failed to start minecraft")]
		SpawnFail(#[source] tokio::io::Error),
	}	
}
pub(crate) use error::MinecraftError;

/// Get the minecraft process and its filehandles
pub(crate) fn get_minecraft (mc_command :&str, mc_args :&[&str])
	-> Result<(Child, ChildStdin, ChildStdout, Pid), MinecraftError>
{
	use std::process::Stdio;

	let mut child = terror! {
		Command::new(mc_command).args(mc_args)
		.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
		.spawn()
		=> MinecraftError::SpawnFail
	};

	let stdin = terror! { child.stdin.take() => |_| MinecraftError::NoStdin };
	let stdout = terror! { child.stdout.take() => |_| MinecraftError::NoStdout };
	let pid = Pid::from_raw(child.id().try_into().unwrap());
	
	Ok((child, stdin, stdout, pid))
}

/// Spawn a task that runs the minecraft process in a task and return that handle
pub(crate) fn run_minecraft (mc :Child, mut tx_stop :mpsc::Sender<()>)
	-> tokio::task::JoinHandle<()> {
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

// === Process minecraft output and attendant requests ===

type SharedOutputFilters = Arc<Mutex<Vec<OutputFilter>>>;

// Spawn tasks to handle minecraft console output and input requests
pub(crate) fn handle_minecraft_io <I, O> (input :I, output :O)
	-> (mpsc::Sender<AttendantRequest>, ready::Receiver, ready::Receiver)
	where
		I : 'static + Send + Sync + Unpin + AsyncWrite,
		O : 'static + Send + Unpin + AsyncRead,
{
	let (tx_req, rx_req) = mpsc::channel::<AttendantRequest>(64);
	let input_filters = ExpiroSet::new();
	// Output filters are shared between req_handler and handle_minecraft_output
	let output_filters = Arc::new(Mutex::new(Vec::new()));
	let (status_checker, rx_ready, rx_closed) = ServerStatusChecker::new();

	let req_handler = RequestHandler::new(input, input_filters, output_filters.clone());

	// TODO way to request output channel

	// TODO move the following todo elsewhere
	// TODO add listeners that say:
	//   println!("🎁 Server is ready");
	//   println!("🎁 Server is closing soon");

	// Process requests and console output
	tokio::spawn(req_handler.process(rx_req));
	tokio::spawn(handle_minecraft_output(output, output_filters, status_checker));
	
	// TODO return rx_console, rx_raw_console
	(tx_req, rx_ready, rx_closed)
}

/// State for the server status checker. It sends a ready signal on the ready and closed channels.
struct ServerStatusChecker {
	is_ready :bool,
	is_closed :bool,
	tx_ready :ready::Sender,
	tx_closed :ready::Sender,
}

impl ServerStatusChecker {
	/// Creates a new status checker and returns it, along with the ready and close receivers
	fn new () -> (Self, ready::Receiver, ready::Receiver) {
		let (tx_ready, rx_ready) = ready::channel();
		let (tx_closed, rx_closed) = ready::channel();
		let checker = Self {
			is_ready: false,
			is_closed: false,
			tx_ready,
			tx_closed,
		};

		(checker, rx_ready, rx_closed)
	}

	/// Checks if the message matches a server ready or close and signal on ready channels
	fn check (&mut self, message :&str) {
		if !self.is_ready && SERVER_READY_REGEX.is_match(message) {
			self.is_ready = true;
			self.tx_ready.ready();
		} else if !self.is_closed && SERVER_CLOSED_REGEX.is_match(message) {
			self.is_closed = true;
			self.tx_closed.ready();
		}
	}
}

// === Output handling ===

lazy_static! {
	/// Matches the server ready line: "Done (.*) !"
	static ref SERVER_READY_REGEX :Regex = Regex::new(
		r"^Done \([^(]+\)!"
		).unwrap();
	/// Matches the server closing line: "Closing Server"
	static ref SERVER_CLOSED_REGEX :Regex = Regex::new(
		r"^Closing Server$"
		).unwrap();

	/// Matches unwanted escapes and prompt at the start of the console line
	static ref UNWANTED_ESCAPES_REGEX :Regex = Regex::new(r"(?x)
		^
		(?: > \.+         # Prompt
		| \r              # Prompt deleter
		| \x1B\[ K        # idem
		| \x1B\[ \?1h
		| \x1B\[ \?2004h
		| \x1B =
		) +").unwrap();

	/// Matches the minecraft timestamp to remove. $1 is the terminal color escape at the start of the line
	static ref REMOVE_TIMESTAMP_REGEX :Regex = Regex::new(r"(?x)
		^
		((?: \x1B \[ .*? m )*)  # ANSI color code
		\[ .*? \]: \            # Timestamp with trailing space
		").unwrap();

	static ref IS_CHAT_REGEX :Regex = Regex::new(r"(?x)
		^ < [a-z A-Z 0-9 _] {3,32} > # username in angle brackets
		").unwrap();
}

async fn handle_minecraft_output (
	output :impl 'static + Send + Unpin + AsyncRead,
	output_filters :SharedOutputFilters,
	mut status_checker :ServerStatusChecker)
{
	// Console output channels: message-only or raw
	let (tx_console, rx_console) = broadcast::channel::<String>(16);
	let (tx_raw_console, rx_raw_console) = broadcast::channel::<String>(16);

	println!("Handle mc output");

	let mut output = BufReader::new(output).lines();
	while let Some(line) = output.next().await {
		let line = {
			let mut l :String = line.expect("io or utf error"); /* FIXME */

			// Remove interactive prompt related characters
			if let Some(found) = UNWANTED_ESCAPES_REGEX.find(&l) {
				l = l[found.end()..].to_string();
			};
			l };

		// Print minecraft console to output
		// NB: \x1B[m resets colouring at the end of the line.
		//     Minecraft seems to always reenable the color on the next line
		println!("💻 {}\x1B[m", line);
		
		// Chop of the timestamp for tx_console (but keep the leading ansi color escape)
		let message = REMOVE_TIMESTAMP_REGEX.replace(&line, "$1");
		if message != line {
			// We actually removed the timestamp
			let message = message.into_owned(); // The (coloured) message
			let is_chat = IS_CHAT_REGEX.is_match(&message);
			// optimization: chat messages will never match a server status line
			if !is_chat { status_checker.check(&message); }

			// DEV display escapes
			// println!("{}", line.replace("\x1B", "\\e"));

			// FIXME this should be handled better
			// Reset color at the end of the line. WARN could cause problems with client matching
			mut_scope!{ message, message.now_ends_with("\x1B[m"); }

			// UNLOCK Run output filters, and remove those that are completed
			output_filters.lock().unwrap()
				.map_retain(|filter| filter.check(&message, is_chat));
			
			// ignore error which happens when there are *currently* no receivers
			let _ = tx_console.send(message.to_string());
		}

		// ignore error which happens when there are *currently* no receivers
		let _ = tx_raw_console.send(line);
	}
}

// {{{ === Attendant Requests ===

/// A command send by an attendant to the minecraft handler
pub(crate) enum AttendantCommand {
	SendCommand(String),
	AddOutputFilter(OutputFilter),
	StartGuard(OurInputFilter, Duration),
	RenewGuard(OurInputFilter, Duration),

	/// Send a command and return the lines that match in a single command to prevent queuing issues
	GetLine(String, OutputFilter),
}

impl AttendantCommand {
	/// Returns a reference to the command string, if any
	fn get_slash(&self) -> Option<&str> {
		use AttendantCommand::*;
		match self {
			SendCommand(s) => Some(s),
			GetLine(s, _) => Some(s),
			_ => None,
		}
	}
}

/// Client commands that can be queued (because of input guards)
pub(crate) struct AttendantRequest {
	/// Signal to the client request completion because requests can be blocked by input filters
	pub(crate) done :ready::Sender,
	pub(crate) command : AttendantCommand,
	/// Needed for the request to pass its own input filters. Might be useful for debugging
	pub(crate) client_id : u32,
}

// }}}


/// Handles input requests and writes to minecraft console
struct RequestHandler<I :AsyncWrite + Unpin> {
	/// Minecraft console writer
	input :I,
	/// Output filters shared with the minecraft output handler
	output_filters :SharedOutputFilters,
	/// Active input filters
	input_filters :ExpiroSet<OurInputFilter>,
	/// Stores the requests blocked by an input filter
	queued_requests :HashMap<OurInputFilter, Vec<AttendantRequest>>,
	/// Requests that have been unblocked, that should be processed first
	unblocked_requests :VecDeque<AttendantRequest>,
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
			unblocked_requests: VecDeque::new(),
		}
	}

	/// Entry point
	async fn process (mut self, mut rx_req :mpsc::Receiver<AttendantRequest>) {

		// One action per loop
		loop {
			// Process any requeued requests before any new requests
			let next_request :Pin<Box<dyn Send + Future<Output = Option<AttendantRequest>>>> =
				if !self.unblocked_requests.is_empty() {
					let filter = Some(self.unblocked_requests.pop_front().unwrap());
					Box::pin(async { filter })
				} else {
					Box::pin(rx_req.recv())
				};

			// Wrap self.input_filters.next() to ignore when the queue is currently empty
			let expired_input_filter = async {
				loop {
					if let Some(v) = self.input_filters.next().await {
						break v;
					}
					yield_now().await; // prevent infinite loop
				}
			};

			select! {
				// Check incoming requests
				request = next_request => match request {
					Some(request) => self.process_request(request).await,
					None => break, // CHECKME is this enough ?
				},
				// Check guard timeouts
				expired = expired_input_filter => match expired {
					// A guard has expired
					Ok(input_filter) => {
						// Notify client
						input_filter.signal_expiration();
						// Requeue delayed requests
						self.requeue_requests_blocked_by(&input_filter);
					},
					Err(e) => {
						// TODO add warning
						if e.is_shutdown() { break; /* Catastrophic timer failure */ }
						// FIXME Otherwise, ignore error
					},
				},
			}
		}
	}

	/// Process a single request
	async fn process_request (&mut self, request :AttendantRequest) {
		use AttendantCommand::*;

		// Check input filters first
		let request = tear! { self.check_input_filters(request) => tear::gut };

		// The request should be processed now
		match request.command {
			SendCommand(slash) => {
				self.send_command(slash).await;
			},
			AddOutputFilter(filter) => {
				// UNLOCK Add to filter to set
				self.output_filters.lock().unwrap().push(filter);
			},
			StartGuard(filter, duration) => {
				self.input_filters.insert(filter, duration);
			},
			RenewGuard(filter, duration) => {
				self.input_filters.reset(&filter, duration);
			},
			GetLine(slash, output_filter) => {
				// UNLOCK Add output_filter and send command
				self.output_filters.lock().unwrap().push(output_filter);
				self.send_command(slash).await;
			},
		}

		// Signal request completion to the client
		request.done.ready();
	}

	/** Check the request against the input filters. Returns the request if it passed them
	(otherwise, consume the request and queue it). It also refreshes the input filter set */
	fn check_input_filters(&mut self, request :AttendantRequest) -> Option<AttendantRequest> {
		// input_filters only apply to console commands
		if let Some(slash) = request.command.get_slash() {
			// The regexes match against the command without leading ascii hspace
			let slash = util::strip_leading_ascii_hspace(&slash).to_string();

			// Drop filters that aren't used anymore, ie. refreshing them
			self.remove_expired_input_filters();

			for filter in self.input_filters.iter() {
				if filter.is_match(&slash) {
					if filter.from_same_attendant(&request) {
						break; // allow command
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

	/// Send a command to minecraft, making sure the string ends with a newline
	async fn send_command(&mut self, command: String) {
		mut_scope! { command, command.now_ends_with("\n"); }

		print!("✏️  {}", command);

		if let Err(_) = self.input.write_all(command.as_bytes()).await {
			eprintln!("🎁 Failed to write to minecraft console");
		}
	}

	/// Checks that all input filters are still used by an attendant and remove inactive ones
	fn remove_expired_input_filters (&mut self) {
		// Iterate through them and keep track of those that need to be removed
		// The removal is inefficient, but it's easier to write
		let mut expired = Vec::new();
		for filter in self.input_filters.iter() {
			let strong_count = filter.strong_count();
			if strong_count == 1
				|| strong_count == 2 && self.queued_requests.contains_key(filter)
			{
				expired.push(filter.clone());
			}
		}

		for filter in expired.iter() {
			self.input_filters.remove(filter);
			self.requeue_requests_blocked_by(filter)
		}
	}

	/// Requeues all the requests blocked by a filter
	fn requeue_requests_blocked_by (&mut self, filter :&OurInputFilter) {
		if let Some(queued_requests) = self.queued_requests.remove(filter) {
			self.unblocked_requests.extend(queued_requests);
		}
	}
}
