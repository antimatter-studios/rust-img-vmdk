//! VMDK read + write path. Currently handles the **monolithic sparse**
//! variant (single-file VMDK with embedded descriptor + grain directory +
//! grain tables). Other variants are reported as
//! [`Error::Unsupported`](crate::Error::Unsupported) so the caller can
//! either fall back or surface a clear message.
//!
//! ## Backing storage
//!
//! The reader is generic over [`fs_core::BlockDevice`]. Open from a path
//! via [`VmdkReader::open`] / [`VmdkReader::open_rw`] (the file is
//! wrapped in a [`fs_core::FileDevice`] internally), or hand in any
//! other `BlockDevice` via [`VmdkReader::open_on_device`] /
//! [`VmdkReader::open_rw_on_device`]. The on-device variants are how the
//! VMDK layer stacks on top of a host-supplied block resource (FSKit
//! `FSBlockDeviceResource`, slice reader, etc.).
//!
//! Implements [`fs_core::BlockRead`] and [`fs_core::BlockDevice`] so a
//! `VmdkReader` can be handed straight to a partition probe, a
//! filesystem driver, or any other consumer of those traits — and
//! exposed as a generic [`fs_core::ffi::FsCoreDevice`] handle through
//! the C ABI.
//!
//! ## Write path (monolithicSparse only)
//!
//! Writes mutate grain tables in place: a write into a sparse grain
//! allocates a fresh grain at the device tail, points the grain table
//! entry at it, then lands the data. If the grain's grain-table cluster
//! itself isn't allocated yet the table is allocated (and the GD updated)
//! before the grain is allocated. Crash-safety order:
//!
//!   grain data → grain-table entry → grain-directory entry (when growing)
//!   → device flush
//!
//! with `dev.flush()` between each step. A crash mid-allocation may leak
//! a grain or grain table but never produces a wrong-data read.

use crate::descriptor::Descriptor;
use crate::error::{Error, Result};
use crate::header::{SparseHeader, HEADER_SIZE};
use fs_core::{BlockDevice, FileDevice};
use std::path::Path;
use std::sync::{Arc, Mutex};

const SECTOR_SIZE: u64 = 512;

pub struct VmdkReader {
    /// Backing block device. All host-offset reads/writes go through here.
    /// `Arc<dyn BlockDevice>` because `BlockDevice` is `Send + Sync` and
    /// the reader may live behind an `Arc` itself (FFI handles).
    dev: Arc<dyn BlockDevice>,
    header: SparseHeader,
    /// Cached primary grain directory (one u32 per grain table). Always
    /// small — `ceil(capacity / (grain_size * num_gtes_per_gt))` entries.
    /// Mutex-wrapped because the writer mutates entries in place when
    /// allocating a new grain table.
    gd: Mutex<Vec<u32>>,
    /// Single-slot grain-table cache. Loading a GT means a 2 KiB
    /// (512 entries × 4 bytes) read; caching the most-recent table
    /// keeps sequential reads cheap without holding all GTs resident.
    gt_cache: Mutex<GtCache>,
    /// Virtual disk size in bytes (`capacity * 512`).
    virtual_size: u64,
    /// True when the image was opened read-write.
    writable: bool,
    /// Allocation cursor in sectors — the next free sector at the tail
    /// of the backing device. Initialised to the ceil of the device's
    /// reported size at open time and bumped as grains/tables are
    /// allocated. Mutex-wrapped because allocation has to be serialised.
    alloc_cursor: Mutex<u64>,
}

struct GtCache {
    /// Index into `gd` of the table currently held; `usize::MAX` if empty.
    loaded_idx: usize,
    entries: Vec<u32>,
}

