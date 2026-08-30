//! Sparse extent header (512 bytes at offset 0 of a monolithic sparse VMDK).
//!
//! Layout (offsets within the 512-byte header). All multi-byte integers
//! are LITTLE-ENDIAN.
//!
//! ```text
//!   0   4  magic_number       (= 0x564D444B 'KDMV')
//!   4   4  version
//!   8   4  flags
//!  12   8  capacity            (sectors of 512 bytes — virtual size)
//!  20   8  grain_size          (sectors per grain — typically 128 = 64 KiB)
//!  28   8  descriptor_offset   (sector — embedded descriptor location)
//!  36   8  descriptor_size     (sectors)
//!  44   4  num_gtes_per_gt     (typically 512)
//!  48   8  rgd_offset          (sector — redundant grain directory)
//!  56   8  gd_offset           (sector — primary grain directory)
//!  64   8  over_head           (sectors before first grain)
//!  72   1  unclean_shutdown
//!  73   1  single_end_line_char
//!  74   1  non_end_line_char
//!  75   1  double_end_line_char1
//!  76   1  double_end_line_char2
//!  77   2  compress_algorithm  (0=none, 1=DEFLATE)
//!  79 433  pad (zeros)
//! ```

use crate::error::{Error, Result};

pub const HEADER_SIZE: usize = 512;
pub const MAGIC: u32 = 0x564D_444B; // 'KDMV' little-endian on disk

#[derive(Debug, Clone)]
pub struct SparseHeader {
    pub version: u32,
    pub flags: u32,
    pub capacity: u64,
    pub grain_size: u64,
    pub descriptor_offset: u64,
    pub descriptor_size: u64,
    pub num_gtes_per_gt: u32,
    pub rgd_offset: u64,
    pub gd_offset: u64,
    pub over_head: u64,
    pub unclean_shutdown: u8,
    pub compress_algorithm: u16,
}

/// Byte offsets of each field within the on-disk `SparseExtentHeader`.
///
/// The module documentation above draws the structure as an ASCII
/// table, which is good documentation and is not the same as a name: a
/// numeric literal in a parse expression carries no way to tell a
/// correct offset from a typo, while a name can be checked against the
/// table by eye.
///
/// **`COMPRESS_ALGORITHM` is the one that matters most.** It is where
/// the compression flag lives, so reading it from the wrong place means
/// silently accepting a `streamOptimized` image as an ordinary one —
/// and then decoding its grains as raw data.
///
/// `tests/corruption.rs` had already felt the need and half-met it,
/// naming three of these in a test file where the parser that also
/// needs them could not see them.
///
/// The module is `pub` rather than `pub(crate)` precisely so those
/// tests can import it: integration tests compile as a separate crate.
/// That makes it public surface, which is the right call for a format
/// crate — the layout is already published as an ASCII table in this
/// module's own documentation, so naming the offsets adds no
/// commitment that the drawing did not already make.
pub mod offsets {
    /// `magicNumber` — `KDMV`.
    pub const MAGIC: usize = 0;
    /// `version`.
    pub const VERSION: usize = 4;
    /// `flags`.
    pub const FLAGS: usize = 8;
    /// `capacity`, in sectors.
    pub const CAPACITY: usize = 12;
    /// `grainSize`, in sectors.
    pub const GRAIN_SIZE: usize = 20;
    /// `descriptorOffset`, in sectors.
    pub const DESCRIPTOR_OFFSET: usize = 28;
    /// `descriptorSize`, in sectors.
    pub const DESCRIPTOR_SIZE: usize = 36;
    /// `numGTEsPerGT`.
    pub const NUM_GTES_PER_GT: usize = 44;
    /// `rgdOffset` — redundant grain directory, in sectors.
    pub const RGD_OFFSET: usize = 48;
    /// `gdOffset` — grain directory, in sectors.
    pub const GD_OFFSET: usize = 56;
    /// `overHead`, in sectors.
    pub const OVER_HEAD: usize = 64;
    /// `uncleanShutdown` — one byte.
    pub const UNCLEAN_SHUTDOWN: usize = 72;
    /// `compressAlgorithm`.
    ///
    /// Not 76: the four single-byte `singleEndLineChar` /
    /// `nonEndLineChar` / `doubleEndLineChar1` / `doubleEndLineChar2`
    /// fields sit at 73..=76, so the compression word starts at 77 and
    /// is therefore **unaligned**. That is the format's doing, and it is
    /// why this offset looks wrong at a glance and is not.
    pub const COMPRESS_ALGORITHM: usize = 77;
}

