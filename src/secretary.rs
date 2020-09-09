//! Functions that handle incoming connection and commands on the socket

/*!

# Synopsis
```
let rx_ready = unimplemented!(); // Server ready channel
let tx_req = unimplemented!();   // Minecraft request sender
secretary::listen_on_socket(socket, rx_ready, tx_req);
```

# Socket language

Client commands look like this:
```raku
rx {
	^
	$<header> = [
		[ "[" <[0..9]> ** 1..9 "] " ]?       # Optional command id
		$<command> = [ <-[ \  \[ ]> + ]  # Command
	]
	[ " " $<args> = (.*) ]?           # Command arguments
	$
}
```

And replies look like this:
```text
rx {
	^
	$<header> = [
		[ "[" <[0..9]> + "] " ]?      # Optional command id
		$<command> = [ <-[ \  ]> + ]  # Command
	]
	": " [ ok | error | bail ]        # Command status
	[ " " $<message> = [.*] ]?        # Optional command message
	$
}
```


FIXME these are out of date
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

/// Returns a new attendant id. This _can_ overflow (after 2**32)
pub(crate) fn get_attendant_id() -> u32 {
	static COUNTER :AtomicU32 = AtomicU32::new(0);
	COUNTER.fetch_add(1, Ordering::Relaxed)
}

// Parsing regexes
lazy_static! {
	/// lazy_static regex for command components
	static ref GET_HEADER_REGEX :Regex = Regex::new(r"(?x)
		^ (
			(?: \[ [0-9]{1,9} \] \  )?  # Command id eg. [5]
			([^ \  \[ ])                # Command
		)                               # = reply header
		(?: \  (.*) )?                  # Command arguments
		$").unwrap();

	/// lazy_static regex for get-line command
	static ref GET_LINE_REGEX :Regex = Regex::new(r###"(?x)
		^ r\#" (.*?) "\#   # regex
		\  (.+)            # command
		$"###).unwrap();

	/// lazy_static regex for guard-begin command
	static ref GUARD_BEGIN_REGEX :Regex = Regex::new(r###"(?x)
		^ r\#" (.*?) "\#         # Regex eg. r#"^save-"#
		(?: \  ( [0-9]+ ) s )?   # Optional duration eg. 5s
		$"###).unwrap();

	/// lazy_static regex for guard-renew command
	static ref GUARD_RENEW_REGEX :Regex = Regex::new(r#"(?x)
		^ ([0-9]+)       # guard id
		\  ([0-9]+) s    # duration
		$"#).unwrap();

	/// lazy_static regex for guard-end command
	static ref GUARD_END_REGEX :Regex = Regex::new(r#"(?x)
		^ ([0-9]+)       # guard id
		$"#).unwrap();

	/// lazy_static regex for filter-start command
	static ref FILTER_START_REGEX :Regex = Regex::new(r###"(?x)
		^ r\#" (.*?) "\#         # Regex eg. r#"^save-"#
		(?: \
			(?: (?P<ignore-chat> ignore-chat)
			| count= (?P<count> [0-9]+)
			| fail= (?P<fail> [0-9]+)
			)
		)*
		$"###).unwrap();

	/// lazy_static regex for filter-stop command
	static ref FILTER_STOP_REGEX :Regex = Regex::new(r#"(?x)
		^ ([0-9]+)       # filter id
		$"#).unwrap();
}

// === Data types ===

/// What the attendant sends to the client
struct AttendantAnswer {
	/** The line to send, It MUST end with a newline!

	Use Self::new and Self::closed to create correct instances */
	line :String,
	/// If we should close the connection
	end_connection :bool,
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

