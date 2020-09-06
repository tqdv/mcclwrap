//! ExpiroSet: An ordered list of elements that expire after a timeout

use std::task::{Poll, Context}; use std::pin::Pin;
use std::fmt::Debug;
use std::sync::Arc;
use tokio::time::Duration;
use tokio::time::delay_queue::{self, DelayQueue, Expired};

/** An ordered list of elements that expire after a timeout

Based on `tokio::time::DelayQueue`.

Use the `Stream` implementation to extract and advance the timers.\
Use `.iter` to iterate over the elements in insertion order
*/
// The Arc<T> is only shared between the vmap and the queue
pub(crate) struct ExpiroSet<T> {
	vmap :Vec<(Arc<T>, delay_queue::Key)>,
	queue :DelayQueue<Arc<T>>,
}

impl<T :Eq + Debug> ExpiroSet<T> {
	/// Creates a new ExpiroSet
	pub fn new () -> Self {
		Self {
			vmap: Vec::new(),
			queue: DelayQueue::new(),
		}
	}

	/// Adds the value to the back of the list with the given timeout
	pub fn insert (&mut self, value :T, timeout :Duration) {
		let value = Arc::new(value);
		let key = self.queue.insert(value.clone(), timeout);
		self.vmap.push((value, key));
	}

	/// Removes the value from the list. Returns whether the value was present
	pub fn remove (&mut self, value :&T) -> bool {
		if let Some(i) = self.get_pos(value) {
			let (_, queue_key) = self.vmap.remove(i);
			self.queue.remove(&queue_key);
			true
		} else {
			false
		}
	}

	/// Sets the timeout of the value. Returns whether it was successful (ie. the value is present)
	pub fn reset (&mut self, value :&T, timeout :Duration) -> bool {
		if let Some(i) = self.get_pos(value) {
			let (_, queue_key) = &self.vmap[i];
			self.queue.reset(queue_key, timeout);
			true
		} else {
			false
		}
	}

	/// Returns an iterator over the items in insertion order
	pub fn iter (&self) -> impl '_ + Iterator<Item = &T> {
		self.vmap.iter().map(|v| &*v.0)
	}

	/** Tries to pull an element out of the delay queue, registering the wakeup when needed.
	Returns None when there are _currently_ no items in the queue */
	fn poll_expired (&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<T, tokio::time::Error>>> {
		// NB this also registers the waker
		let status = self.queue.poll_expired(cx);

		// If it has been removed from the queue, also remove it from the map
		if let Poll::Ready(Some(Ok(expired))) = status {
			let value :Arc<T> = expired.into_inner();
			if let Some(i) = self.get_pos(&value) {
				self.vmap.remove(i);
			}

			// Unwrap shouldn't fail because we removed it from vmap too.
			let value :T = Arc::try_unwrap(value).unwrap();
			Poll::Ready(Some(Ok(value)))
		} else {
			// convert Poll<Option<Result< Expired<Arc<T>> >… into Poll<Option<Result< T >…
			status.map(|v :Option<_>| v.map(|v :Result<_, _>| v.map(
				|_ :Expired<_>| unreachable!() // Converts to the right type
			)))
		}
	}

	/// Returns the position of value in the map if it is found
	fn get_pos (&self, value :&T) -> Option<usize> {
		self.vmap.iter().position(|e| *e.0 == *value)
	}
}

/// See StreamExt
impl<T :Eq + Debug> tokio::stream::Stream for ExpiroSet<T> {
	type Item = Result<T, tokio::time::Error>;

	/// NB: this also registers the waker cf. ExpiroSet::poll_expired
	fn poll_next (self :Pin<&mut Self>, cx :&mut Context) -> Poll<Option<Self::Item>> {
		Self::poll_expired(self.get_mut(), cx)
	}
}