impl SparseHeader {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < HEADER_SIZE {
            return Err(Error::Corrupt("header shorter than 512 bytes"));
        }
        let magic = read_u32(bytes, offsets::MAGIC);
        if magic != MAGIC {
            return Err(Error::NotVmdk);
        }
        let version = read_u32(bytes, offsets::VERSION);
        let flags = read_u32(bytes, offsets::FLAGS);
        let capacity = read_u64(bytes, offsets::CAPACITY);
        let grain_size = read_u64(bytes, offsets::GRAIN_SIZE);
        let descriptor_offset = read_u64(bytes, offsets::DESCRIPTOR_OFFSET);
        let descriptor_size = read_u64(bytes, offsets::DESCRIPTOR_SIZE);
        let num_gtes_per_gt = read_u32(bytes, offsets::NUM_GTES_PER_GT);
        let rgd_offset = read_u64(bytes, offsets::RGD_OFFSET);
        let gd_offset = read_u64(bytes, offsets::GD_OFFSET);
        let over_head = read_u64(bytes, offsets::OVER_HEAD);
        let unclean_shutdown = bytes[offsets::UNCLEAN_SHUTDOWN];
        let compress_algorithm = read_u16(bytes, offsets::COMPRESS_ALGORITHM);

        if grain_size == 0 {
            return Err(Error::Corrupt("grain_size is zero"));
        }
        if num_gtes_per_gt == 0 {
            return Err(Error::Corrupt("num_gtes_per_gt is zero"));
        }
        if compress_algorithm != 0 {
            return Err(Error::Unsupported(
                "compressed VMDK (compress_algorithm != 0)",
            ));
        }

        Ok(SparseHeader {
            version,
            flags,
            capacity,
            grain_size,
            descriptor_offset,
            descriptor_size,
            num_gtes_per_gt,
            rgd_offset,
            gd_offset,
            over_head,
            unclean_shutdown,
            compress_algorithm,
        })
    }
}

fn read_u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

fn read_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn read_u64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        b[off],
        b[off + 1],
        b[off + 2],
        b[off + 3],
        b[off + 4],
        b[off + 5],
        b[off + 6],
        b[off + 7],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_magic() {
        let bytes = [0u8; HEADER_SIZE];
        assert!(matches!(SparseHeader::parse(&bytes), Err(Error::NotVmdk)));
    }

    #[test]
    fn parses_minimal_header() {
        let mut h = [0u8; HEADER_SIZE];
        h[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        h[4..8].copy_from_slice(&1u32.to_le_bytes());
        h[12..20].copy_from_slice(&2048u64.to_le_bytes()); // capacity
        h[20..28].copy_from_slice(&128u64.to_le_bytes()); // grain_size
        h[28..36].copy_from_slice(&1u64.to_le_bytes()); // descriptor_offset
        h[36..44].copy_from_slice(&20u64.to_le_bytes()); // descriptor_size
        h[44..48].copy_from_slice(&512u32.to_le_bytes()); // num_gtes_per_gt
        h[56..64].copy_from_slice(&100u64.to_le_bytes()); // gd_offset

        let p = SparseHeader::parse(&h).unwrap();
        assert_eq!(p.version, 1);
        assert_eq!(p.capacity, 2048);
        assert_eq!(p.grain_size, 128);
        assert_eq!(p.num_gtes_per_gt, 512);
        assert_eq!(p.gd_offset, 100);
    }

    /// A header with every required field set to a valid value, ready to
    /// be perturbed by a single test.
    fn valid_header() -> [u8; HEADER_SIZE] {
        let mut h = [0u8; HEADER_SIZE];
        h[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        h[4..8].copy_from_slice(&1u32.to_le_bytes());
        h[12..20].copy_from_slice(&2048u64.to_le_bytes());
        h[20..28].copy_from_slice(&128u64.to_le_bytes());
        h[28..36].copy_from_slice(&1u64.to_le_bytes());
        h[36..44].copy_from_slice(&20u64.to_le_bytes());
        h[44..48].copy_from_slice(&512u32.to_le_bytes());
        h[56..64].copy_from_slice(&100u64.to_le_bytes());
        h[64..72].copy_from_slice(&7u64.to_le_bytes()); // over_head
        h
    }

    #[test]
    fn rejects_header_shorter_than_512_bytes() {
        let err = SparseHeader::parse(&[0u8; 100]).unwrap_err();
        assert!(matches!(err, Error::Corrupt(_)), "got {err:?}");
    }

    #[test]
    fn rejects_zero_grain_size() {
        let mut h = valid_header();
        h[20..28].copy_from_slice(&0u64.to_le_bytes());
        let err = SparseHeader::parse(&h).unwrap_err();
        assert!(matches!(err, Error::Corrupt(_)), "got {err:?}");
    }

    #[test]
    fn rejects_zero_num_gtes_per_gt() {
        let mut h = valid_header();
        h[44..48].copy_from_slice(&0u32.to_le_bytes());
        let err = SparseHeader::parse(&h).unwrap_err();
        assert!(matches!(err, Error::Corrupt(_)), "got {err:?}");
    }

    #[test]
    fn rejects_compressed_images() {
        // compress_algorithm @ 77..79; 1 = DEFLATE (streamOptimized).
        let mut h = valid_header();
        h[77..79].copy_from_slice(&1u16.to_le_bytes());
        match SparseHeader::parse(&h).unwrap_err() {
            Error::Unsupported(_) => {}
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn exposes_descriptor_and_directory_offsets() {
        let p = SparseHeader::parse(&valid_header()).unwrap();
        assert_eq!(p.descriptor_offset, 1);
        assert_eq!(p.descriptor_size, 20);
        assert_eq!(p.over_head, 7);
        assert_eq!(p.compress_algorithm, 0);
    }
}
