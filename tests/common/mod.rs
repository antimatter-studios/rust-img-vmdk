//! Shared test scaffolding.
//!
//! # What this exists to stop
//!
//! Four test files each grew their own temp-path helper, and **two of
//! the four never deleted anything**: `synthetic.rs` and `write.rs`
//! returned a bare `PathBuf` and removed it at the end of the happy
//! path, so any assertion that panicked left a `.vmdk` behind in the
//! system temp directory. `corruption.rs` and `qemu_validation.rs` each
//! had a `TempPath` with a `Drop`, written twice.
//!
//! A `Drop` impl is the only version that survives a panic, which is
//! precisely the case a test fixture needs to survive: a test that
//! fails is the one most likely to leave rubbish behind, and the one
//! whose rubbish nobody notices because attention is on the failure.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// A temp file path that removes itself, panic or not.
pub struct TempPath(pub PathBuf);

impl TempPath {
    /// A unique path in the system temp directory.
    ///
    /// Named after the test that made it, and carrying the process id,
    /// so a crashed run leaves an identifiable corpse rather than a
    /// collision with the next run.
    pub fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!("vmdk_{tag}_{}_{n}.vmdk", std::process::id()));
        TempPath(p)
    }
}

impl std::ops::Deref for TempPath {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for TempPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Positional writes that work on every target.
///
/// `std::os::unix::fs::FileExt` does not exist on the Windows runner CI
/// also uses; seek-then-write is the portable equivalent, which is why
/// this takes `&mut self`.
pub trait WriteAt {
    fn write_all_at(&mut self, buf: &[u8], offset: u64) -> std::io::Result<()>;
}

impl WriteAt for std::fs::File {
    fn write_all_at(&mut self, buf: &[u8], offset: u64) -> std::io::Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        self.seek(SeekFrom::Start(offset))?;
        self.write_all(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixture is removed even when the test that made it panics.
    ///
    /// This is the whole difference between `TempPath` and the bare
    /// `PathBuf` that `synthetic.rs` and `write.rs` used to return: a
    /// cleanup at the end of the happy path never runs on the failure
    /// path — and the failure path is exactly when a fixture is most
    /// likely to be left behind, and least likely to be noticed,
    /// because attention is on the failure.
    #[test]
    fn a_panicking_test_still_removes_its_fixture() {
        let outcome = std::panic::catch_unwind(|| {
            let p = TempPath::new("drop_probe");
            std::fs::write(&p.0, b"x").expect("write fixture");
            assert!(p.0.exists(), "the fixture was created");
            panic!("deliberate");
        });
        assert!(outcome.is_err(), "the closure must have panicked");

        let dir = std::env::temp_dir();
        let strays: Vec<_> = std::fs::read_dir(&dir)
            .expect("read temp dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("vmdk_drop_probe_"))
            .collect();
        assert!(
            strays.is_empty(),
            "a panicking test left fixtures behind: {strays:?}"
        );
    }
}