impl VmdkReader {
    /// Open `path` read-only and parse the sparse header + descriptor +
    /// grain directory. Internally wraps the file in a
    /// [`fs_core::FileDevice`].
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let dev = FileDevice::open(path.as_ref()).map_err(fs_core_to_vmdk_error)?;
        Self::open_inner(Arc::new(dev), false)
    }

    /// Open `path` read-write. Errors if the path isn't writable.
    pub fn open_rw<P: AsRef<Path>>(path: P) -> Result<Self> {
        let dev = FileDevice::open_rw(path.as_ref()).map_err(fs_core_to_vmdk_error)?;
        Self::open_inner(Arc::new(dev), true)
    }

    /// Open read-only on top of an arbitrary [`BlockDevice`]. Used when
    /// the caller already holds a device handle (FSKit-supplied block
    /// resource, slice adapter, etc.) and wants the VMDK layer to sit
    /// on top of it.
    pub fn open_on_device(dev: Arc<dyn BlockDevice>) -> Result<Self> {
        Self::open_inner(dev, false)
    }

    /// Open read-write on top of an arbitrary [`BlockDevice`]. The
    /// device must report `is_writable()`; otherwise the call returns
    /// [`Error::ReadOnly`].
    pub fn open_rw_on_device(dev: Arc<dyn BlockDevice>) -> Result<Self> {
        if !dev.is_writable() {
            return Err(Error::ReadOnly);
        }
        Self::open_inner(dev, true)
    }

    fn open_inner(dev: Arc<dyn BlockDevice>, writable: bool) -> Result<Self> {
        let dev_size = dev.size_bytes();
        if dev_size < HEADER_SIZE as u64 {
            return Err(Error::Corrupt("device shorter than 512 bytes"));
        }

        // Sparse header at sector 0.
        let mut hdr_bytes = [0u8; HEADER_SIZE];
        dev.read_at(0, &mut hdr_bytes)
            .map_err(fs_core_to_vmdk_error)?;
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
        if desc_byte_off + desc_byte_len > dev_size {
            return Err(Error::Corrupt("descriptor extends past EOF"));
        }
        let mut desc_bytes = vec![0u8; desc_byte_len as usize];
        dev.read_at(desc_byte_off, &mut desc_bytes)
            .map_err(fs_core_to_vmdk_error)?;
        let desc_text =
            std::str::from_utf8(&desc_bytes).map_err(|_| Error::Corrupt("descriptor not UTF-8"))?;
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
        if (gd_byte_off + gd_byte_len as u64) > dev_size {
            return Err(Error::Corrupt("grain directory extends past EOF"));
        }

        let mut gd_bytes = vec![0u8; gd_byte_len];
        dev.read_at(gd_byte_off, &mut gd_bytes)
            .map_err(fs_core_to_vmdk_error)?;

        let mut gd = Vec::with_capacity(gt_count as usize);
        for chunk in gd_bytes.chunks_exact(4) {
            gd.push(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }

        let virtual_size = header
            .capacity
            .checked_mul(SECTOR_SIZE)
            .ok_or(Error::Corrupt("capacity*512 overflow"))?;

        // Allocation cursor: round the device's current size up to the
        // next sector. Newly allocated grains/tables land at the tail.
        let alloc_cursor = dev_size.div_ceil(SECTOR_SIZE);

        Ok(Self {
            dev,
            header,
            gd: Mutex::new(gd),
            gt_cache: Mutex::new(GtCache {
                loaded_idx: usize::MAX,
                entries: Vec::new(),
            }),
            virtual_size,
            writable,
            alloc_cursor: Mutex::new(alloc_cursor),
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

    /// Whether the image was opened read-write.
    pub fn is_writable(&self) -> bool {
        self.writable
    }

    // -- internal device adapters ------------------------------------------

    fn dev_read(&self, off: u64, buf: &mut [u8]) -> Result<()> {
        self.dev.read_at(off, buf).map_err(fs_core_to_vmdk_error)
    }

    fn dev_write(&self, off: u64, buf: &[u8]) -> Result<()> {
        self.dev.write_at(off, buf).map_err(fs_core_to_vmdk_error)
    }

    fn dev_flush(&self) -> Result<()> {
        self.dev.flush().map_err(fs_core_to_vmdk_error)
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
            let chunk_len = std::cmp::min(bytes_remaining_in_grain, end - cursor) as usize;

            let dst = &mut buf[written..written + chunk_len];

            let gt_sector = {
                let gd = self.gd.lock().unwrap();
                if gt_idx >= gd.len() {
                    return Err(Error::Corrupt("gt_idx past grain directory"));
                }
                gd[gt_idx]
            };

            if gt_sector == 0 {
                // Whole grain table unallocated — region reads as zero.
                dst.fill(0);
            } else {
                let grain_sector = self.lookup_grain(gt_idx, gte_idx, gt_sector)?;
                if grain_sector == 0 {
                    dst.fill(0);
                } else {
                    let host_off = (grain_sector as u64) * SECTOR_SIZE + in_grain;
                    self.dev_read(host_off, dst)?;
                }
            }

            cursor += chunk_len as u64;
            written += chunk_len;
        }

        Ok(())
    }

    /// Resolve `gt[gte_idx]`, loading the grain table from disk if it
    /// isn't the one currently cached.
    fn lookup_grain(&self, gt_idx: usize, gte_idx: usize, gt_sector: u32) -> Result<u32> {
        let entries_per_gt = self.header.num_gtes_per_gt as usize;
        let mut cache = self.gt_cache.lock().unwrap();
        if cache.loaded_idx != gt_idx {
            let mut bytes = vec![0u8; entries_per_gt * 4];
            let off = (gt_sector as u64) * SECTOR_SIZE;
            self.dev_read(off, &mut bytes)?;
            let mut entries = Vec::with_capacity(entries_per_gt);
            for chunk in bytes.chunks_exact(4) {
                entries.push(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
            }
            cache.entries = entries;
            cache.loaded_idx = gt_idx;
        }
        if gte_idx >= cache.entries.len() {
            return Err(Error::Corrupt("gte_idx past grain table"));
        }
        Ok(cache.entries[gte_idx])
    }

    /// Write to the image. Behaviour by grain state:
    ///
    /// - **Allocated grain**: direct write at the host offset.
    /// - **Sparse grain (gt[gte] == 0) inside an allocated GT**: allocate
    ///   a fresh grain at the device tail, zero-pad it, write the user
    ///   payload at the in-grain offset, then update the GT entry.
    /// - **Sparse grain whose GT is itself unallocated**: allocate a new
    ///   grain table (zero-filled), then a new grain, then update the
    ///   GT entry, then publish the GT into the GD.
    ///
    /// Crash-safety order:
    ///   grain data → grain-table entry → grain-directory entry (when
    ///   growing) → flush, with `dev.flush()` between each step.
    pub fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        if !self.writable {
            return Err(Error::ReadOnly);
        }
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

        let mut cursor = offset;
        let mut written: usize = 0;

        while cursor < end {
            let in_grain = cursor % grain_bytes;
            let grain_idx = cursor / grain_bytes;
            let gt_idx = (grain_idx / entries_per_gt) as usize;
            let gte_idx = (grain_idx % entries_per_gt) as usize;

            let bytes_remaining_in_grain = grain_bytes - in_grain;
            let chunk_len = std::cmp::min(bytes_remaining_in_grain, end - cursor) as usize;
            let src = &buf[written..written + chunk_len];

            // Resolve current GT pointer. When zero we have to allocate
            // a fresh grain table before the grain itself.
            let gt_sector = {
                let gd = self.gd.lock().unwrap();
                if gt_idx >= gd.len() {
                    return Err(Error::Corrupt("gt_idx past grain directory"));
                }
                gd[gt_idx]
            };

            let gt_sector = if gt_sector == 0 {
                self.allocate_grain_table(gt_idx)?
            } else {
                gt_sector
            };

            // Now look up (or allocate) the grain inside this GT.
            let grain_sector = self.lookup_grain(gt_idx, gte_idx, gt_sector)?;
            if grain_sector == 0 {
                // Sparse grain: allocate, zero-pad, write payload, then
                // publish the GT entry.
                let new_grain_sector = self.allocate_grain()?;
                if in_grain != 0 || (chunk_len as u64) < grain_bytes {
                    // Partial-grain write: zero-init the whole grain
                    // first so the unwritten head/tail reads as zero
                    // (the spec's "absent → zero" semantics carry over
                    // to a freshly allocated grain).
                    let zeros = vec![0u8; grain_bytes as usize];
                    self.dev_write((new_grain_sector as u64) * SECTOR_SIZE, &zeros)?;
                }
                self.dev_write((new_grain_sector as u64) * SECTOR_SIZE + in_grain, src)?;
                self.dev_flush()?;
                // Step 2: publish the GT entry.
                self.update_gt_entry(gt_idx, gte_idx, gt_sector, new_grain_sector)?;
            } else {
                // Allocated grain: write through.
                let host_off = (grain_sector as u64) * SECTOR_SIZE + in_grain;
                self.dev_write(host_off, src)?;
                self.dev_flush()?;
            }

            cursor += chunk_len as u64;
            written += chunk_len;
        }
        Ok(())
    }

    /// Flush writes to stable storage. No-op for read-only images.
    pub fn flush(&self) -> Result<()> {
        if !self.writable {
            return Ok(());
        }
        self.dev_flush()
    }

    /// Allocate `n_sectors` worth of host space at the device tail and
    /// return the starting sector. The cursor is bumped under lock so
    /// concurrent allocations don't collide.
    fn allocate_sectors(&self, n_sectors: u64) -> Result<u64> {
        let mut cur = self.alloc_cursor.lock().unwrap();
        let start = *cur;
        let new_end = start
            .checked_add(n_sectors)
            .ok_or(Error::Corrupt("alloc_cursor overflow"))?;
        // Sanity: cap at u32::MAX-1 since GT/GD entries are 32-bit
        // sector numbers. Real images never come close, but guard
        // anyway so we error cleanly instead of producing a bad entry.
        if new_end > (u32::MAX as u64) {
            return Err(Error::Unsupported(
                "image grew past u32 sector addressable range",
            ));
        }
        *cur = new_end;
        Ok(start)
    }

    /// Allocate one fresh grain at the device tail and return its
    /// starting sector. Caller is responsible for writing the grain
    /// data and updating the grain table entry.
    fn allocate_grain(&self) -> Result<u32> {
        let n = self.header.grain_size;
        let s = self.allocate_sectors(n)?;
        Ok(s as u32)
    }

    /// Allocate a fresh, zero-filled grain table at the device tail and
    /// publish the new GT pointer into the in-memory + on-disk grain
    /// directory. Returns the sector number of the new GT.
    ///
    /// Crash-safety order: zero-init GT data → flush → publish GD entry
    /// → flush. A crash mid-sequence may leak a grain table but never
    /// produces a wrong-data read (an absent GD entry reads as zeros).
    fn allocate_grain_table(&self, gt_idx: usize) -> Result<u32> {
        let entries_per_gt = self.header.num_gtes_per_gt as u64;
        // Each entry is 4 bytes; round up to whole sectors.
        let gt_bytes = entries_per_gt * 4;
        let gt_sectors = gt_bytes.div_ceil(SECTOR_SIZE);
        let new_gt_sector_u64 = self.allocate_sectors(gt_sectors)?;
        if new_gt_sector_u64 > u32::MAX as u64 {
            return Err(Error::Unsupported("grain table sector past u32 range"));
        }
        let new_gt_sector = new_gt_sector_u64 as u32;

        // Step 1: zero-init the new GT on disk.
        let zeros = vec![0u8; (gt_sectors * SECTOR_SIZE) as usize];
        self.dev_write(new_gt_sector_u64 * SECTOR_SIZE, &zeros)?;
        self.dev_flush()?;

        // Step 2: publish the GD entry. Update on-disk GD slot then
        // mirror in-memory; hold the lock across both so a concurrent
        // reader never sees a memory/disk mismatch.
        {
            let mut gd = self.gd.lock().unwrap();
            if gt_idx >= gd.len() {
                return Err(Error::Corrupt("gt_idx past grain directory"));
            }
            let gd_entry_off = self.header.gd_offset * SECTOR_SIZE + (gt_idx as u64) * 4;
            let bytes = new_gt_sector.to_le_bytes();
            self.dev_write(gd_entry_off, &bytes)?;
            self.dev_flush()?;
            gd[gt_idx] = new_gt_sector;
        }

        // Invalidate the GT cache slot if it happened to hold this index
        // (it can't have meaningful contents — the GT was just zeroed —
        // but being defensive avoids a stale-cache surprise).
        let mut cache = self.gt_cache.lock().unwrap();
        if cache.loaded_idx == gt_idx {
            cache.loaded_idx = usize::MAX;
            cache.entries.clear();
        }

        Ok(new_gt_sector)
    }

    /// Overwrite a single grain-table entry on disk and refresh the
    /// in-memory cache if it currently holds this GT.
    fn update_gt_entry(
        &self,
        gt_idx: usize,
        gte_idx: usize,
        gt_sector: u32,
        new_grain_sector: u32,
    ) -> Result<()> {
        let entry_off = (gt_sector as u64) * SECTOR_SIZE + (gte_idx as u64) * 4;
        let bytes = new_grain_sector.to_le_bytes();
        self.dev_write(entry_off, &bytes)?;
        self.dev_flush()?;

        let mut cache = self.gt_cache.lock().unwrap();
        if cache.loaded_idx == gt_idx && gte_idx < cache.entries.len() {
            cache.entries[gte_idx] = new_grain_sector;
        }
        Ok(())
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

impl fs_core::BlockDevice for VmdkReader {
    fn write_at(&self, offset: u64, buf: &[u8]) -> fs_core::Result<()> {
        VmdkReader::write_at(self, offset, buf).map_err(vmdk_to_fs_core_error)
    }

    fn flush(&self) -> fs_core::Result<()> {
        VmdkReader::flush(self).map_err(vmdk_to_fs_core_error)
    }

    fn is_writable(&self) -> bool {
        VmdkReader::is_writable(self)
    }
}

fn vmdk_to_fs_core_error(e: Error) -> fs_core::Error {
    match e {
        Error::Io(io) => fs_core::Error::Io(io),
        Error::OutOfBounds { offset, len, size } => {
            fs_core::Error::OutOfBounds { offset, len, size }
        }
        Error::ReadOnly => fs_core::Error::ReadOnly,
        other => fs_core::Error::Custom(other.to_string()),
    }
}

fn fs_core_to_vmdk_error(e: fs_core::Error) -> Error {
    match e {
        fs_core::Error::Io(io) => Error::Io(io),
        fs_core::Error::ShortRead { offset, want, got } => Error::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            format!("short read at {offset}: wanted {want} got {got}"),
        )),
        fs_core::Error::ReadOnly => Error::ReadOnly,
        fs_core::Error::OutOfBounds { offset, len, size } => {
            Error::OutOfBounds { offset, len, size }
        }
        fs_core::Error::Custom(s) => Error::Io(std::io::Error::other(s)),
    }
}
