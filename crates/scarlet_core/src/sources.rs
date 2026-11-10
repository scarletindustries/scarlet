//! Where a build reads the text of the modules it loads. The caller hands a
//! [`Sources`] to each check, so the build holds no files, stats or editor
//! buffers of its own: a command reads from disk, and an editor reads its open
//! buffers first.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// A module's text, and its revision.
#[derive(Debug, Clone)]
pub struct Text {
    pub text: Rc<str>,
    pub(crate) revision: Revision,
}

impl Text {
    pub fn new(text: impl Into<Rc<str>>) -> Self {
        let text = text.into();
        Text {
            revision: Revision::of(&text),
            text,
        }
    }
}

/// What a module's text is, as a value that changes whenever the text does.
/// A cached check is reused only when its revision is the text's revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Revision(u64);

impl Revision {
    /// A 64-bit FNV-1a hash of the text.
    pub(crate) fn of(text: &str) -> Self {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in text.bytes() {
            h = (h ^ b as u64).wrapping_mul(0x100000001b3);
        }
        Revision(h)
    }
}

/// Where module text comes from.
pub trait Sources {
    /// The text of the file at `path`, now.
    fn read(&mut self, path: &Path) -> std::io::Result<Text>;
}

/// Files on disk. A file whose stat hasn't changed since it was last read
/// isn't read again.
#[derive(Default)]
pub struct DiskSources {
    read: HashMap<PathBuf, (FileStat, Text)>,
}

impl DiskSources {
    pub fn new() -> Self {
        DiskSources::default()
    }

    /// Say that the file at `path` changed, so the next read reads it even if
    /// its stat looks the same. An edit within one clock tick that keeps the
    /// length and inode would otherwise go unseen.
    pub fn changed(&mut self, path: &Path) {
        self.read.remove(path);
    }
}

impl Sources for DiskSources {
    fn read(&mut self, path: &Path) -> std::io::Result<Text> {
        let stat = file_stat(path)?;
        if let Some((seen, text)) = self.read.get(path)
            && *seen == stat
        {
            return Ok(text.clone());
        }
        let text = Text::new(std::fs::read_to_string(path)?);
        self.read.insert(path.to_path_buf(), (stat, text.clone()));
        Ok(text)
    }
}

/// A file's modification time, length and inode. An editor that saves by
/// replacing the file changes the inode even within the same second.
type FileStat = (std::time::SystemTime, u64, u64);

fn file_stat(path: &Path) -> std::io::Result<FileStat> {
    let meta = std::fs::metadata(path)?;
    #[cfg(unix)]
    let ino = std::os::unix::fs::MetadataExt::ino(&meta);
    #[cfg(not(unix))]
    let ino = 0;
    Ok((meta.modified()?, meta.len(), ino))
}
