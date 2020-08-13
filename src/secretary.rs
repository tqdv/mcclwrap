// Functions that handle incoming connection and commands on the socket

/* Socket language: 

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
*/
use crate::slang::*;

use crate::ready;
use tokio::sync::oneshot;

use crate::minecraft::{
	SharedOutputFilters, SharedInputFilters,
	OutputFilter, OurInputFilter, InputFilter,
	action_once
};
use tokio::net::{UnixStream, UnixListener};
use regex::RegexBuilder;

pub(crate) struct Client {
	rx_ready :ready::Receiver,
	tx_req :mpsc::Sender<String>,
	output_filters :SharedOutputFilters,
	input_filters :SharedInputFilters,
	current_guard :Option<OurInputFilter>,
	id :u32,
}

type ClientAnswer = Result<String, String>;

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

impl Client {
	async fn process (mut self, mut stream :UnixStream) {
		let (reader, mut writer) = stream.split();
		let mut lines = BufReader::new(reader).lines();
		let mut should_break;

		while let Some(l) = lines.next().await {
			let l = l.expect("Some IO or utf error probably");
			println!("{}⬅️  {}", self.id, l);

			let reply = self.process_line(&l).await;
			should_break = reply.is_err();
			let mut reply = reply.into_inner();

			if !reply.ends_with("\n") {
				reply.push_str("\n");
			}

			print!("{}➡️  {}", self.id, reply);
			if let Err(_) = writer.write_all(reply.as_bytes()).await {
				// CHECKME stop handling client on first write error
				should_break = true;
			}

			if should_break {
				break;
			}
		}
	}

	async fn process_line(&mut self, line :&str) -> ClientAnswer {
		let (command, rest) = match line.find(&[' ', '\t'] as &[_]){
			Some(i) => (&line[..i], &line[i+1..]),
			None => (line, ""),
		};

		match command {
			"ready" => {
				match (&mut self.rx_ready).await {
					Some(_) => Ok("ok: ready".to_string()),
					None => Err("error: ready unknown".to_string()),
				}
			},
			"slash" => {
				let sending_command = (&mut self.tx_req)
					.send(rest.trim_start_matches(&[' ', '\t'] as &[_]).to_string());
				match sending_command.await {
					Ok(_) => Ok("ok: slash".to_string()),
					Err(_) => Err("error: slash server stopped handling commands".to_string()),
				}
			},
			"get-line" => self.get_line(rest).await,
			"cya" => Err("cya o/".to_string()),
			"guard" => self.guard_command(rest).await,
			// otherwise
			x => Ok(format!("error: unknown command {}", x)),
		}
	}

	async fn get_line (&mut self, rest :&str) -> ClientAnswer {
		// Parse line
		let (regex, slash) = match GET_LINE_REGEX.captures(rest) {
			Some(cap) => (cap.get(1).unwrap().as_str(), cap[2].to_string()),
			None => return Ok("error: get-line bad arguments".to_string()),
		};
		// Compile regex
		let regex = tear! { RegexBuilder::new(regex).size_limit(1 << 20).build()
			=> |_| Ok("error: get-line invalid or expensive regex".to_string())
		};

		// Prepare callback
		let (tx, rx) = oneshot::channel::<String>();
		let filter = OutputFilter {
			regex,
			action: action_once(move |msg :&str| {
				let _ = tx.send(msg.to_string()); // ignore if sender dropped
			}),
			count: Cell::new(1),
			fail: Some(Cell::new(64)),
		};

		{
			// LOCK register output parser
			let mut output_filters = self.output_filters.lock().unwrap();
			output_filters.push(filter);
		}

		// Send command
		if let Err(_) = self.tx_req.send(slash).await {
			return Err("error: get-line server stopped handling commands".to_string());
		}

		// Wait for callback
		let r :String = tear! { rx.await => |_| Err("error: get-line line not found".to_string())};

		Ok(format!("ok: get-line, {}", r))
	}

