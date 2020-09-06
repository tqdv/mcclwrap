//! Functions that handle incoming connection and commands on the socket

/*! Socket language:
```text
ready → wait for server ready
	=> ok: ready → server ready
	=> error: ready unknown → (rare) can't tell if server is ready
slash <cmd> → execute command
	=> ok: slash → command will be executed
	=> error: slash server stopped handling commands → (rare)
get-line =#<regex>#= <cmd> → run command and wait for line that matches regex
	=> ok: get-line, <result> → here's the line you wanted
	=> error: get-line bad arguments → =#<regex>#= <cmd> is somehow malformed
	=> error: get-line invalid or expensive regex
	=> error: get-line server server stopped handling commands → (rare)
cya → close this connection
	=> cya o/ → we are indeed closing
```

Client commands:

```text
ready
slash say Never mine straight down
get-line r#"^There are .*? players online"# list
cya
guard-begin r#"^save-"#
guard-begin r#^"a"# 50s
guard-renew 2
guard-end 2
filter-start r#"fuck"#
filter-start r#"tps"# ignore-chat count=10
filter-stop 5
```

Server answers:

```text
ready: ok
slash: ok
get-line: ok There are 0 of max 20 players online
cya: ok
guard-begin: ok 2
guard-renew: ok 2
guard-end: ok 2
filter-start: ok 5
filter-stop: ok 5

filter 5: ok <pashiin> fuck
guard 2: err expired
```

*/
use crate::slang::*;

use crate::ready;
use crate::filters::{OutputFilter, OurInputFilter, FilterOutput, InputFilter};
use crate::minecraft::{AttendantRequest, AttendantCommand};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::{task::Poll, task::Context, pin::Pin};
use tokio::stream::Stream;
use tokio::net::{UnixStream, UnixListener};
use tokio::sync::Semaphore;
use regex::RegexBuilder;

// === Functions ===

/// Entry point. Spawns an attendant for each incoming socket connection
pub(crate) fn listen_on_socket (
	mut listener :UnixListener, rx_ready :ready::Receiver,
	tx_req :mpsc::Sender<AttendantRequest>)
	-> tokio::task::JoinHandle<()>
{
	tokio::spawn(async move {
		let mut incoming = listener.incoming();

		while let Some(stream) = incoming.next().await {
			match stream {
				Ok(stream) => {
					let attendant = Attendant::new(rx_ready.clone(), tx_req.clone());
					println!("🎁 Client {} connected", attendant.id);
					tokio::spawn(attendant.process(stream));
				},
				Err(e) => {
					eprintln!("🎁 Socket client failed to connect: {}", e);
				}
			}

		}
	})
}

/// Returns a new client id. This _can_ overflow (after 2**32)
pub(crate) fn get_client_id () -> u32 {
	static COUNTER :AtomicU32 = AtomicU32::new(0);
	COUNTER.fetch_add(1, Ordering::Relaxed)
}