/// Identifier used to keep track of attendants
pub(crate) type AttendantId = u32;
/// Identifier used to keep track of both input and output filters
type FilterId = u32;
type AttendantInputFilters = Arc<Mutex<HashMap<FilterId, OurInputFilter>>>;
type AttendantOutputFilters = Arc<Mutex<HashMap<FilterId, broadcast::Receiver<FilterOutput>>>>;
type AttendantRequestIdGenerator = Arc<AtomicU32>;
/// (dev) Item of impl Stream for Attendant::NextFilterOutput
type NextFilterOutputItem = (FilterId, Result<FilterOutput, broadcast::RecvError>);

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
			id: get_attendant_id(),
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
	fn next_filter_output (&self) -> impl Stream<Item = NextFilterOutputItem> {
		// cf. futures::future::select_all;
		use broadcast::RecvError;

		struct NextFilterOutput {
			output_filters :AttendantOutputFilters,
		}

		impl Stream for NextFilterOutput {
			type Item = (FilterId, Result<FilterOutput, RecvError>); // = NextFilterOutputItem

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
		// TODO test filter-start arguments

		let answer :AttendantAnswer = match GET_HEADER_REGEX.captures(line) {
			Some(cap) => {
				let command = &cap[2];
				let header = &cap[1];
				let args = cap.get(3).map(|v| v.as_str()).unwrap_or("");
				match command {
					"ready" => {
						match (&mut self.rx_ready).await {
							Some(_) => Self::reply_ok(header, ""),
							None => Self::reply_bail(header, "unknown server status"),
						}
					},
					"slash" => {
						if args.is_empty() {
							Self::reply_err(header, "missing command")
						} else {
							let attendant_req = AttendantCommand::SendCommand(args.to_string());
							match self.send_request(header, attendant_req).await {
								Ok(_) => Self::reply_ok(header, ""),
								Err(v) => v,
							}
						}
					},
					"get-line" => self.get_line(header, args).await,

					"guard-begin"  => self.guard_begin(header, args).await,
					"guard-renew"  => self.guard_renew(header, args).await,
					"guard-end"    => self.guard_end(header, args).await,
					"filter-start" => self.filter_start(header, args).await,
					"filter-stop"  => self.filter_stop(header, args).await,

					"cya" => Self::reply_bail(header, "success"),
					// otherwise
					x => Self::reply_err(x, "unknown command"),
				}
			},
			None => Self::reply_err("", "invalid request")
		};

		let _ = self.tx_reply.send(answer).await; // Ignore if receiver has dropped
	}

	// {{{ Subcommands

	async fn get_line (&mut self, header :&str, rest :&str) -> AttendantAnswer {
		// Parse arguments
		let cap = tear! { Self::parse_arguments(header, &GET_LINE_REGEX, rest) };
		let regex = tear! { Self::compile_regex(header, &cap[1]) };
		let slash = cap[2].to_string();

		// Send request
		let (filter, mut rx_line) = OutputFilter::new(regex)
			.count(Some(1)).fail(Some(64))
			.build();
		let command = AttendantCommand::GetLine(slash, filter);
		tear! { self.send_request(header, command).await };

		// Wait for result
		let line /* :impl Future<Output = Option<String>> */ = rx_line.recv().map(|v| {
			v.ok() /* :Option<FilterOutput> */ .and_then(FilterOutput::take_line)
		});
		let line = tear! { line.await => |_| Self::reply_err(header, "line not found") };

		// Return line to client
		Self::reply_ok(header, line)
	}

	async fn guard_begin (&mut self, header :&str, rest :&str) -> AttendantAnswer {
		// Parse arguments
		let cap = tear! { Self::parse_arguments(header, &GUARD_BEGIN_REGEX, rest) };
		let regex = tear! { Self::compile_regex(header, &cap[1]) };
		let duration = Duration::from_secs(match cap.get(2) {
			Some(s) => tear! { s.as_str().parse()
				=> |_| Self::reply_err(header, "invalid duration") },
			None => crate::GUARD_TIMEOUT_SECS,
		});

		// Send the request
		let (filter, rx_expired) = self.new_input_filter(regex);
		let command = AttendantCommand::StartGuard(filter.clone(), duration);
		tear! { self.send_request(header, command).await };

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
		Self::reply_ok(header, filter_id.to_string())
	}

	async fn guard_renew (&mut self, header :&str, rest :&str) -> AttendantAnswer {
		// Parse arguments
		let cap = tear! { Self::parse_arguments(header, &GUARD_RENEW_REGEX, rest) };
		let raw_filter_id = &cap[1];
		let filter_id :FilterId = tear! { raw_filter_id.parse()
			=> |_| Self::reply_err(header, "invalid filter id") };
		let duration :Duration = Duration::from_secs(tear! { cap[2].parse()
			=> |_| Self::reply_err(header, "invalid duration") });

		// UNLOCK Check if the filter is still considered active
		let input_filter :Option<OurInputFilter> = self.input_filters.lock().unwrap()
			.get(&filter_id)
			.map(|f| f.clone());

		// Send request
		if let Some(filter) = input_filter {
			let command = AttendantCommand::RenewGuard(filter, duration);
			tear! { self.send_request(header, command).await }

			Self::reply_ok(header, raw_filter_id)
		} else {
			Self::reply_err(header, "no such guard")
		}
	}

	async fn guard_end (&mut self, header :&str, rest :&str) -> AttendantAnswer {
		// Parse arguments
		let cap = tear! { Self::parse_arguments(header, &GUARD_END_REGEX, rest) };
		let raw_filter_id = &cap[1];
		let filter_id :FilterId = tear! { raw_filter_id.parse()
			=> |_| Self::reply_err(header, "invalid filter id") };

		if self.remove_input_filter(filter_id) {
			Self::reply_ok(header, raw_filter_id)
		} else {
			Self::reply_err(header, "no such guard")
		}
	}

	async fn filter_start (&mut self, header :&str, rest :&str) -> AttendantAnswer {
		// Parse arguments
		let cap = tear! { Self::parse_arguments(header, &FILTER_START_REGEX, rest) };
		let regex = tear! { Self::compile_regex(header, &cap[1]) };
		let ignore_chat = cap.name("ignore-chat").is_some();
		let count = cap.name("count").and_then(|m| m.as_str().parse().ok());
		let fail = cap.name("fail").and_then(|m| m.as_str().parse().ok());

		// Send request
		let (filter, filter_rx) = OutputFilter::new(regex)
			.ignore_chat(ignore_chat).count(count).fail(fail)
			.build();
		let command = AttendantCommand::AddOutputFilter(filter);
		tear! { self.send_request(header, command).await }

		// Add filter receiver to set
		let filter_id = self.add_output_filter_rx(filter_rx);

		// Return id to client
		Self::reply_ok(header, filter_id.to_string())
	}

	async fn filter_stop (&mut self, header :&str, rest :&str) -> AttendantAnswer {
		// Parse arguments
		let cap = tear! { Self::parse_arguments(header, &FILTER_STOP_REGEX, rest) };
		let raw_filter_id = &cap[1];
		let filter_id :FilterId = tear! { raw_filter_id.parse()
			=> |_| Self::reply_err(header, "invalid filter id") };

		if self.remove_output_filter_rx(filter_id) {
			Self::reply_ok(header, raw_filter_id)
		} else {
			Self::reply_err(header, "no such filter")
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
