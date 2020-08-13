//! Our slang

pub(crate) use tear::prelude::*;
pub(crate) use tokio::prelude::*;
pub(crate) use lazy_static::lazy_static;

pub(crate) use tokio::sync::mpsc;

pub(crate) use regex::Regex;
pub(crate) use std::sync::{Arc, Mutex};
pub(crate) use std::cell::Cell;
pub(crate) use nix::unistd::Pid;
pub(crate) use tokio::io::BufReader;

pub(crate) use tokio::stream::StreamExt as _;

pub(crate) trait ResultExt<T> {
	fn into_inner (self) -> T;
}

impl<T> ResultExt<T> for Result<T, T> {
	/// Returns the inner value if both Ok and Err wrap the same type
	fn into_inner (self) -> T {
		match self {
			Ok(v) => v,
			Err(v) => v,
		}
	}
}