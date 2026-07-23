//! A console the tests can read back.

use std::string::String;
use std::vec::Vec;

use crate::console::Console;

pub(crate) struct Capture(Vec<u8>);

impl Capture {
    pub(crate) fn new() -> Self {
        Self(Vec::new())
    }

    /// What was printed, which every test here writes as text.
    pub(crate) fn text(&self) -> String {
        String::from_utf8(self.0.clone()).expect("the shell printed bytes that are not UTF-8")
    }
}

impl Console for Capture {
    fn write(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }
}
