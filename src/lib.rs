//! Pure-Rust VMDK (VMware Virtual Machine Disk) reader.
//!
//! Currently handles the **monolithic sparse** variant — by far the
//! most common shape: a single `.vmdk` file containing a sparse-extent
//! header, an embedded text descriptor, the grain directory, the grain
//! tables, and the grain data. Other variants (monolithicFlat split
//! across files, twoGbMaxExtent, streamOptimized, vmfs) are reported
//! as [`Error::Unsupported`] so callers can surface a clear message
//! rather than reading garbage.
//!
//! Implements [`fs_core::BlockRead`] and [`fs_core::BlockDevice`] so the
//! reader plugs straight into the partition probe + filesystem driver
//! stack.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod capi;
pub mod descriptor;
pub mod error;
pub mod header;
pub mod reader;

pub use error::{Error, Result};
pub use header::{SparseHeader, MAGIC};
pub use reader::VmdkReader;
