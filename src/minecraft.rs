//! Handle minecraft process and console
use crate::slang::*;

use crate::ready;
use tokio::sync::broadcast;

use std::collections::HashSet;
use tokio::process::{Command, Child, ChildStdin, ChildStdout};
use tokio::time::Duration;

use std::convert::TryInto as _;

pub(crate) enum FilterCallback {
	Once(Box<dyn FnOnce(&str) -> () + Send>),
	Multiple(Box<dyn FnMut(&str) -> () + Send>), // TESTME
}

pub(crate) struct OutputFilter {
	pub(crate) regex :Regex,
	// Cell<Option<T>> lets us take the value out of a borrow
	pub(crate) action :Cell<Option<FilterCallback>>,
	// How many lines match the regex
	pub(crate) count :Cell<i32>,
	// How many lines are allowed not to match
	pub(crate) fail :Option<Cell<i32>>,
}

#[derive(Debug)]
pub(crate) struct InputFilter {
	pub(crate) regex :Regex,
	pub(crate) time :Duration,
	pub(crate) client_id :u32,
}

#[derive(Debug, Clone)]
pub(crate) struct OurInputFilter (pub(crate) Arc<InputFilter>);

// Wraps the function into the right type for OutputFilter.action that will be called once (FilterCallback::Once)
pub(crate) fn action_once (f :impl FnOnce(&str) + Send + 'static)
	-> Cell<Option<FilterCallback>>
{
	Cell::new(Some(FilterCallback::Once(Box::new(f))))
}

// TODO defined fn action_multiple if needed 

pub(crate) type SharedOutputFilters = Arc<Mutex<Vec<OutputFilter>>>;
pub(crate) type SharedInputFilters = Arc<Mutex<HashSet<OurInputFilter>>>;

// === Implementing traits for {Input,Output}Filter and FilterCallback ===

impl PartialEq for OurInputFilter {
	fn eq(&self, other :&Self) -> bool {
		Arc::ptr_eq(&self.0, &other.0) }}

impl Eq for OurInputFilter {}

impl std::hash::Hash for OurInputFilter {
	fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
		(&*self.0 as *const InputFilter).hash(state) }}

impl std::fmt::Debug for FilterCallback {
	fn fmt(&self, f :&mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			FilterCallback::Once(_) =>
				f.write_str("FilterCallback::Once(<anon>)"),
			FilterCallback::Multiple(_) =>
				f.write_str("FilterCallback::Multiple(<anon>)"), }}}

/// A struct whose debug output is the string it wraps
struct DebugStr<'a> (&'a str);
impl std::fmt::Debug for DebugStr<'_> {
	fn fmt(&self, f :&mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(self.0) }}

impl std::fmt::Debug for OutputFilter {
	fn fmt(&self, f :&mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let callback = self.action.take();
		let action;
		match callback {
			Some(callback) => {
				action = format!("Cell::new(Some({:?}))", callback);
				self.action.set(Some(callback));
			},
			None => action = "Cell::new(None)".to_string(),
		}

		f.debug_struct("OutputFilter")
			.field("regex", &self.regex)
			.field("action", &DebugStr(&action))
			.field("count", &self.count)
			.field("fail", &self.fail)
		 	.finish() }}

// ___ Implementing Debug for OutputFilter and FilterCallback ___

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

pub(crate) fn get_minecraft (mc_command :&str, mc_args :&[&str])
	-> Result<(Child, ChildStdin, ChildStdout, Pid), error::StartMinecraft>
{
	use std::process::Stdio;

	let mut child = terror! {
		Command::new(mc_command)
		.args(mc_args)
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.stdin(Stdio::piped())
		.spawn()
		=> error::StartMinecraft::Spawn
	};

	let stdin = terror! { child.stdin.take() => |_| error::StartMinecraft::Stdin };
	let stdout = terror! { child.stdout.take() => |_| error::StartMinecraft::Stdout };
	let pid = Pid::from_raw(child.id().try_into().unwrap());
	
	Ok((child, stdin, stdout, pid))
}

// Spawn a task that runs minecraft in the background
pub(crate) fn run_minecraft (mc :Child, mut tx_stop :mpsc::Sender<()>)
	-> tokio::task::JoinHandle<()>
{
	tokio::spawn(async move {
		if let Err(e) = mc.await {
			eprintln!("🎁 Server process unexpectedly stopped. {}", e);
		}
		// Errors if buffer is full (which is fine), or dropped (which can't be helped)
		let _ = tx_stop.try_send(());
	})
}