// Parsing regexes
lazy_static! {
	static ref GET_LINE_REGEX :Regex = Regex::new(r###"(?x)
		^ r\#" (.*?) "\#   # regex
		\  (.+)            # command
		$"###).unwrap();

	static ref GUARD_BEGIN_REGEX :Regex = Regex::new(r###"(?x)
		^ r\#" (.*?) "\#         # Regex eg. r#"^save-"#
		(?: \  ( [0-9]+ ) s )?   # Optional duration eg. 5s
		$"###).unwrap();

	static ref GUARD_RENEW_REGEX :Regex = Regex::new(r#"(?x)
		^ ([0-9]+)       # guard id
		\  ([0-9]+) s    # duration
		$"#).unwrap();

	static ref GUARD_END_REGEX :Regex = Regex::new(r#"(?x)
		^ ([0-9]+)       # guard id
		$"#).unwrap();

	static ref FILTER_START_REGEX :Regex = Regex::new(r###"(?x)
		^ r\#" (.*?) "\#         # Regex eg. r#"^save-"#
		(?: \
			(?: (?P<ignore-chat> ignore-chat)
			| count= (?P<count> [0-9]+)
			| fail= (?P<fail> [0-9]+)
			)
		)*
		$"###).unwrap();

	static ref FILTER_STOP_REGEX :Regex = Regex::new(r#"(?x)
		^ ([0-9]+)       # filter id
		$"#).unwrap();
}

// === Data types ===

struct AttendantAnswer {
	line :String,         // The line to send, MUST end with a newline!
	end_connection :bool, // Should we close the connection
}

impl AttendantAnswer {
	/// Create a normal answer without trailing whitespace
	pub fn new (line :String) -> Self {
		let mut line = line.trim_end_matches(util::ASCII_WS_AND_NL).to_string();
		line.push_str("\n");
		Self { line, end_connection: false }
	}
	/// Create a closing answer without trailing whitespace
	pub fn closed (line :String) -> Self {
		let mut line = line.trim_end_matches(util::ASCII_WS_AND_NL).to_string();
		line.push_str("\n");
		Self { line, end_connection: true }
	}
}

pub(crate) type AttendantId = u32;
type FilterId = u32;
type AttendantInputFilters = Arc<Mutex<HashMap<FilterId, OurInputFilter>>>;
type AttendantOutputFilters = Arc<Mutex<HashMap<FilterId, broadcast::Receiver<FilterOutput>>>>;
type AttendantRequestIdGenerator = Arc<AtomicU32>;
type NextFilterOutputFutOutput = (FilterId, Result<FilterOutput, broadcast::RecvError>);

/** An attendant that handles a single client connection

socket: rx_filter, rx_reply
ClientProcessor: rx_ready, tx_req, tx_filter (to give to minecraft handler), input_filters, id
*/
pub(crate) struct Attendant {
	rx_reply :mpsc::Receiver<AttendantAnswer>, // Processing answer

	rx_ready :ready::Receiver,               // Minecraft server is ready
	tx_reply :mpsc::Sender<AttendantAnswer>, // Send reply to io loop
	tx_req :mpsc::Sender<AttendantRequest>,  // Send a request to the minecraft handler

	/// Active filters ids. Shared internally to remove expired filters
	input_filters :AttendantInputFilters,
	/// Active output filters. Shared internally between the io loop and the processor
	output_filters :AttendantOutputFilters,

	// Redundant with shared, for convenience
	id :AttendantId,
	request_id_generator :AttendantRequestIdGenerator,
}

impl Attendant {
	/// Constructor. Pass it the server ready channel and the request sender channel
	fn new (rx_ready :ready::Receiver, tx_req :mpsc::Sender<AttendantRequest>) -> Self {
		let (tx_reply, rx_reply) = mpsc::channel(32); // FIXME constant
		Self {
			rx_reply,
			rx_ready,
			tx_reply,
			tx_req,
			input_filters: Default::default(),
			output_filters: Default::default(),
			id: get_client_id(),
			request_id_generator: Arc::new(AtomicU32::new(0)),
		}
	}

	/// Entry point when processing socket connections. Assumes tokio context
	async fn process (mut self, mut stream :UnixStream) {
		let (reader, mut writer) = stream.split();
		let mut client_commands = BufReader::new(reader).lines();
		let mut next_filter_output = self.next_filter_output();
		let processor_semaphore = Arc::new(Semaphore::new(5)); // FIXME arbitrary constant

		// Continuously process connection io
		loop {
			// Rate limited at 5 processors at a time.
			// WARN the client can't tell which reply corresponds to which command
			let next_command = processor_semaphore.clone().acquire_owned()
				.then(|permit| async { (permit, client_commands.next().await) });

			select! {
				// Handle filter output
				Some((filter_id, filter_output)) = next_filter_output.next() => {
					let (output, remove_rx) = Self::filter_reply(filter_id, filter_output);

					if remove_rx {
						self.output_filters.lock().unwrap().remove(&filter_id);
					}

					match writer.write_all(output.as_bytes()).await {
						Ok(_) => print!("{}➡️  {}", self.id, output),
						Err(e) => eprintln!("🎁 Error while write to client {}: {}", self.id, e),
					}
				},
				// Handle processor replies
				Some(answer) = self.rx_reply.recv() => {
					let reply = answer.line;


					match writer.write_all(reply.as_bytes()).await {
						Ok(_) => print!("{}➡️  {}", self.id, reply),
						Err(e) => eprintln!("🎁 Error while write to client {}: {}", self.id, e),
					}

					if answer.end_connection { break; }
				},
				// Handle client requests
				(permit, Some(command)) = next_command => {
					let command :String = twist! { command => |_| {
						eprintln!("🎁 IO or UTF-8 error while reading client input {}", self.id);
						tear::last!()
					}};

					println!("{}⬅️  {}", self.id, command);

					// Process command in another task so we don't block output filter processing
					let mut processor = self.new_processor();
					tokio::spawn(async move {
						processor.process_command(&command).await;
						drop(permit);
					});
				},
				// All channels are exhausted and/or closed
				else => break,
			}
		}
	}

	/// Creates a processor that can be spawned on a new task
	fn new_processor (&self) -> AttendantProcessor {
		AttendantProcessor {
			rx_ready: self.rx_ready.clone(),
			tx_reply: self.tx_reply.clone(),
			tx_req: self.tx_req.clone(),

			input_filters: self.input_filters.clone(),
			output_filters: self.output_filters.clone(),

			id: self.id,
			request_id_generator: self.request_id_generator.clone(),
		}
	}

	/** Returns a `impl Stream` of `(FilterId, <filter receiver output>` that never ends
	(never returns `None`. */
	fn next_filter_output (&self) -> impl Stream<Item = NextFilterOutputFutOutput> {
		// cf. futures::future::select_all;
		use broadcast::RecvError;

		struct NextFilterOutput {
			output_filters :AttendantOutputFilters,
		}

		impl Stream for NextFilterOutput {
			type Item = (FilterId, Result<FilterOutput, RecvError>);

			fn poll_next (self :Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
				let mut output_filters = tear! { self.output_filters.try_lock()
					=> |_| Poll::Pending };
				let item = output_filters.iter_mut().find_map(|(&filter_id, filter_rx)| {
					// .boxed() cf. https://users.rust-lang.org/t/the-trait-unpin-is-not-implemented-for-genfuture-error-when-using-join-all/23612/3
					match filter_rx.recv().boxed().poll_unpin(cx) {
						Poll::Pending => None,
						Poll::Ready(e) => Some((filter_id, e)),
					}
				});
				match item {
					Some(id_n_res) => Poll::Ready(Some(id_n_res)),
					None => Poll::Pending,
				}
			}
		}

		NextFilterOutput { output_filters: self.output_filters.clone() }
	}

	/// Formatted string for filter output. Also returns whether to remove the receiver or not
	fn filter_reply (filter_id :FilterId, output :Result<FilterOutput, broadcast::RecvError>)
		-> (String, bool) {
		use broadcast::RecvError;

		let mut remove_rx = false;
		let string; // Ownership of the message
		let message :&str = match output {
			Ok(FilterOutput::Line(s)) => {
				string = format!("ok line {}", s);
				&string
			},
			Err(RecvError::Lagged(count)) => {
				string = format!("error lagged {}", count);
				&string
			},
			Ok(FilterOutput::Expired) | Err(RecvError::Closed) => {
				remove_rx = true;
				"ok done"
			},
		};

		let reply = format!("filter {}: {}\n", filter_id, message);
		(reply, remove_rx)
	}
}

/// Command processor, detached from the attendant io loop
struct AttendantProcessor {
	rx_ready :ready::Receiver,
	tx_reply :mpsc::Sender<AttendantAnswer>,
	tx_req :mpsc::Sender<AttendantRequest>,

	input_filters :AttendantInputFilters,
	output_filters :AttendantOutputFilters,

	id :AttendantId,
	request_id_generator :AttendantRequestIdGenerator,
}

impl AttendantProcessor {
	/// Entry point. Handles a single command
	async fn process_command(&mut self, line :&str) {
		// Extract the first space-delimited word
		let (command, rest) = match line.find(' ') {
			Some(i) => (&line[..i], &line[i+1..]),
			None => (line, ""),
		};

		let answer = match command {
			"ready" => {
				match (&mut self.rx_ready).await {
					Some(_) => Self::reply_ok(command, ""),
					None => Self::reply_bail(command, "unknown server status"),
				}
			},
			"slash" => {
				if rest.is_empty() {
					Self::reply_err(command, "missing command")
				} else {
					let attendant_req = AttendantCommand::SendCommand(rest.to_string());
					match self.send_request(command, attendant_req).await {
						Ok(_) => Self::reply_ok(command, ""),
						Err(v) => v,
					}
				}
			},
			"get-line" => self.get_line(rest).await,

			"guard-begin"  => self.guard_begin(rest).await,
			"guard-renew"  => self.guard_renew(rest).await,
			"guard-end"    => self.guard_end(rest).await,
			"filter-start" => self.filter_start(rest).await,
			"filter-stop"  => self.filter_stop(rest).await,

			"cya" => Self::reply_bail(command, "success"),
			// otherwise
			x => Self::reply_err(x, "unknown command"),
		};

		let _ = self.tx_reply.send(answer).await; // Ignore if receiver has dropped
	}

	// {{{ Subcommands

	async fn get_line (&mut self, rest :&str) -> AttendantAnswer {
		const COMMAND :&str = "get-line";

		// Parse arguments
		let cap = tear! { Self::parse_arguments(COMMAND, &GET_LINE_REGEX, rest) };
		let regex = tear! { Self::compile_regex(COMMAND, &cap[1]) };
		let slash = cap[2].to_string();

		// Send request
		let (filter, mut rx_line) = OutputFilter::new(regex)
			.count(Some(1)).fail(Some(64))
			.build();
		let command = AttendantCommand::GetLine(slash, filter);
		tear! { self.send_request(COMMAND, command).await };

		// Wait for result
		let line /* :impl Future<Output = Option<String>> */ = rx_line.recv().map(|v| {
			v.ok() /* :Option<FilterOutput> */ .map(FilterOutput::take_line).flatten()
		});
		let line = tear! { line.await => |_| Self::reply_err(COMMAND, "line not found") };

		// Return line to client
		Self::reply_ok(COMMAND, line)
	}

	async fn guard_begin (&mut self, rest :&str) -> AttendantAnswer {
		const COMMAND :&str = "guard-begin";

		// Parse arguments
		let cap = tear! { Self::parse_arguments(COMMAND, &GUARD_BEGIN_REGEX, rest) };
		let regex = tear! { Self::compile_regex(COMMAND, &cap[1]) };
		let duration = Duration::from_secs(match cap.get(2) {
			Some(s) => tear! { s.as_str().parse() => |_| Self::reply_err(COMMAND, "invalid duration") },
			None => crate::GUARD_TIMEOUT_SECS,
		});

		// Send the request
		let (filter, rx_expired) = self.new_input_filter(regex);
		let command = AttendantCommand::StartGuard(filter.clone(), duration);
		tear! { self.send_request(COMMAND, command).await };

		// Add to active filters after sending the request
		let filter_id = self.track_input_filter(filter);

		// Spawn task that signals filter expiration to the client
		let input_filters = self.input_filters.clone();
		let mut tx_reply = self.tx_reply.clone();
		tokio::spawn(async move {
			let _ = rx_expired.await; // Ignore if filter has dropped

			// UNLOCK It expired if the client still thinks it's active
			let removed = input_filters.lock().unwrap()
				.remove(&filter_id).is_some();
			if removed {
				// Message client
				let guard_name = format!("guard {}", filter_id);
				let reply = Self::reply_err(guard_name, "expired");
				let _ = tx_reply.send(reply).await; // Ignore if receiver has dropped
			}
		});

		// Return id to client
		Self::reply_ok(COMMAND, filter_id.to_string())
	}

	async fn guard_renew (&mut self, rest :&str) -> AttendantAnswer {
		const COMMAND :&str = "guard-renew";

		// Parse arguments
		let cap = tear! { Self::parse_arguments(COMMAND, &GUARD_RENEW_REGEX, rest) };
		let raw_filter_id = &cap[1];
		let filter_id :FilterId = tear! { raw_filter_id.parse()
			=> |_| Self::reply_err(COMMAND, "invalid filter id") };
		let duration :Duration = Duration::from_secs(tear! { cap[2].parse()
			=> |_| Self::reply_err(COMMAND, "invalid duration") });

		// UNLOCK Check if the filter is still considered active
		let input_filter :Option<OurInputFilter> = self.input_filters.lock().unwrap()
			.get(&filter_id)
			.map(|f| f.clone());

		// Send request
		if let Some(filter) = input_filter {
			let command = AttendantCommand::RenewGuard(filter, duration);
			tear! { self.send_request(COMMAND, command).await }

			Self::reply_ok(COMMAND, raw_filter_id)
		} else {
			Self::reply_err(COMMAND, "no such guard")
		}
	}

	async fn guard_end (&mut self, rest :&str) -> AttendantAnswer {
		const COMMAND :&str = "guard-end";

		// Parse arguments
		let cap = tear! { Self::parse_arguments(COMMAND, &GUARD_END_REGEX, rest) };
		let raw_filter_id = &cap[1];
		let filter_id :FilterId = tear! { raw_filter_id.parse()
			=> |_| Self::reply_err(COMMAND, "invalid filter id") };

		if self.remove_input_filter(filter_id) {
			Self::reply_ok(COMMAND, raw_filter_id)
		} else {
			Self::reply_err(COMMAND, "no such guard")
		}
	}

	async fn filter_start (&mut self, rest :&str) -> AttendantAnswer {
		const COMMAND :&str = "filter-start";

		// Parse arguments
		let cap = tear! { Self::parse_arguments(COMMAND, &FILTER_START_REGEX, rest) };
		let regex = tear! { Self::compile_regex(COMMAND, &cap[1]) };
		let ignore_chat = cap.name("ignore-chat").is_some();
		let count = cap.name("count").map(|m| m.as_str().parse().ok()).flatten();
		let fail = cap.name("fail").map(|m| m.as_str().parse().ok()).flatten();

		// Send request
		let (filter, filter_rx) = OutputFilter::new(regex)
			.ignore_chat(ignore_chat).count(count).fail(fail)
			.build();
		let command = AttendantCommand::AddOutputFilter(filter);
		tear! { self.send_request(COMMAND, command).await }

		// Add filter receiver to set
		let filter_id = self.add_output_filter_rx(filter_rx);

		// Return id to client
		Self::reply_ok(COMMAND, filter_id.to_string())
	}

	async fn filter_stop (&mut self, rest :&str) -> AttendantAnswer {
		const COMMAND :&str = "filter-stop";

		// Parse arguments
		let cap = tear! { Self::parse_arguments(COMMAND, &FILTER_STOP_REGEX, rest) };
		let raw_filter_id = &cap[1];
		let filter_id :FilterId = tear! { raw_filter_id.parse()
			=> |_| Self::reply_err(COMMAND, "invalid filter id") };

		if self.remove_output_filter_rx(filter_id) {
			Self::reply_ok(COMMAND, raw_filter_id)
		} else {
			Self::reply_err(COMMAND, "no such filter")
		}
	}

	// }}}

	// {{{ Utility functions

	/// Returns the captures when regex is applied to text, or the attendant error answer
	fn parse_arguments<'a> (command :&str, regex :&Regex, text :&'a str)
		-> Result<regex::Captures<'a>, AttendantAnswer> {
		regex.captures(text).ok_or_else(|| Self::reply_err(command, "invalid arguments"))
	}

	/// Gets a new request id. This _can_ overflow (after 2**32 requests)
	fn new_request_id (&mut self) -> u32 {
		self.request_id_generator.fetch_add(1, Ordering::Relaxed)
	}

	/// Creates a new OurInputFilter and its associated filter expiry receiver
	fn new_input_filter (&self, regex :Regex) -> (OurInputFilter, ready::Receiver) {
		let (tx, rx) = ready::channel();
		let input_filter = InputFilter {
			regex, client_id: self.id, expired: tx
		};
		let our_input_filter = OurInputFilter(Arc::new(input_filter));
		(our_input_filter, rx)
	}

	/// Tries to compile regex, or return failure message
	fn compile_regex (name: &str, text :&str) -> Result<Regex, AttendantAnswer> {
		RegexBuilder::new(text).size_limit(1 << 20).build()
			.map_err(|_| Self::reply_err(name, "invalid or expensive regex"))
	}

	/// Common boilerplate for sending a request and making sure it has been processed
	async fn send_request (&mut self, name :&str, command :AttendantCommand)
		-> Result<(), AttendantAnswer> {

		// Create request
		let (tx_done, rx_done) = ready::channel();
		let request = AttendantRequest {
			done: tx_done,
			command,
			client_id: self.id,
		};

		// Send the request
		if let Err(_) = self.tx_req.send(request).await {
			return Err(Self::reply_err(name, "server stopped handling commands"));
		}

		// Wait for confirmation
		if let None = rx_done.await {
			return Err(Self::reply_err(name, "command was not handled"));
		}

		Ok(())
	}

	/// Adds an input filter into the client's set and return its id
	fn track_input_filter (&mut self, filter :OurInputFilter) -> FilterId {
		let filter_id = self.new_request_id();
		// UNLOCK
		self.input_filters.lock().unwrap().insert(filter_id, filter);
		filter_id
	}

	/// Remove an input filter and returns whether it was present before
	fn remove_input_filter (&mut self, filter_id :FilterId) -> bool {
		// The request handler will automatically drop their copy if we drop ours
		self.input_filters.lock().unwrap()
			.remove(&filter_id)
			.is_some()
	}

	/// Adds an output filter receiver to the client's set and return its id
	fn add_output_filter_rx (&mut self, filter_rx :broadcast::Receiver<FilterOutput>) -> FilterId {
		let filter_id = self.new_request_id();
		// UNLOCK
		self.output_filters.lock().unwrap().insert(filter_id, filter_rx);
		filter_id
	}

	/// Removes an output filter receiver. Returns whether it was present before
	fn remove_output_filter_rx (&mut self, filter_id :FilterId) -> bool {
		self.output_filters.lock().unwrap()
			.remove(&filter_id)
			.is_some()
	}

	/// Formatted ok client answer
	fn reply_ok (command :impl AsRef<str>, msg :impl AsRef<str>) -> AttendantAnswer {
		AttendantAnswer::new(format!("{}: ok {}", command.as_ref(), msg.as_ref()))
	}

	/// Formatted error client answer
	fn reply_err (command :impl AsRef<str>, msg :impl AsRef<str>) -> AttendantAnswer {
		AttendantAnswer::new(format!("{}: error {}", command.as_ref(), msg.as_ref()))
	}

	/// Formatted bail client answer that closes the connection
	fn reply_bail (command :impl AsRef<str>, msg :impl AsRef<str>) -> AttendantAnswer {
		AttendantAnswer::closed(format!("{}: bail {}", command.as_ref(), msg.as_ref()))
	}

	// }}}
}
