// Shared by both tiers, included textually rather than depended on.
//
// `tests/fuzz_decoders.rs` and `fuzz/src/lib.rs` both `include!` this
// file. A crate dependency would have been tidier, but the fuzz crate
// depends on `libfuzzer-sys`, which builds libFuzzer's C++ runtime, and
// making the gate depend on the fuzz crate would drag that into every
// pull request build on the stable toolchain.
//
// What matters is that the two tiers read an image identically, so a
// reproducer from one reproduces in the other.

// Fully qualified below rather than imported: this file is `include!`d
// into modules that already import these names, and a duplicate `use`
// is a hard error.
use fs_core::{BlockRead, Result as BlockResult};

/// An image held in memory, presented as a device.
pub struct Bytes(pub Vec<u8>);

impl BlockRead for Bytes {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> BlockResult<()> {
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        let end = start.saturating_add(buf.len());
        if end > self.0.len() {
            // What a real device answers for a read past its end, so a
            // crafted image cannot be told apart from a truncated one
            // by which error it provokes.
            return Err(fs_core::Error::ShortRead {
                offset,
                want: buf.len(),
                got: self.0.len().saturating_sub(start),
            });
        }
        buf.copy_from_slice(&self.0[start..end]);
        Ok(())
    }

    fn size_bytes(&self) -> u64 {
        self.0.len() as u64
    }
}

/// How far into the virtual disk a walk will read.
///
/// A crafted header can claim any capacity, and reading all of it would
/// make a case slow rather than failing it -- which reads as a hang
/// without being one.
pub const READ_BUDGET: u64 = 1 << 20;

/// Open an image and read what a caller would: the geometry, the
/// descriptor, and data from either side of the grain table.
///
/// Every result is discarded. A crafted image is *supposed* to be
/// refused; what it may not do is panic, hang, or read somebody else's
/// memory.
pub fn walk(image: &[u8]) {
    let dev: std::sync::Arc<dyn BlockRead> = std::sync::Arc::new(Bytes(image.to_vec()));
    let Ok(reader) = vmdk::VmdkReader::open_on_device(dev) else {
        return;
    };

    let virtual_size = reader.virtual_size();
    let grain = reader.grain_size_bytes();
    let _ = reader.descriptor();
    let _ = reader.is_writable();
    let header = reader.header();
    let _ = header.uses_zeroed_grain_marker();
    let _ = header.has_redundant_grain_directory();
    let _ = header.is_stream_optimized();

    // Grain boundaries are where the directory and table arithmetic
    // happens, so step by one -- but only while the step is a real
    // number, since a crafted header can claim a grain size of zero.
    if grain > 0 {
        let mut at = 0u64;
        let mut steps = 0;
        let mut buf = [0u8; 512];
        while at < virtual_size.min(READ_BUDGET) && steps < 256 {
            let _ = reader.read_at(at, &mut buf);
            at = at.saturating_add(grain);
            steps += 1;
        }
    }

    let want = usize::try_from(virtual_size.min(READ_BUDGET)).unwrap_or(0);
    if want > 0 {
        let mut buf = vec![0u8; want];
        let _ = reader.read_at(0, &mut buf);
    }
}
