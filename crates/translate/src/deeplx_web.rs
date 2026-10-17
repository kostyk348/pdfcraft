//! The web build has no sockets, so there is no network backend there: [`Deeplx`] exists (callers
//! build it by name) but nothing can be reached through it.

use crate::{Error, Result, Translator};

/// Unreachable on the web: [`Deeplx::new`] always fails, so no value of this type exists.
pub struct Deeplx;

impl Deeplx {
    pub fn new(_endpoint: &str, _source_lang: &str, _target_lang: &str, _timeout_secs: f64) -> Result<Self> {
        Err(Error::Invalid("network translation needs a native build: the web build has no sockets, so translate offline with a map".into()))
    }

    pub fn endpoint(&self) -> &str {
        ""
    }
}

impl Translator for Deeplx {
    fn translate(&mut self, _text: &str) -> Result<Option<String>> {
        Err(Error::Invalid("network translation needs a native build: the web build has no sockets".into()))
    }
}
