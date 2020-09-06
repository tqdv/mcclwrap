/*! A ready channel (spmc)

```
# use tokio::time::{delay_for, Duration}

let (tx, rx) = ready::channel();
let ready = rx.clone();

tokio::spawn(async move {
	delay_for(Duration::from_secs(1)).await;
	tx.ready();
});

tokio::spawn(async move {
	ready.await;
	println!("ready!");
});

if let None = rx.await {
	println!("closed before ready!");
}

// Output after 1s: ready!␤
```
*/

use tear::prelude::*;

use std::collections::HashSet;

use std::sync::{Arc, Weak, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{future::Future, pin::Pin};
use std::task::{Poll, Context, Waker};

use std::cmp::{Eq, PartialEq};
use std::hash::Hash;
use std::ops::Deref;

// based on tokio::sync::watch

const CLOSED :usize = 1;
const READY  :usize = 2;

// The id for the HashSet comes from the Arc
#[derive(Clone, Debug)]
struct Listener (Arc<Waker>);

impl PartialEq for Listener {
	fn eq(&self, other: &Listener) -> bool {
		Arc::ptr_eq(&self.0, &other.0) }}

impl Eq for Listener {}

impl Hash for Listener {
	fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
		(&*self.0 as *const Waker).hash(state) }}

impl Deref for Listener {
	type Target = Arc<Waker>;
	fn deref (&self) -> &Self::Target { &self.0 }
}

#[derive(Debug)]
pub struct Sender{
	shared: Weak<Shared>,
}

#[derive(Debug)]
pub struct Receiver {
	shared :Arc<Shared>,
	listener :Option<Listener>,
}

#[derive(Debug)]
struct Shared {
	ready :AtomicUsize,
	/// WARN This mutex protects the hashset AND the atomicusize !
	listeners :Mutex<HashSet<Listener>>,
}

impl Sender {
	/// Returns None if there are no receivers
	pub fn ready(&self) -> Option<()> {
		// Check if we have receivers
		let shared = terror! { self.shared.upgrade() };
		
		{ // LOCK listeners
			let listeners = shared.listeners.lock().unwrap();
			
			// Set state
			shared.ready.store(READY, Ordering::Relaxed);
			
			// Wake receivers
			for listener in listeners.iter() {
				listener.wake_by_ref();
			}
		}
		
		Some(())
	}
}

// CHECKME I'm not sure if this is correct async code
impl Future for Receiver {
	type Output = Option<()>;
	
	fn poll (mut self: Pin<&mut Self>, cx: &mut Context<'_>)
		-> Poll<Self::Output>
	{
		// Read state
		let state = self.shared.ready.load(Ordering::Relaxed);
		
		// It's ready
		if state & READY == READY {
			return Poll::Ready(Some(()));
		}
		
		// We will never get a ready
		if state & CLOSED == CLOSED {
			return Poll::Ready(None);
		}
		
		let my_waker = cx.waker();		
		// Q: Will the waker wake us ?
		if let Some(true) = self.listener.as_ref().map(|l| l.will_wake(my_waker)) {
			// A: Yes, nothing to do
		}
		else {
			// A: No, update the listener
			let new_listener = Listener(Arc::new(my_waker.clone()));
			
			// Our copy…
			self.listener = Some(new_listener.clone());
			
			// LOCK shared listener set
			let mut listeners = self.shared.listeners.lock().unwrap();
			
			// Schedule immediate wakeup if the state has changed since
			if self.shared.ready.load(Ordering::Relaxed) != 0 {
				// Schedule immediate wakeup
				cx.waker().wake_by_ref();
			}
			
			// Update the set
			if let Some(old_listener) = &self.listener {
				listeners.remove(old_listener);
			}
			listeners.insert(new_listener);
		}
		
		Poll::Pending
	}
}

impl Drop for Sender {
	fn drop (&mut self) {
		// Wake receivers if any
		if let Some(shared) = self.shared.upgrade() {
			// LOCK shared state
			let listeners = shared.listeners.lock().unwrap();
			
			// Update state
			shared.ready.fetch_or(CLOSED, Ordering::AcqRel);
			
			// Wake listeners
			for listener in listeners.iter() {
				listener.wake_by_ref();
			}
		}
	}
}

impl Drop for Receiver {
	fn drop (&mut self) {
		// Remove our listener from the shared set
		if let Some(our_listener) = &self.listener {
			let mut listeners = self.shared.listeners.lock().unwrap();
			listeners.remove(our_listener);
		}
	}
}

impl Clone for Receiver {
	fn clone (&self) -> Self {
		Receiver {
			shared: self.shared.clone(),
			listener: None,
		}
	}
}

pub fn channel () -> (Sender, Receiver) {
	let shared = Arc::new(Shared {
		ready: AtomicUsize::new(0),
		listeners: Mutex::new(HashSet::new()),
	});
	
	let tx = Sender { shared: Arc::downgrade(&shared) };
	let rx = Receiver { shared, listener: None };
	
	(tx, rx)
}
