//! Our slang

pub(crate) use tear::prelude::*;
pub(crate) use tear::ret;
pub(crate) use tokio::prelude::*;
pub(crate) use lazy_static::lazy_static;
pub(crate) use tokio::select;

pub(crate) use crate::util;
pub(crate) use tokio::sync::{mpsc, broadcast};

pub(crate) use regex::Regex;
pub(crate) use std::sync::{Arc, Mutex};
pub(crate) use nix::unistd::Pid;
pub(crate) use tokio::io::BufReader;
pub(crate) use tokio::time::Duration;

pub(crate) use tokio::stream::StreamExt as _;

pub(crate) trait ResultExt<T> {
	/// Returns the inner value if both Ok and Err wrap the same type
	fn into_inner (self) -> T;
}

pub(crate) trait VecExt<T> {
	/** Call the function for each element and return the number of elements removed
	The function returns true to keep the element, and false to remove it from the vec */
	fn map_retain(&mut self, f :impl FnMut(&mut T) -> bool) -> usize;
}

pub(crate) trait StringExt {
	/// Returns a string that ends with the provided suffix
	fn now_ends_with (&mut self, suf :&str);
}

macro_rules! mut_scope {
	( $var:ident, $($block:tt)* ) => {
		let mut $var = $var;
		let $var = {
			$($block)*;
			$var
		};
	}
}

// === `impl`s ===

impl<T> ResultExt<T> for Result<T, T> {
	fn into_inner (self) -> T {
		match self {
			Ok(v) => v,
			Err(v) => v,
		}
	}
}

impl<T> VecExt<T> for Vec<T> {
	fn map_retain(&mut self, mut f :impl FnMut(&mut T) -> bool) -> usize {
		// cf. std::vec::Vec::retain source code (which is deprecated)
		let len = self.len();
		let mut del = 0; // nb of deleted elements

		for i in 0..len {
			let keep = f(&mut self[i]);
			if !keep {
				del += 1;
			} else if del > 0 {
				self.swap(i - del, i);
			}
		}
		if del > 0 {
			self.truncate(len - del);
		}

		del
	}
}

impl StringExt for String {
	fn now_ends_with (&mut self, suf :&str) {
		if !self.ends_with(suf) {
			self.push_str(suf);
		}
	}
}
