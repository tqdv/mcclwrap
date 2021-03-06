//! Utility functions

use crate::slang::*;

pub(crate) fn sigint_process (pid :Pid) -> nix::Result<()> {
	use nix::sys::signal::{kill, Signal};
	
	kill(pid, Signal::SIGINT)
}

pub(crate) fn sigkill_process (pid :Pid) -> nix::Result<()> {
	use nix::sys::signal::{kill, Signal};
	
	kill(pid, Signal::SIGKILL)
}

pub(crate) const ASCII_WS :&[char] = &[' ', '\t'];
pub(crate) const ASCII_WS_AND_NL :&[char] = &[' ', '\t', '\n'];

pub(crate) fn strip_leading_ascii_hspace(s :&str) -> &str {
	s.trim_start_matches(&[' ', '\t'] as &[_])
}

// lazy_static! {
// 	static ref COLOR_REGEX :Regex = Regex::new(r"(?x)
// 		\x1B .*? m").unwrap();
// }
//
// FIXME unused
// pub(crate) fn strip_colors<'a> (s :impl AsRef<str> + 'a) -> String {
// 	COLOR_REGEX.replace_all(s.as_ref(), "").into_owned()
// }
