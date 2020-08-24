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
*/
use crate::slang::*;

use crate::ready;
use crate::filters::{self, OutputFilter, OurInputFilter, FilterOutput};
use crate::minecraft::{ClientRequest, ClientCommand, ConsoleCommandKind};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::net::{UnixStream, UnixListener};
use regex::RegexBuilder;

// === Functions ===

//noinspection ALL,Annotator
/// Entry point for socket listening
pub(crate) fn listen_on_socket (
	mut listener :UnixListener, rx_ready :ready::Receiver,
	tx_req :mpsc::Sender<ClientRequest>)
	-> tokio::task::JoinHandle<()>
{
	tokio::spawn(async move {
		let mut incoming = listener.incoming();

		while let Some(Ok(stream)) = incoming.next().await {
			let client = Client::new(rx_ready, tx_req);
			tokio::spawn(client.process(stream));
		}
	})
}

/// Returns a new client id. This _can_ overflow (after 2**32)
pub(crate) /* FIXME */ fn get_client_id () -> u32 {
	static COUNTER :AtomicU32 = AtomicU32::new(0);
	// Starts at 1 because 0 is reserved for internal clients
	COUNTER.fetch_add(1, Ordering::Relaxed)
}

// Parsing regexes
lazy_static! {
	// eg. get-line /There are .* players online:/ list
	static ref GET_LINE_REGEX :Regex = Regex::new(r"(?x)
		^ [\ \t]*
		=\# (.*?) \#=  # regex
		[\ \t]+
		(.*) $").unwrap();
	static ref GUARD_SUBCMD_REGEX :Regex = Regex::new(r"(?x)
		^ [\ \t]*
		([a-zA-Z-]+)  # subcommand
		[\ \t]+
		(.*) $").unwrap();
	static ref GUARD_PREVENT_REGEX :Regex = Regex::new(r"(?x)
		^ [\ \t]*
		=\# (.*?) \#=  # regex
		[\ \t]* $").unwrap();
}

// === Data types ===

pub(crate) struct Client {
	// Server channels
	rx_ready :ready::Receiver, // Minecraft server is ready
	tx_req :mpsc::Sender<ClientRequest>, // Send a request to the minecraft handler

	// Filter output
	rx_filter :mpsc::Receiver<FilterOutput>,

	// Client state
	id :u32,
	request_id_generator :AtomicU32,
}

/// Err means dropping the connection because of unrecoverable failures cf. Client::reply_*
type ClientAnswer = Result<String, String>;

impl Client {
	/// Constructor. Pass it the server ready channel and the request sender channel
	fn new (rx_ready :ready::Receiver, tx_req :mpsc::Sender<ClientRequest>) -> Self {
		Self {
			rx_ready,
			tx_req,
			id: get_client_id(),
			request_id_generator: AtomicU32::new(0),
		}
	}

	// {{{ Process a stream
	// NB: we are in tokio context so tokio::spawn is valid

	/// Entry point when processing socket connections
	async fn process (mut self, mut stream :UnixStream) {
		let (reader, mut writer) = stream.split();
		let mut lines = BufReader::new(reader).lines();
		let mut close_connection = false;

		// Listen to filter output on another task
		tokio::spawn(async move {
			while Some(m) = self.rx_filter.recv() {
				unimplemented!();
			}
		});

		while let Some(l) = lines.next().await {
			let l = l.expect("Some IO or utf error probably"); // FIXME
			println!("{}⬅️  {}", self.id, l);

			let reply = self.process_command(&l).await;
			if reply.is_err() { close_connection = true; }

			let reply = { // Get string with newline
				let mut s = reply.into_inner();
				s.now_ends_with("\n");
				s
			};

			print!("{}➡️  {}", self.id, reply);

			let write_result = writer.write_all(reply.as_bytes()).await;
			// CHECK stop handling client on first write error
			if write_result.is_err() { close_connection = true; }

			if close_connection { break; }
		}
	}

	/// Handles a single command
	async fn process_command(&mut self, line :&str) -> ClientAnswer {
		// Extract the first whitespace-delimited word
		let (command, rest) = match line.find(util::ASCII_WS){
			Some(i) => (&line[..i], &line[i+1..]),
			None => (line, ""),
		};

		match command {
			"ready" => {
				match (&mut self.rx_ready).await {
					Some(_) => Self::reply_ok(command, ":)"),
					None => Self::reply_bail(command, "unknown"),
				}
			},
			"slash" => {
				// Send request
				let slash = util::strip_leading_ascii_hspace(rest).to_string();
				tear! { self.send_request(command,
					ClientCommand::ConsoleCommand{ slash, kind: ConsoleCommandKind::Slash }
				).await };

				// slash command has been run
				Self::reply_ok(command, ":)")
			},
			"get-line" => self.get_line(rest).await,
			"guard" => self.guard_command(rest).await,
			"cya" => Err("ok: cya, o/".to_string()),
			// otherwise
			x => Self::reply_err(x, "unknown command"),
		}
	}

	async fn get_line (&mut self, rest :&str) -> ClientAnswer {
		const COMMAND:&str = "get-line";

		// Parse line
		let (regex, slash) = match GET_LINE_REGEX.captures(rest) {
			Some(cap) => (cap.get(1).unwrap().as_str(), cap[2].to_string()),
			None => return Self::reply_err(COMMAND, "bad arguments"),
		};
		// Compile regex
		let regex = tear! { RegexBuilder::new(regex).size_limit(1 << 20).build()
			=> |_| Self::reply_err(COMMAND, "invalid or expensive regex") };

		// Prepare filter
		let (filter, mut rx_line) = OutputFilter::new(regex)
			.count(Some(1))
			.fail(Some(64))
			.build();
		// TODO channel custom capacity

		// Send request
		tear! { self.send_request(COMMAND,
			ClientCommand::ConsoleCommand { slash, kind: ConsoleCommandKind::GetLine(filter) }
		).await };

		// Wait for result
		let r = tear! { rx_line.recv().await => |_| Self::reply_err(COMMAND, "line not found") };

		Self::reply_ok(COMMAND, &r)
	}

	async fn guard_command (&mut self, rest :&str) -> ClientAnswer {
		let (subcommand, rest) = match GUARD_SUBCMD_REGEX.captures(rest) {
			Some(cap) => (cap.get(1).unwrap().as_str(), cap.get(2).unwrap().as_str()),
			None => return Self::reply_err("guard", "missing subcommand"),
		};
		match subcommand {
			"prevent" => {
				const COMMAND :&str = "guard prevent";

				// Parse arguments
				let regex = match GUARD_PREVENT_REGEX.captures(rest) {
					Some(cap) => cap.get(2).unwrap().as_str(),
					None => return Self::reply_ok(COMMAND, "bad argument"),
				};
				// Compile regex
				let regex = tear! { RegexBuilder::new(regex).size_limit(1 << 20).build()
					=> |_| Self::reply_ok(COMMAND, "invalid or expensive regex") };

				// Prepare input filter
				let filter = OurInputFilter(Arc::new(filters::InputFilter{
					regex, client_id: self.id,
				}));
				let filter_id = Self::new_request_id();

				// Create request
				let (request, rx_done) = self.new_request(ClientCommand::StartGuard(filter));

				// Insert it first into the active requests
				self.input_filters.insert(); // FIXME

				// Send the request
				if let Err(_) = self.tx_req.send(request).await {
					return Self::reply_err(COMMAND, "server stopped handling commands");
				}

				// Wait for confirmation
				if let None = rx_done.await {
					return Self::reply_err(COMMAND, "command was not handled");
				}

				unimplemented!();



				Self::reply_ok(COMMAND, filter_id.to_string)
			}
			"renew" => {
				const COMMAND :&str = "guard renew";

				if let Some(Some(filter)) = self.current_guard.as_ref().map(|v| std::sync::Weak::upgrade(v)) {
					tear! { self.send_request(COMMAND,
						ClientCommand::RenewGuard(OurInputFilter(filter))
					).await }

					Self::reply_ok(COMMAND, "done")
				} else {
					Self::reply_err(COMMAND, "no such guard")
				}
			},
			"done" => {
				// TODO remove guard
				if let None = self.current_guard {
					return Ok("error: guard done, no active guard".to_string());
				}

				unimplemented!()
			},
			x => Ok(format!("error: guard unknown subcommand {}", x)),
		}
	}

	// }}}

	// {{{ Utility functions

	/// Gets a new request id. This _can_ overflow (after 2**32 requests)
	fn new_request_id (&mut self) -> u32 {
		self.request_id_generator.fetch_add(1, Ordering::Relaxed)
	}

	/// Given a command, creates a new request and its ready receiver
	fn new_request (&self, command :ClientCommand) -> (ClientRequest, ready::Receiver) {
		let (tx, rx) = ready::channel();
		let request = ClientRequest {
			done: tx,
			command,
			client_id: self.id,
		};
		(request, rx)
	}

	/// Common boilerplate for sending a request and making sure it has been processed
	async fn send_request (&mut self, name :&str, command :ClientCommand)
	-> tear::ValRet<(), ClientAnswer>
	{
		let (request, rx_done) = self.new_request(command);

		// Send the request
		if let Err(_) = self.tx_req.send(request).await {
			ret!{ Self::reply_err(name, "server stopped handling commands") };
		}

		// Wait for confirmation
		if let None = rx_done.await {
			ret!{ Self::reply_err(name, "command was not handled") };
		}

		Val(())
	}

	/// Formatted ok client answer
	fn reply_ok (command :&str, msg :&str) -> ClientAnswer {
		Ok(format!("ok: {}, {}", command, msg))
	}

	/// Formatted err client answer
	fn reply_err (command :&str, msg :&str) -> ClientAnswer {
		Ok(format!("error: {}, {}", command, msg))
	}

	/// Formatted bail client answer (drops the client)
	fn reply_bail (command :&str, msg :&str) -> ClientAnswer {
		Err(format!("bail: {}, {}", command, msg))
	}

	// }}}
}