	async fn guard_command (&mut self, rest :&str) -> ClientAnswer {
		let (subcommand, rest) = match GUARD_SUBCMD_REGEX.captures(rest) {
			Some(cap) => (cap.get(1).unwrap().as_str(), cap.get(2).unwrap().as_str()),
			None => return Ok("error: guard missing subcommand".to_string()),
		};
		match subcommand {
			"prevent" => {
				// Parse arguments
				let regex = match GUARD_PREVENT_REGEX.captures(rest) {
					Some(cap) => cap.get(2).unwrap().as_str(),
					None => return Ok("error: guard prevent, bad argument".to_string()),
				};
				// Compile regex
				let regex = tear! { RegexBuilder::new(regex).size_limit(1 << 20).build()
					=> |_| Ok("error: guard prevent, invalid or expensive regex".to_string())
				};

				// Prepare input filter
				let filter = OurInputFilter(Arc::new(InputFilter{
					regex, time: crate::GUARD_TIMEOUT, client_id: self.id,
				}));

				{
					// LOCK add filter to set and keep a copy
					let mut input_filters = self.input_filters.lock().unwrap();

					self.current_guard = Some(filter.clone());
					input_filters.insert(filter);
				}

				Ok("ok: guard prevent".to_string())
			}
			"renew" => {
				// TODO reset timer
				if let None = self.current_guard {
					return Ok("error: guard renew, no guard".to_string());
				}

				{
					// LOCK update filter
					self.input_filters.lock().unwrap();


				}
				unimplemented!()
			},
			"done" => {
				// TODO remove guard
				unimplemented!()
			},
			x => Ok(format!("error: guard unknown subcommand {}", x)),
		}
	}
}

// impl Drop for Client {
// 	// TODO Drop our guard
// }

fn get_id () -> u32 {
	use std::sync::atomic::{AtomicU32, Ordering};
	
	static COUNTER :AtomicU32 = AtomicU32::new(0);
	COUNTER.fetch_add(1, Ordering::Relaxed)
}

// Entry point
pub(crate) fn listen_on_socket (
	mut listener :UnixListener, rx_ready :ready::Receiver,
	tx_req :mpsc::Sender<String>,
	output_filters :SharedOutputFilters,
	input_filters :SharedInputFilters)
	-> tokio::task::JoinHandle<()>
{
	tokio::spawn(async move {
		let mut incoming = listener.incoming();
		
		while let Some(Ok(stream)) = incoming.next().await {
			let client = Client {
				rx_ready: rx_ready.clone(),
				tx_req: tx_req.clone(),
				output_filters: output_filters.clone(),
				input_filters: input_filters.clone(),
				current_guard :None,
				id: get_id(),
			};
			tokio::spawn(client.process(stream));
			// tokio::spawn(process_client(
			// 	stream, rx_ready.clone(), tx_req.clone(), output_filters.clone()
			// ));
		}
	})
}

// async fn process_client (
// 	mut stream :UnixStream,
// 	mut rx_ready :ready::Receiver,
// 	mut tx_req :mpsc::Sender<String>,
// 	output_filters :SharedOutputFilters,
// ) {
// 	let id = get_id();
// 	let (reader, mut writer) = stream.split();
// 	let mut lines = BufReader::new(reader).lines();
// 	let mut should_break;
//
// 	while let Some(l) = lines.next().await {
// 		let l = l.expect("Some IO or utf error probably");
// 		println!("{}⬅️  {}", id, l);
//
// 		let reply = process_line(l, &mut rx_ready, &mut tx_req, &output_filters).await;
// 		should_break = reply.is_err();
// 		let mut reply = reply.into_inner();
//
// 		if !reply.ends_with("\n") {
// 			reply.push_str("\n");
// 		}
//
// 		print!("{}➡️  {}", id, reply);
// 		if let Err(_) = writer.write_all(reply.as_bytes()).await {
// 			// CHECKME stop handling client on first write error
// 			should_break = true;
// 		}
//
// 		if should_break {
// 			break;
// 		}
// 	}
// }

