/*! A set of elements that can expire

You insert and remove `Arc<T>`. This is because we're using it to share the item between the
hashmap and the delayqueue.

inserting removing elements with insert and remove
advancing expiry state with ?
*/

use std::sync::Arc;
use std::collections::HashMap;
use std::task::{Poll, Context};
use std::pin::Pin;
use tokio::time::delay_queue::{self, DelayQueue, Expired};
use tokio::time::Duration;

use std::hash::Hash;

struct ExpiroSet<T :Hash + Eq> {
	map :HashMap<Arc<T>, delay_queue::Key>,
	queue :DelayQueue<Arc<T>>,
}

impl<T :Hash + Eq> ExpiroSet<T> {
	/** Inserts the value into the set with the given timeout. Returns if the value is new to the set */
	pub fn insert (&mut self, value :Arc<T>, timeout :Duration) -> bool {
		let key = self.queue.insert(value.clone(), timeout);
		self.map.insert(value, key).is_none()
	}

	/** Removes the value from the set. Returns whether the value belonged to the set */
	pub fn remove<B> (&mut self, value :&Arc<T>) -> bool
	{
		if let Some(queue_key) = self.map.remove(value) {
			// It hasn't expired yet, remove it from the queue
			self.queue.remove(&queue_key);
			true
		} else {
			false
		}
	}

	/** Sets the delay of value to timeout. Returns whether it was successful (ie. still in the set) */
	pub fn reset (&mut self, value :&Arc<T>, timeout :Duration) -> bool {
		if let Some(queue_key) = self.map.get(value) {
			self.queue.reset(queue_key, timeout);
			true
		} else {
			false
		}
	}

	/** Tries to pull an element out of the delay queue, registering the wakeup when needed.
	Returns None when there are currently no items in the queue */
	fn poll_expired (&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<Expired<Arc<T>>, tokio::time::Error>>> {
		// This also registers the waker
		let status = self.queue.poll_expired(cx);
		// If it has been removed from the queue, also remove it from the map
		if let Poll::Ready(Some(Ok(expired))) = &status {
			let expired = expired.into_inner();
			self.map.remove(&expired);
		}
		status
	}
}

impl<T :Hash + Eq> tokio::stream::Stream for ExpiroSet<T> {
	type Item = Result<Expired<Arc<T>>, tokio::time::Error>;

	fn poll_next (self :Pin<&mut Self>, cx :&mut Context) -> Poll<Option<Self::Item>> {
		Self::poll_expired(self.get_mut(), cx)
	}
}
