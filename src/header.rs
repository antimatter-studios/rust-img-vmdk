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

/// The largest grain table this reader will accept, in entries.
///
/// `numGTEsPerGT` is 512 in every image the reference producers write —
/// 2 KiB of pointers. The field is a `u32` and the format puts no
/// ceiling on it, so an image can ask for a 16 GiB table, and two
/// separate paths size a buffer from it: the read path when it loads a
/// table, the write path when it creates one. The read path can bound
/// the load against the file the table lives in. The write path cannot,
/// because the table does not exist yet — which is how a 512-byte write
/// into a 68 KiB image came to allocate and zero-fill 64 MiB.
///
/// 262,144 entries is a 1 MiB table: 512 times what any producer writes,
/// and small enough that neither path can be turned into an allocation
/// weapon. A value above it is not describing a real image.
pub const MAX_GTES_PER_GT: u32 = 1 << 18;

/// `flags` bit 1 — **the image carries a redundant grain table**.
///
/// A second copy of the grain directory, and of every grain table, kept
/// at `rgd_offset`. It exists to be read when the primary is damaged, so
/// a writer that leaves it stale turns real data into a hole for exactly
/// the recovery it was there for. qemu and VMware set this bit on every
/// sparse extent they produce.
pub const FLAG_REDUNDANT_GRAIN_TABLE: u32 = 0x2;

/// `flags` bit 2 — **the image uses the zeroed-grain marker**.
///
/// When set, the grain-table entry [`GTE_ZEROED_GRAIN`] is a sentinel
/// rather than a sector number. Producers announce the convention here
/// because the sentinel is otherwise indistinguishable from a pointer,
/// and a reader that does not know about it will follow it.
pub const FLAG_ZEROED_GRAIN: u32 = 0x4;

/// The grain-table entry that means "this grain exists and is entirely
/// zero", valid only when [`FLAG_ZEROED_GRAIN`] is set in `flags`.
///
/// It is `1` — an ordinary-looking sector number, and specifically the
/// sector where a monolithic sparse VMDK keeps its embedded descriptor.
/// A reader that treats it as a pointer returns the descriptor's ASCII
/// as guest data; a writer that treats it as a pointer overwrites the
/// descriptor and the file stops being a VMDK.
pub const GTE_ZEROED_GRAIN: u32 = 1;

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
        // A GRAIN IN BYTES, NOT IN SECTORS.
        //
        // `grain_size` is a sector count and every use of it multiplies
        // by 512. Checking it against zero says nothing about the
        // product: any multiple of 2^55 multiplies out to exactly 2^64,
        // which is zero, and the byte size is then used as a divisor.
        //
        // Division by zero panics whatever the profile -- unlike an
        // overflow it does not depend on `overflow-checks`, which is off
        // in release here. So an unchecked product here is a panic in
        // the shipped library, out of a plain `read_at`.
        if grain_size.checked_mul(512).is_none_or(|bytes| bytes == 0) {
            return Err(Error::Corrupt(
                "grain_size in sectors does not fit in a byte count",
            ));
        }
        if num_gtes_per_gt == 0 {
            return Err(Error::Corrupt("num_gtes_per_gt is zero"));
        }
        if num_gtes_per_gt > MAX_GTES_PER_GT {
            return Err(Error::Corrupt(
                "grain table larger than any real image declares (num_gtes_per_gt out of range)",
            ));
        }
        if compress_algorithm != 0 {
            return Err(Error::Unsupported(
                "compressed VMDK (compress_algorithm != 0)",
            ));
        }

        // `flags` used to be parsed and never read, which is how the
        // zeroed-grain convention went unimplemented: the bit that
        // announces it was sitting in the struct the whole time.

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

    /// Whether this image uses the zeroed-grain marker — bit 2 of
    /// `flags`, [`FLAG_ZEROED_GRAIN`].
    ///
    /// In such an image a grain-table entry of [`GTE_ZEROED_GRAIN`]
    /// means "present, and entirely zero" rather than "at host sector
    /// 1". qemu sets the bit whenever an image is created with
    /// `zeroed_grain=on`, and writes the marker for every explicit zero
    /// write or discard the guest makes.
    pub fn uses_zeroed_grain_marker(&self) -> bool {
        self.flags & FLAG_ZEROED_GRAIN != 0
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

    /// The field is a `u32` with no ceiling in the format, and both the
    /// read path and the write path size a buffer from it. The write
    /// path is the one with no file to bound against, since the table it
    /// is creating does not exist yet.
    #[test]
    fn rejects_a_grain_table_larger_than_any_real_image() {
        let mut h = valid_header();
        h[44..48].copy_from_slice(&(MAX_GTES_PER_GT + 1).to_le_bytes());
        match SparseHeader::parse(&h).unwrap_err() {
            Error::Corrupt(msg) => assert!(msg.contains("grain table"), "got {msg:?}"),
            other => panic!("expected Corrupt, got {other:?}"),
        }

        // The cap itself is accepted: it is a bound, not a preference.
        let mut h = valid_header();
        h[44..48].copy_from_slice(&MAX_GTES_PER_GT.to_le_bytes());
        assert_eq!(
            SparseHeader::parse(&h).unwrap().num_gtes_per_gt,
            MAX_GTES_PER_GT
        );
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

    /// The `flags` word is what tells a reader that entry `1` is a
    /// sentinel and not a sector number, so the bit has to survive the
    /// parse and be legible afterwards. It used to do the first and not
    /// the second: `flags` was stored and no code ever read it.
    #[test]
    fn reports_whether_the_image_uses_the_zeroed_grain_marker() {
        let mut h = valid_header();
        assert!(!SparseHeader::parse(&h).unwrap().uses_zeroed_grain_marker());

        // 0x7 is what qemu writes for `zeroed_grain=on`: valid
        // newline-detection bit, redundant-GD bit, zeroed-grain bit.
        h[offsets::FLAGS..offsets::FLAGS + 4].copy_from_slice(&0x7u32.to_le_bytes());
        let p = SparseHeader::parse(&h).unwrap();
        assert_eq!(p.flags, 0x7);
        assert!(p.uses_zeroed_grain_marker());

        // Neighbouring bits must not be mistaken for it.
        h[offsets::FLAGS..offsets::FLAGS + 4].copy_from_slice(&0x3u32.to_le_bytes());
        assert!(!SparseHeader::parse(&h).unwrap().uses_zeroed_grain_marker());
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
