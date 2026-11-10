//! A module's text, as one check read it. Diagnostics point at one of these,
//! so a message is always shown against the text it was made from, never
//! against a file read again afterwards.

use std::path::PathBuf;
use std::rc::Rc;

use crate::module_path::ModuleKey;

#[derive(Debug, PartialEq, Eq)]
pub struct SourceFile {
    pub key: ModuleKey,
    /// The file it was read from, when it came from one.
    path: Option<PathBuf>,
    pub text: Rc<str>,
}

impl SourceFile {
    pub fn new(key: ModuleKey, path: Option<PathBuf>, text: impl Into<Rc<str>>) -> Rc<Self> {
        Rc::new(SourceFile {
            key,
            path,
            text: text.into(),
        })
    }

    /// How to name it to a person: its path, or else its key.
    pub(crate) fn display_name(&self) -> String {
        match &self.path {
            Some(p) => p.display().to_string(),
            None => self.key.to_string(),
        }
    }
}
