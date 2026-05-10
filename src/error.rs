//! Narrow error type for the VMDK reader. Each variant maps cleanly onto
//! [`fs_core::Error`] at the trait boundary; see [`crate::reader`].

use std::fmt;
use std::io;

#[derive(Debug)]
pub enum Error {
    /// Underlying I/O failure (open, seek, read, write).
    Io(io::Error),
    /// Magic number didn't match `KDMV`.
    NotVmdk,
    /// Header or descriptor field combination is internally inconsistent.
    Corrupt(&'static str),
    /// A VMDK variant the reader doesn't yet handle (everything other
    /// than `monolithicSparse`).
    Unsupported(&'static str),
    /// Read past the end of the virtual disk.
    OutOfBounds { offset: u64, len: u64, size: u64 },
    /// `write_at` called on an image opened read-only, or on top of a
    /// non-writable backing device.
    ReadOnly,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io: {e}"),
            Error::NotVmdk => write!(f, "not a VMDK image (magic mismatch)"),
            Error::Corrupt(s) => write!(f, "corrupt VMDK: {s}"),
            Error::Unsupported(s) => write!(f, "unsupported VMDK feature: {s}"),
            Error::OutOfBounds { offset, len, size } => {
                write!(
                    f,
                    "read [{offset}, {offset}+{len}) past virtual size {size}"
                )
            }
            Error::ReadOnly => write!(f, "image was opened read-only"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