pub(crate) fn handle_minecraft_io <I, O> (input :I, output :O)
	-> (mpsc::Sender<String>, SharedOutputFilters, SharedInputFilters, ready::Receiver, ready::Receiver)
	where
		I : 'static + Send + Unpin + AsyncWrite,
		O : 'static + Send + Unpin + AsyncRead,
{
	// User commands
	let (tx_req, rx_req) = mpsc::channel::<String>(64);
	
	// Input filters (for command guards)
	let input_filters = Arc::new(Mutex::new(HashSet::<OurInputFilter>::new()));
	
	// Process user commands
	tokio::spawn(handle_minecraft_input(input, rx_req, input_filters.clone()));
	
	// TODO way to request output channel
	
	// Output filters
	let output_filters = Arc::new(Mutex::new(Vec::<OutputFilter>::new()));
	
	// Server status: ready, stopped
	let (tx_ready, rx_ready) = ready::channel();
	let (tx_closed, rx_closed) = ready::channel();
	{
		// LOCK add the start and stop filters
		let mut output_filters = output_filters.lock().unwrap();
		output_filters.push(OutputFilter {
			regex: Regex::new(r"^Done \([^(]+\)!").unwrap(),
			action: action_once(move |_ :&str| {
				println!("🎁 Server is ready");
				tx_ready.ready();
			}),
			count: Cell::new(1),
			fail: None,
		});
		output_filters.push(OutputFilter {
			regex: Regex::new("^Closing Server$").unwrap(),
			action: action_once(move |_ :&str| {
				println!("🎁 Server is closing soon");
				tx_closed.ready();
			}),
			count: Cell::new(1),
			fail: None,
		});
	}
	
	// Process output
	tokio::spawn(handle_minecraft_output(output, output_filters.clone()));
	
	// TODO return rx_console, rx_raw_console
	(tx_req, output_filters, input_filters, rx_ready, rx_closed)
}

async fn handle_minecraft_output (
	output :impl 'static + Send + Unpin + AsyncRead,
	output_filters :SharedOutputFilters)
{
	let unwanted_escapes = Regex::new(r"(?x)
		^
		(?: > \.+         # Prompt
		| \r              # Prompt deleter
		| \x1B\[ K        # idem
		| \x1B\[ \?1h
		| \x1B\[ \?2004h
		| \x1B =
		) +").unwrap();
	let remove_timestamp = Regex::new(r"(?x)
		^
		((?: \x1B \[ .*? m )*)  # ANSI color code
		\[ .*? \]: \            # Timestamp with trailing space
		").unwrap();
		
	// Console output: message-only or raw
	let (tx_console, rx_console) = broadcast::channel::<String>(16);
	let (tx_raw_console, rx_raw_console) = broadcast::channel::<String>(16);
	
	let mut output = BufReader::new(output).lines();
	while let Some(line) = output.next().await {
		let mut line = line.expect("io or utf error"); /* FIXME */
		
		// Remove interactive prompt related characters
		if let Some(found) = unwanted_escapes.find(&line) {
			line = line[found.end() .. ].to_string();
		};
		
		// Reset colouring at the end of the line
		println!("💻 {}\x1B[m", line);
		
		// Chop of the timestamp for tx_console
		let message = remove_timestamp.replace(&line, "$1");
		if message != line {
			// We actually removed the timestamp
			let mut message = message.into_owned(); // The (coloured) message
			
			// CHECK Add color reset at the end of the line (as it is always reenabled on the following line)
			message.push_str("\x1B[m");
			
			// Run the output filters
			{
				// LOCK
				let mut output_filters = output_filters.lock().unwrap();
				output_filters.retain(|x| run_output_filter(x, &message));
			}
			
			// errors if there are *currently* no receivers
			let _ = tx_console.send(message.to_string());
		}
		
		// errors if there are *currently* no receivers
		let _ = tx_raw_console.send(line);
	}
}

// Run a single output filter on a message, returns whether to keep checking it
fn run_output_filter (filter :& OutputFilter, message :&str) -> bool {
	// Check match
	if !filter.regex.is_match(&message) {
		// It didn't match, update max fail count
		if let Some(v) = &filter.fail {
			let remaining_tries = v.get() - 1;
			if remaining_tries < 0 {
				// No more tries, removing it
				return false;
			}
			v.set(remaining_tries);
		}
		return true;
	};
	
	// Call function
	let cb = filter.action.take().unwrap();
	match cb {
		FilterCallback::Once(callback) => {
			callback(&message);
			return false; // remove from filters
		},
		FilterCallback::Multiple(mut callback) => {
			callback(&message);
			// put it back
			filter.action.set(Some(FilterCallback::Multiple(callback)));
		},
	}
	
	// Update match count
	let count = filter.count.get() - 1;
	filter.count.set(count);
	
	count != 0 // <=> remove if last try
}

async fn handle_minecraft_input (
	mut input :impl 'static + Send + Unpin + AsyncWrite,
	mut rx_req :mpsc::Receiver<String>,
	mut input_filters :SharedInputFilters,
) {
	while let Some(mut command) = rx_req.recv().await {
		// Make sure the command ends with a newline
		if !command.ends_with("\n") {
			command.push_str("\n");
		}
		
		// DEBUG
		print!("✏️  {}", command);
		if let Err(_) = input.write_all(command.as_bytes()).await {
			eprintln!("🎁 Failed to write to minecraft console");
		}
		
		// NB The user is informed of command completion through the output filters
	}
}