// async fn process_line (
// 	line :String,
// 	rx_ready :&mut ready::Receiver,
// 	tx_req :&mut mpsc::Sender<String>,
// 	output_filters :&SharedOutputFilters)
// 	-> Result<String, String>
// {
// 	let (command, rest) = match line.find(|c| c == ' ' || c == '\t'){
// 		Some(i) => (&line[..i], &line[i+1..]),
// 		None => (line.as_str(), ""),
// 	};
//
// 	match command {
// 		"ready" => {
// 			match rx_ready.await {
// 				Some(_) => Ok("ready ok".to_string()),
// 				None => Err("error: ready unknown".to_string()),
// 			}
// 		},
// 		"slash" => {
// 			match tx_req.send(rest.to_string()).await {
// 				Ok(_) => Ok("slash ok".to_string()),
// 				Err(_) => Err("error: slash server stopped handling commands".to_string()),
// 			}
// 		},
// 		"get-line" => get_line(rest, output_filters, tx_req).await,
// 		"cya" => Err("cya o/".to_string()),
// 		"guard" => guard_command(rest, tx_req).await,
// 		// otherwise
// 		x => Ok(format!("error: unknown command {}", x)),
// 	}
// }
//
// async fn get_line (
// 	rest :&str,
// 	output_filters :&SharedOutputFilters,
// 	tx_req :&mut mpsc::Sender<String>)
// 	-> Result<String, String>
// {
// 	// Parse line
// 	let (regex, slash) = match GET_LINE_REGEX.captures(rest) {
// 		Some(cap) => (cap.get(1).unwrap().as_str(), cap[2].to_string()),
// 		None => return Ok("error: get-line bad arguments".to_string()),
// 	};
// 	// Compile regex
// 	let regex = match RegexBuilder::new(regex).size_limit(1 << 20).build() {
// 		Ok(v) => v,
// 		Err(_) => return Ok("error: get-line invalid or expensive regex".to_string()),
// 	};
//
// 	// Prepare callback
// 	let (tx, rx) = oneshot::channel::<String>();
// 	let filter = OutputFilter {
// 		regex,
// 		action: action_once(move |msg :&str| {
// 			let _ = tx.send(msg.to_string()); // ignore if sender dropped
// 		}),
// 		count: Cell::new(1),
// 		fail: Some(Cell::new(64)),
// 	};
//
// 	{
// 		// LOCK register output parser
// 		let mut output_filters = output_filters.lock().unwrap();
// 		output_filters.push(filter);
// 	}
//
// 	// Send command
// 	if let Err(_) = tx_req.send(slash).await {
// 		return Err("error: get-line server stopped handling commands".to_string());
// 	}
//
// 	// Wait for callback
// 	let r :String = tear! { rx.await => |_| Err("error: get-line line not found".to_string())};
//
// 	Ok(format!("get-line ok {}", r))
// }
//
// async fn guard_command (rest :&str, tx_req :&mut mpsc::Sender<String>) -> Result<String, String> {
// 	let (subcommand, rest) = match GUARD_SUBCMD_REGEX.captures(rest) {
// 		Some(cap) => (cap.get(1).unwrap().as_str(), cap.get(2).unwrap().as_str()),
// 		None => return Ok("error: guard missing subcommand".to_string()),
// 	};
// 	match subcommand {
// 		"prevent" => {
// 			// Parse arguments
// 			let (time, regex) = match GUARD_PREVENT_REGEX.captures(rest) {
// 				Some(cap) => (cap.name("time").map(|v| v.as_str()), cap.get(2).unwrap().as_str()),
// 				None => return Ok("error: guard prevent, bad argument".to_string()),
// 			};
// 			// Compile regex
// 			let regex = match RegexBuilder::new(regex).size_limit(1 << 20).build() {
// 				Ok(v) => v,
// 				Err(_) => return Ok("error: guard prevent, invalid or expensive regex".to_string()),
// 			};
//
// 			// Prepare input filter
// 			let filter = OurInputFilter (Arc::new(InputFilter{
// 				regex,
// 				time: Duration::from_secs(30), // FIXME
// 				client_id: 0, // FIXME
// 			}));
//
// 			unimplemented!()
// 		}
// 		"renew" => {
// 			// TODO reset timer
// 			unimplemented!()
// 		},
// 		"done" => {
// 			// TODO remove guard
// 			unimplemented!()
// 		},
// 		x => Ok(format!("error: guard unknown subcommand {}", x)),
// 	}
// }
