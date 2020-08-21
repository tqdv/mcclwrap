//! Data types used for filtering the Minecraft console IO
use crate::slang::*;

// === Output Filters ===

#[derive(Debug)]
pub(crate) struct OutputFilter {
	// Filter data:
	pub(crate) regex :Regex,
	/// We use broadcast instead of mpsc because it doesn't await on send
	pub(crate) chan :broadcast::Sender<String>,

	// Filter options:
	/// How many lines can match the regex. None means unrestricted.
	pub(crate) count :Option<i32>,
	/// How many lines can not to match.
	pub(crate) fail :Option<i32>,
	/// Should we process chat lines ?
	pub(crate) ignore_chat :bool,
}

// === Input Filters ===

#[derive(Debug)]
pub(crate) struct InputFilter {
	// Filter data:
	pub(crate) regex :Regex,
	/// To signal guard expiration
	pub(crate) chan :oneshot::Sender<()>, // FIXME
	/// To allow requests sent by the same client
	pub(crate) client_id :u32,
}

#[derive(Debug, Clone)] // also does PartialEq, Eq, Hash, Deref cf. later this file
pub(crate) struct OurInputFilter (pub(crate) Arc<InputFilter>);

// === Output produced by the filters

struct FilterOutput {
	filter_id :u32,
	message :FilterOutputMessage
}

/// Received by the client
enum FilterOutputMessage {
	Line(String), // Line matched by the output filter
	Expired(u32), // Input filter has expired
}

// === Shared sets of filters ===

pub(crate) type SharedOutputFilters = Arc<Mutex<Vec<OutputFilter>>>;

// === Helper functions ===

impl OutputFilter {
	pub fn new(regex :Regex) -> OutputFilterBuilder {
		OutputFilterBuilder {
			regex: Some(regex),
			chan_capacity: 16,
			count: None,
			fail: None,
			ignore_chat: true,
		}
	}
}

/// Builder struct for OutputFilter
pub(crate) struct OutputFilterBuilder {
	regex :Option<Regex>, // Option<_>, so we can .take() the Regex. It should always be initialized
	chan_capacity :usize,
	count :Option<i32>,
	fail :Option<i32>,
	ignore_chat :bool,
}

// === implementations ===

impl OutputFilterBuilder {
	pub fn capacity(&mut self, cap :usize) -> &mut Self { self.chan_capacity = cap; self }

	pub fn count (&mut self, c :Option<i32>) -> &mut Self { self.count = c; self }

	pub fn fail (&mut self, f :Option<i32>) -> &mut Self { self.fail = f; self }

	pub fn ignore_chat (&mut self, ic :bool) -> &mut Self { self.ignore_chat = ic; self }

	pub fn build (&mut self) -> (OutputFilter, broadcast::Receiver<String>) {
		let (tx, rx) = broadcast::channel(self.chan_capacity);

		let obj = OutputFilter {
			regex: self.regex.take().unwrap(),
			chan: tx,
			count: self.count,
			fail: self.fail,
			ignore_chat: self.ignore_chat,
		};

		(obj, rx)
	}
}

// {{{ impl PartialEq, Eq, Hash, Deref for OurInputFilter
	impl PartialEq for OurInputFilter {
		fn eq(&self, other: &Self) -> bool {
			Arc::ptr_eq(&self.0, &other.0)
		}
	}

	impl Eq for OurInputFilter {}

	impl std::hash::Hash for OurInputFilter {
		fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
			(&*self.0 as *const InputFilter).hash(state)
		}
	}

	impl std::ops::Deref for OurInputFilter {
		type Target = InputFilter;
		fn deref(&self) -> &Self::Target {
			&self.0
		}
	}
// }}}
