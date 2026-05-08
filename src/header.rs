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

impl SparseHeader {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < HEADER_SIZE {
            return Err(Error::Corrupt("header shorter than 512 bytes"));
        }
        let magic = read_u32(bytes, 0);
        if magic != MAGIC {
            return Err(Error::NotVmdk);
        }
        let version = read_u32(bytes, 4);
        let flags = read_u32(bytes, 8);
        let capacity = read_u64(bytes, 12);
        let grain_size = read_u64(bytes, 20);
        let descriptor_offset = read_u64(bytes, 28);
        let descriptor_size = read_u64(bytes, 36);
        let num_gtes_per_gt = read_u32(bytes, 44);
        let rgd_offset = read_u64(bytes, 48);
        let gd_offset = read_u64(bytes, 56);
        let over_head = read_u64(bytes, 64);
        let unclean_shutdown = bytes[72];
        let compress_algorithm = read_u16(bytes, 77);

        if grain_size == 0 {
            return Err(Error::Corrupt("grain_size is zero"));
        }
        if num_gtes_per_gt == 0 {
            return Err(Error::Corrupt("num_gtes_per_gt is zero"));
        }
        if compress_algorithm != 0 {
            return Err(Error::Unsupported("compressed VMDK (compress_algorithm != 0)"));
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
        b[off], b[off + 1], b[off + 2], b[off + 3],
        b[off + 4], b[off + 5], b[off + 6], b[off + 7],
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
}
