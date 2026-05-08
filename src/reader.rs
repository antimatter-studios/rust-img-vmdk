//! VMDK read path. Currently handles the **monolithic sparse** variant
//! (single-file VMDK with embedded descriptor + grain directory + grain
//! tables). Other variants are reported as
//! [`Error::Unsupported`](crate::Error::Unsupported) so the caller can
//! either fall back or surface a clear message.
//!
//! Implements [`fs_core::BlockRead`] and [`fs_core::BlockDevice`] so a
//! `VmdkReader` can be handed straight to a partition probe, a
//! filesystem driver, or any other consumer of those traits — and
//! exposed as a generic [`fs_core::ffi::FsCoreDevice`] handle through
//! the C ABI.

use crate::descriptor::Descriptor;
use crate::error::{Error, Result};
use crate::header::{SparseHeader, HEADER_SIZE};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Mutex;

const SECTOR_SIZE: u64 = 512;

pub struct VmdkReader {
    file: Mutex<File>,
    header: SparseHeader,
    /// Cached primary grain directory (one u32 per grain table). Always
    /// small — `ceil(capacity / (grain_size * num_gtes_per_gt))` entries.
    gd: Vec<u32>,
    /// Single-slot grain-table cache. Loading a GT means a 2 KiB
    /// (512 entries × 4 bytes) read; caching the most-recent table
    /// keeps sequential reads cheap without holding all GTs resident.
    gt_cache: Mutex<GtCache>,
    /// Virtual disk size in bytes (`capacity * 512`).
    virtual_size: u64,
}

struct GtCache {
    /// Index into `gd` of the table currently held; `usize::MAX` if empty.
    loaded_idx: usize,
    entries: Vec<u32>,
}

