//! Data types used for filtering the Minecraft console IO
use crate::slang::*;
use crate::ready;
use crate::minecraft::AttendantRequest;

// === Output Filters ===

/// Filter output received by the client
#[derive(Debug, Clone)] // Clone is needed to copy out of the broadcast channel
pub(crate) enum FilterOutput {
	Line(String), // Line matched by the output filter
	Expired,      // Input filter has expired
}

impl FilterOutput {
	/// Consumes self and returns the matched line if any
	pub fn take_line (self) -> Option<String> {
		match self {
			Self::Line(v) => Some(v),
			_ => None,
		}
	}
}

#[derive(Debug)]
pub(crate) struct OutputFilter {
	// Filter data:
	pub(crate) regex :Regex,
	/// We use broadcast instead of mpsc because it doesn't await on send
	pub(crate) chan :broadcast::Sender<FilterOutput>,

	// Filter options:
	/// How many lines can match the regex. None means unrestricted.
	pub(crate) count :Option<i32>,
	/// How many lines can not to match.
	pub(crate) fail :Option<i32>,
	/// Should we process chat lines ?
	pub(crate) ignore_chat :bool,
}

impl OutputFilter {
	/** Creates a new output filter builder with the following defaults:
	- the filter never expires
	- the filter ignores chat
	- channel capacity is 16 */
	pub fn new(regex :Regex) -> OutputFilterBuilder {
		OutputFilterBuilder {
			regex,
			chan_capacity: 16,
			count: None,
			fail: None,
			ignore_chat: true,
		}
	}

	/** Run the filter against a message, and returns whether to keep checking it.
	This updates the match counts if there are any. If there are no listeners, the filter is dropped */
	pub fn check (&mut self, message :&str, is_chat :bool) -> bool {
		/// Send FilterOutput::Expired and return false to remove the filter
		fn send_expired_message (myself :&mut OutputFilter) -> bool {
			// It doesn't matter if there are no receivers, we're removing this filter anyways
			let _ = myself.chan.send(FilterOutput::Expired);
			return false;
		};

		tear_if! {is_chat && self.ignore_chat, true }

		// Check match
		if !self.regex.is_match(&message) {
			// It didn't match, update fail count
			if let Some(tries) = &mut self.fail {
				*tries -= 1;
				if *tries <= 0 {
					return send_expired_message(self); // No more tries, remove it
				}
			} else {
				return true; // No fail limit, keep it
			}
		}

		// Send line to client
		if let Err(_) = self.chan.send(FilterOutput::Line(message.to_string())) {
			// There are currently no receivers (which means that the client has stopped listening)
			return false;
		}

		// Update match count
		if let Some(c) = &mut self.count {
			*c -= 1;
			if *c > 0 {
				true // Keep it if the client expects more lines
			} else {
				send_expired_message(self) // It is done
			}
		} else {
			true // No match limit, keep it
		}
	}
}

/** Builder struct for OutputFilter.
This is a consuming builder ie. the methods take `self` instead of `&mut self` */
pub(crate) struct OutputFilterBuilder {
	regex :Regex, // Option<_>, so we can .take() the Regex. It should always be initialized
	chan_capacity :usize,
	count :Option<i32>,
	fail :Option<i32>,
	ignore_chat :bool,
}

impl OutputFilterBuilder {
	pub fn capacity(mut self, cap :usize) -> Self { self.chan_capacity = cap; self }

	pub fn count (mut self, c :Option<i32>) -> Self { self.count = c; self }

	pub fn fail (mut self, f :Option<i32>) -> Self { self.fail = f; self }

	pub fn ignore_chat (mut self, ic :bool) -> Self { self.ignore_chat = ic; self }

	pub fn build (self) -> (OutputFilter, broadcast::Receiver<FilterOutput>) {
		let (tx, rx) = broadcast::channel(self.chan_capacity);

		let obj = OutputFilter {
			regex: self.regex,
			chan: tx,
			count: self.count,
			fail: self.fail,
			ignore_chat: self.ignore_chat,
		};

		(obj, rx)
	}
}

// === Input Filters ===

#[derive(Debug)]
pub(crate) struct InputFilter {
	// Filter data:
	pub(crate) regex :Regex,
	/// To signal guard expiration
	pub(crate) expired :ready::Sender,
	/// To allow requests sent by the same client
	pub(crate) client_id :u32,
}

impl InputFilter {
	/// Checks if a message from client_id is allowed by this filter
	pub fn is_match (&self, message :&str) -> bool {
		self.regex.is_match(message)
	}

	/// Checks if the filter comes from the same attendant
	pub fn from_same_attendant (&self, request :&AttendantRequest) -> bool {
		self.client_id == request.client_id
	}

	/// Signals to the client this filter has expired. Returns None if there are no receivers
	pub fn signal_expiration (&self) -> Option<()> {
		self.expired.ready()
	}
}

/// Newtype for Arc<InputFilter> because we need to implement (simple) Eq and share it
#[derive(Debug, Clone)] // also does PartialEq, Eq, Hash, Deref cf. later this file
pub(crate) struct OurInputFilter (pub(crate) Arc<InputFilter>);

impl OurInputFilter {
	/// Get wrapped Arc's strong reference count
	pub fn strong_count (&self) -> usize {
		Arc::strong_count(&self.0)
	}
}

// === impls ===

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