impl VmdkReader {
    /// Open `path`, parse the sparse header + descriptor + grain
    /// directory.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::open_inner(path.as_ref())
    }

    fn open_inner(path: &Path) -> Result<Self> {
        let mut file = File::open(path)?;
        let file_len = file.metadata()?.len();
        if file_len < HEADER_SIZE as u64 {
            return Err(Error::Corrupt("file shorter than 512 bytes"));
        }

        // Sparse header at sector 0.
        let mut hdr_bytes = [0u8; HEADER_SIZE];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut hdr_bytes)?;
        let header = SparseHeader::parse(&hdr_bytes)?;

        // Descriptor must say monolithicSparse.
        if header.descriptor_offset == 0 || header.descriptor_size == 0 {
            return Err(Error::Unsupported(
                "no embedded descriptor (probably monolithicFlat or split)",
            ));
        }
        let desc_byte_off = header
            .descriptor_offset
            .checked_mul(SECTOR_SIZE)
            .ok_or(Error::Corrupt("descriptor_offset overflow"))?;
        let desc_byte_len = header
            .descriptor_size
            .checked_mul(SECTOR_SIZE)
            .ok_or(Error::Corrupt("descriptor_size overflow"))?;
        if desc_byte_off + desc_byte_len > file_len {
            return Err(Error::Corrupt("descriptor extends past EOF"));
        }
        let mut desc_bytes = vec![0u8; desc_byte_len as usize];
        file.seek(SeekFrom::Start(desc_byte_off))?;
        file.read_exact(&mut desc_bytes)?;
        let desc_text = std::str::from_utf8(&desc_bytes)
            .map_err(|_| Error::Corrupt("descriptor not UTF-8"))?;
        let _descriptor = Descriptor::parse(desc_text)?;

        // Primary grain directory.
        if header.gd_offset == 0 {
            return Err(Error::Corrupt("gd_offset is zero"));
        }

        let entries_per_gt = header.num_gtes_per_gt as u64;
        let grains_total = header.capacity.div_ceil(header.grain_size);
        let gt_count = grains_total.div_ceil(entries_per_gt);
        if gt_count > (u32::MAX as u64) {
            return Err(Error::Corrupt("grain directory too large"));
        }

        let gd_byte_off = header
            .gd_offset
            .checked_mul(SECTOR_SIZE)
            .ok_or(Error::Corrupt("gd_offset overflow"))?;
        let gd_byte_len = (gt_count * 4) as usize;
        if (gd_byte_off + gd_byte_len as u64) > file_len {
            return Err(Error::Corrupt("grain directory extends past EOF"));
        }

        let mut gd_bytes = vec![0u8; gd_byte_len];
        file.seek(SeekFrom::Start(gd_byte_off))?;
        file.read_exact(&mut gd_bytes)?;

        let mut gd = Vec::with_capacity(gt_count as usize);
        for chunk in gd_bytes.chunks_exact(4) {
            gd.push(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }

        let virtual_size = header
            .capacity
            .checked_mul(SECTOR_SIZE)
            .ok_or(Error::Corrupt("capacity*512 overflow"))?;

        Ok(Self {
            file: Mutex::new(file),
            header,
            gd,
            gt_cache: Mutex::new(GtCache {
                loaded_idx: usize::MAX,
                entries: Vec::new(),
            }),
            virtual_size,
        })
    }

    pub fn virtual_size(&self) -> u64 {
        self.virtual_size
    }

    pub fn grain_size_bytes(&self) -> u64 {
        self.header.grain_size * SECTOR_SIZE
    }

    pub fn header(&self) -> &SparseHeader {
        &self.header
    }

    /// Read exactly `buf.len()` bytes starting at virtual `offset`.
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let len = buf.len() as u64;
        if len == 0 {
            return Ok(());
        }
        let end = offset
            .checked_add(len)
            .ok_or(Error::Corrupt("offset+len overflow"))?;
        if end > self.virtual_size {
            return Err(Error::OutOfBounds {
                offset,
                len,
                size: self.virtual_size,
            });
        }

        let grain_bytes = self.grain_size_bytes();
        let entries_per_gt = self.header.num_gtes_per_gt as u64;

        // Walk grain by grain. Each grain either: (a) has a host
        // location (gd[gt] != 0 and gt[gte] != 0) → read straight from
        // disk, or (b) is unallocated → fill destination with zero.
        let mut cursor = offset;
        let mut written: usize = 0;

        while cursor < end {
            let in_grain = cursor % grain_bytes;
            let grain_idx = cursor / grain_bytes;
            let gt_idx = (grain_idx / entries_per_gt) as usize;
            let gte_idx = (grain_idx % entries_per_gt) as usize;

            let bytes_remaining_in_grain = grain_bytes - in_grain;
            let chunk_len =
                std::cmp::min(bytes_remaining_in_grain, end - cursor) as usize;

            let dst = &mut buf[written..written + chunk_len];

            if gt_idx >= self.gd.len() {
                return Err(Error::Corrupt("gt_idx past grain directory"));
            }
            let gt_sector = self.gd[gt_idx];

            if gt_sector == 0 {
                // Whole grain table unallocated — region reads as zero.
                dst.fill(0);
            } else {
                let grain_sector = self.lookup_grain(gt_idx, gte_idx, gt_sector)?;
                if grain_sector == 0 {
                    dst.fill(0);
                } else {
                    let host_off = (grain_sector as u64) * SECTOR_SIZE + in_grain;
                    let mut f = self.file.lock().unwrap();
                    f.seek(SeekFrom::Start(host_off))?;
                    f.read_exact(dst)?;
                }
            }

            cursor += chunk_len as u64;
            written += chunk_len;
        }

        Ok(())
    }

    /// Resolve `gt[gte_idx]`, loading the grain table from disk if it
    /// isn't the one currently cached.
    fn lookup_grain(
        &self,
        gt_idx: usize,
        gte_idx: usize,
        gt_sector: u32,
    ) -> Result<u32> {
        let entries_per_gt = self.header.num_gtes_per_gt as usize;
        let mut cache = self.gt_cache.lock().unwrap();
        if cache.loaded_idx != gt_idx {
            let mut bytes = vec![0u8; entries_per_gt * 4];
            let off = (gt_sector as u64) * SECTOR_SIZE;
            {
                let mut f = self.file.lock().unwrap();
                f.seek(SeekFrom::Start(off))?;
                f.read_exact(&mut bytes)?;
            }
            let mut entries = Vec::with_capacity(entries_per_gt);
            for chunk in bytes.chunks_exact(4) {
                entries.push(u32::from_le_bytes([
                    chunk[0], chunk[1], chunk[2], chunk[3],
                ]));
            }
            cache.entries = entries;
            cache.loaded_idx = gt_idx;
        }
        if gte_idx >= cache.entries.len() {
            return Err(Error::Corrupt("gte_idx past grain table"));
        }
        Ok(cache.entries[gte_idx])
    }
}

// ---------------------------------------------------------------------------
// fs_core::BlockRead / BlockDevice bridge
// ---------------------------------------------------------------------------

impl fs_core::BlockRead for VmdkReader {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        VmdkReader::read_at(self, offset, buf).map_err(vmdk_to_fs_core_error)
    }
    fn size_bytes(&self) -> u64 {
        self.virtual_size()
    }
}

/// Read-only currently; the default `Err(ReadOnly)` write path applies.
impl fs_core::BlockDevice for VmdkReader {}

fn vmdk_to_fs_core_error(e: Error) -> fs_core::Error {
    match e {
        Error::Io(io) => fs_core::Error::Io(io),
        Error::OutOfBounds { offset, len, size } => {
            fs_core::Error::OutOfBounds { offset, len, size }
        }
        other => fs_core::Error::Custom(other.to_string()),
    }
}
