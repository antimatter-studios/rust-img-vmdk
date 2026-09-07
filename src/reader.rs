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
//!
//! ## The redundant copies
//!
//! A sparse extent carries a second grain directory at `rgd_offset` and
//! a second copy of every grain table, announced by bit 1 of `flags`.
//! Nothing on the read path consults them — the primary is what reads
//! follow — but they are what a recovery tool falls back to when the
//! primary is damaged, so every directory and table write is mirrored
//! into them.
//!
//! **The redundant copy is written first**, both for a directory entry
//! and for a table entry, following VMware's convention. A crash between
//! the two writes then leaves the primary *behind* the redundant copy
//! rather than ahead of it, so the fallback never claims a grain the
//! primary has not got.
//!
//! An image whose redundant directory does not fit inside the file is
//! still opened read-only, since reads do not need it. `open_rw` refuses
//! it: a write would leave a copy we cannot locate saying the wrong
//! thing, and `qemu-img check` does not look at the redundant tables, so
//! nothing downstream would report it.

use crate::descriptor::Descriptor;
use crate::error::{Error, Result};
use crate::header::{SparseHeader, GTE_ZEROED_GRAIN, HEADER_SIZE};
use fs_core::{BlockDevice, FileDevice};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Bytes per sector. `pub` because the test fixtures build images in
/// sector units and were each redeclaring their own copy.
pub const SECTOR_SIZE: u64 = 512;

/// Bytes per grain-directory and grain-table entry.
///
/// Both tables are arrays of little-endian `u32` sector numbers, so the
/// same 4 governs the length of a GD, the length of a GT, and the offset
/// of any entry within either. It appeared as a bare literal at six
/// sites, where it is indistinguishable from an unrelated 4.
const GD_GT_ENTRY_SIZE: u64 = 4;

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
    /// Cached redundant grain directory, when the image declares one
    /// (`rgd_offset != 0`). `None` for an image without a redundant
    /// copy, and for a read-only open of an image whose redundant
    /// directory does not fit the file.
    ///
    /// Reads never consult it. It exists so that writes can keep it in
    /// step with the primary: it is the copy a recovery tool falls back
    /// to, and a stale one turns real data into a hole.
    rgd: Option<Mutex<Vec<u32>>>,
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
    /// Serialises the whole allocate-and-publish sequence.
    ///
    /// Every individual structure here has its own lock, and that was
    /// not enough: allocating a grain means *testing* an entry, then
    /// allocating, then *publishing* — and with each lock taken and
    /// dropped in between, two writers could both read the same absent
    /// entry, both allocate, and the second could overwrite the first's
    /// published pointer. Both calls returned `Ok(())` and one writer's
    /// bytes were left in a grain nothing referenced.
    ///
    /// Writing through to a grain that already exists does not take this
    /// lock, so writers only serialise where they must: an entry that
    /// names a sector never changes afterwards, because grains are never
    /// relocated.
    allocation: Mutex<()>,
    /// First byte a grain may legally occupy: everything below it is
    /// the image's own metadata. See [`VmdkReader::grain_host_offset`].
    first_grain_byte: u64,
}

/// What a grain-table entry says about its grain.
///
/// The reader used to spell this out as `entry == 0` at one site and
/// the writer as `entry == 0` at another, and neither knew that `0` is
/// not the only entry that means "no data here". Naming the three
/// states puts the format's vocabulary in one place, so a reader and a
/// writer cannot disagree about what an entry means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GrainState {
    /// No grain: nothing is allocated for this range.
    Unallocated,
    /// A grain exists and is entirely zero — the zeroed-grain marker.
    /// Reads as zeros; a write into it must allocate, exactly as
    /// [`GrainState::Unallocated`] does.
    Zeroed,
    /// A grain at this host sector.
    At(u32),
}

struct GtCache {
    /// Which grain table is in `entries`, or `None` if nothing is —
    /// identified by its directory index *and* the sector it was loaded
    /// from.
    ///
    /// The index alone is not an identity. A grain table can be
    /// allocated afresh for an index that already had one, and a cache
    /// keyed only on the index will then hand back, or accept an update
    /// into, the contents of a table the directory no longer points at.
    /// Two values, both of which must match, make a stale slot a miss
    /// rather than a wrong answer.
    ///
    /// (Was `usize::MAX` as a sentinel for "empty", which every read had
    /// to know about and no type enforced.)
    loaded: Option<(usize, u32)>,
    entries: Vec<u32>,
}

/// A byte offset resolved into the grain hierarchy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GrainAddress {
    /// Offset within the grain.
    in_grain: u64,
    /// Index of the grain table in the grain directory.
    gt_idx: usize,
    /// Index of the grain within that table.
    gte_idx: usize,
    /// How many bytes of the caller's range this grain can serve.
    chunk_len: usize,
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

        // WHAT KIND OF FILE IS THIS AT ALL.
        //
        // A monolithicSparse image is a sparse extent: `KDMV` at byte 0,
        // its descriptor embedded a sector in. A flat or split image is
        // not — the file a user is handed is a few hundred bytes of
        // plain text naming the extents that hold the data, with no
        // magic anywhere in it.
        //
        // Committing to the sparse-header parse first meant those images
        // failed on the magic, or on being shorter than a header, and
        // came back as "not a VMDK" or "corrupt". Both are false, and
        // false in the direction that hurts: a caller probing a file
        // against several container readers takes them as permission to
        // move on, and tells the user their VMDK is unrecognised. The
        // create type is right there in the text, and naming it is what
        // tells them which conversion to run.
        if !looks_like_a_sparse_extent(&dev, dev_size)? {
            return Err(describe_descriptor_file(&dev, dev_size));
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
        // Checked, like the two multiplications above it. Both operands
        // are attacker-supplied sector counts scaled to bytes, so the sum
        // can overflow even when neither product did — and an overflowing
        // sum wraps to a small number that passes the EOF test.
        let desc_end = desc_byte_off
            .checked_add(desc_byte_len)
            .ok_or(Error::Corrupt("descriptor extent overflow"))?;
        if desc_end > dev_size {
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
        let gd_byte_len = (gt_count * GD_GT_ENTRY_SIZE) as usize;
        let gd_end = gd_byte_off
            .checked_add(gd_byte_len as u64)
            .ok_or(Error::Corrupt("grain-directory extent overflow"))?;
        if gd_end > dev_size {
            return Err(Error::Corrupt("grain directory extends past EOF"));
        }

        let gd = read_directory(&dev, gd_byte_off, gd_byte_len)?;

        // The redundant grain directory, when the image has one.
        //
        // It is a second copy of the directory and of every table,
        // announced by bit 1 of `flags`, and every sparse extent qemu
        // and VMware produce carries it. Nothing on the read path
        // consults it — the primary is what reads follow — so an image
        // whose redundant directory does not fit the file is still
        // perfectly readable and is opened read-only without complaint.
        //
        // A read-write open is different. Writing to such an image means
        // updating a copy we cannot locate, and an image whose two
        // copies disagree passes `qemu-img check` — which does not look
        // at the redundant tables — while a tool that falls back to them
        // reads a hole where the data is. Refusing the write is the only
        // answer that is visible to anyone.
        let redundant_gd = if header.rgd_offset == 0 {
            None
        } else {
            let rgd = header
                .rgd_offset
                .checked_mul(SECTOR_SIZE)
                .ok_or(Error::Corrupt("redundant grain directory offset overflows"))
                .and_then(|off| {
                    let end = off
                        .checked_add(gd_byte_len as u64)
                        .ok_or(Error::Corrupt("redundant grain-directory extent overflow"))?;
                    if end > dev_size {
                        return Err(Error::Corrupt("redundant grain directory extends past EOF"));
                    }
                    read_directory(&dev, off, gd_byte_len)
                });
            match rgd {
                Ok(entries) => Some(Mutex::new(entries)),
                Err(e) if writable => return Err(e),
                Err(_) => None,
            }
        };

        let virtual_size = header
            .capacity
            .checked_mul(SECTOR_SIZE)
            .ok_or(Error::Corrupt("capacity*512 overflow"))?;

        // Allocation cursor: round the device's current size up to the
        // next sector. Newly allocated grains/tables land at the tail.
        let alloc_cursor = dev_size.div_ceil(SECTOR_SIZE);

        // WHERE THE GRAINS BEGIN.
        //
        // A grain-table entry is an unsigned sector number with no
        // reserved range, so a corrupt or hostile table can point a
        // grain at the header, the descriptor or a directory and the
        // read comes back as plausible bytes. The grain *table* pointer
        // has been bounds-checked since `lookup_grain` was written; the
        // grain pointer never was, and "one past EOF errors anyway" is
        // what made the omission look like a check.
        //
        // Every region below is metadata the format puts in front of the
        // grains, so their end is a floor no grain may sit under. Each
        // is folded in only when its own extent fits the device: a
        // nonsense `rgd_offset` should not make an otherwise readable
        // image unreadable, and the redundant directory is not consulted
        // on the read path at all.
        let dev_sectors = dev_size / SECTOR_SIZE;
        let gd_sectors = (gd_byte_len as u64).div_ceil(SECTOR_SIZE);
        let fits = |start: u64, len: u64| -> Option<u64> {
            let end = start.checked_add(len)?;
            (end <= dev_sectors).then_some(end)
        };
        let mut first_grain_sector = 1; // sector 0 is the sparse header
        for end in [
            fits(header.descriptor_offset, header.descriptor_size),
            fits(header.gd_offset, gd_sectors),
            (header.rgd_offset != 0)
                .then(|| fits(header.rgd_offset, gd_sectors))
                .flatten(),
            // `over_head` is the format's own statement of how many
            // sectors of metadata precede the grains. It is the only one
            // of these that also covers the grain *tables*, whose
            // positions are otherwise only discoverable one directory
            // entry at a time.
            fits(header.over_head, 0),
        ]
        .into_iter()
        .flatten()
        {
            first_grain_sector = first_grain_sector.max(end);
        }
        let first_grain_byte = first_grain_sector * SECTOR_SIZE;

        Ok(Self {
            dev,
            header,
            gd: Mutex::new(gd),
            rgd: redundant_gd,
            gt_cache: Mutex::new(GtCache {
                loaded: None,
                entries: Vec::new(),
            }),
            virtual_size,
            writable,
            alloc_cursor: Mutex::new(alloc_cursor),
            allocation: Mutex::new(()),
            first_grain_byte,
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

        // Walk grain by grain. Each grain either: (a) has a host
        // location (gd[gt] != 0 and gt[gte] != 0) → read straight from
        // disk, or (b) is unallocated → fill destination with zero.
        let mut cursor = offset;
        let mut written: usize = 0;

        while cursor < end {
            let GrainAddress {
                in_grain,
                gt_idx,
                gte_idx,
                chunk_len,
            } = self.grain_address(cursor, end);
            let dst = &mut buf[written..written + chunk_len];
            let gt_sector = self.gd_entry(gt_idx)?;

            if gt_sector == 0 {
                // Whole grain table unallocated — region reads as zero.
                dst.fill(0);
            } else {
                let entry = self.lookup_grain(gt_idx, gte_idx, gt_sector)?;
                match self.grain_state(entry) {
                    GrainState::Unallocated | GrainState::Zeroed => dst.fill(0),
                    GrainState::At(grain_sector) => {
                        let host_off =
                            self.grain_host_offset(grain_sector, in_grain, chunk_len as u64)?;
                        self.dev_read(host_off, dst)?;
                    }
                }
            }

            cursor += chunk_len as u64;
            written += chunk_len;
        }

        Ok(())
    }

    /// Where a byte offset falls in the grain hierarchy, and how much
    /// of the caller's range this grain can serve.
    ///
    /// `read_at` and `write_at` each computed these five values the
    /// same way, then diverged — the reader zero-fills an unallocated
    /// grain, the writer allocates one. The divergence is the whole
    /// point of the two functions; the arithmetic in front of it is not,
    /// and two copies of an index calculation is how one of them ends up
    /// off by a level.
    fn grain_address(&self, cursor: u64, end: u64) -> GrainAddress {
        let grain_bytes = self.grain_size_bytes();
        let entries_per_gt = self.header.num_gtes_per_gt as u64;
        let in_grain = cursor % grain_bytes;
        let grain_idx = cursor / grain_bytes;
        GrainAddress {
            in_grain,
            gt_idx: (grain_idx / entries_per_gt) as usize,
            gte_idx: (grain_idx % entries_per_gt) as usize,
            chunk_len: std::cmp::min(grain_bytes - in_grain, end - cursor) as usize,
        }
    }

    /// What a raw grain-table entry means in *this* image.
    ///
    /// [`GTE_ZEROED_GRAIN`] is a sentinel only in an image whose header
    /// says so. Elsewhere it is an ordinary sector number that happens
    /// to address the embedded descriptor, and
    /// [`Self::grain_host_offset`] refuses it on that basis rather than
    /// this one — a reader should not have to guess which of the two an
    /// unannounced `1` was meant to be.
    fn grain_state(&self, entry: u32) -> GrainState {
        match entry {
            0 => GrainState::Unallocated,
            GTE_ZEROED_GRAIN if self.header.uses_zeroed_grain_marker() => GrainState::Zeroed,
            sector => GrainState::At(sector),
        }
    }

    /// The host byte offset of `len` bytes at `in_grain` inside the
    /// grain starting at `grain_sector`, refusing a pointer that lands
    /// in the image's own metadata or past its end.
    ///
    /// Only the bytes actually being touched are required to be inside
    /// the file: an image whose final grain is truncated is still
    /// readable up to where it stops, which is how a sparse tail behaves
    /// and is not a reason to refuse the whole image.
    fn grain_host_offset(&self, grain_sector: u32, in_grain: u64, len: u64) -> Result<u64> {
        let start = (grain_sector as u64) * SECTOR_SIZE;
        if start < self.first_grain_byte {
            return Err(Error::Corrupt(
                "grain pointer lands inside the image's metadata",
            ));
        }
        let off = start + in_grain;
        let end = off
            .checked_add(len)
            .ok_or(Error::Corrupt("grain extent overflows"))?;
        if end > self.file_extent() {
            return Err(Error::Corrupt("grain extends past EOF"));
        }
        Ok(off)
    }

    /// The grain directory's entry for `gt_idx`, bounds-checked.
    ///
    /// Zero means the whole grain table is unallocated — which the
    /// reader answers with zeros and the writer answers by allocating.
    fn gd_entry(&self, gt_idx: usize) -> Result<u32> {
        let gd = self.gd.lock().unwrap();
        if gt_idx >= gd.len() {
            return Err(Error::Corrupt("gt_idx past grain directory"));
        }
        Ok(gd[gt_idx])
    }

    /// Resolve `gt[gte_idx]`, loading the grain table from disk if it
    /// isn't the one currently cached.
    fn lookup_grain(&self, gt_idx: usize, gte_idx: usize, gt_sector: u32) -> Result<u32> {
        let entries_per_gt = self.header.num_gtes_per_gt as usize;
        let mut cache = self.gt_cache.lock().unwrap();
        if cache.loaded != Some((gt_idx, gt_sector)) {
            // The grain directory is required to fit inside the file
            // where it is read, twenty lines from here. A grain table
            // is the same kind of thing -- a run of 4-byte entries at a
            // sector the header names. `num_gtes_per_gt` is capped at
            // parse time (`header::MAX_GTES_PER_GT`), so the length is
            // bounded before it gets here; this check is what stops a
            // table that fits the cap but not the file it claims to
            // live in.
            let off = (gt_sector as u64) * SECTOR_SIZE;
            let len = (entries_per_gt as u64)
                .checked_mul(GD_GT_ENTRY_SIZE)
                .ok_or(Error::Corrupt("grain table size overflows"))?;
            let end = off
                .checked_add(len)
                .ok_or(Error::Corrupt("grain table extends past EOF"))?;
            if end > self.file_extent() {
                return Err(Error::Corrupt("grain table extends past EOF"));
            }
            let mut bytes = vec![0u8; len as usize];
            self.dev_read(off, &mut bytes)?;
            let mut entries = Vec::with_capacity(entries_per_gt);
            for chunk in bytes.chunks_exact(4) {
                entries.push(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
            }
            cache.entries = entries;
            cache.loaded = Some((gt_idx, gt_sector));
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

        let mut cursor = offset;
        let mut written: usize = 0;

        while cursor < end {
            let addr = self.grain_address(cursor, end);
            let src = &buf[written..written + addr.chunk_len];

            // Fast path: a grain that already exists is written through
            // without taking the allocation lock, so writers into
            // populated regions do not serialise with each other. Safe
            // to decide outside the lock because a grain-table entry
            // that names a sector never changes again — a grain is never
            // relocated once published.
            match self.backing_sector(addr.gt_idx, addr.gte_idx)? {
                Some(grain_sector) => self.write_through(grain_sector, &addr, src)?,
                None => self.allocate_and_write(&addr, src)?,
            }

            cursor += addr.chunk_len as u64;
            written += addr.chunk_len;
        }
        Ok(())
    }

    /// The host sector backing this grain, or `None` if the grain has no
    /// data yet — because its table is unallocated, its entry is zero,
    /// or its entry is the zeroed-grain marker.
    fn backing_sector(&self, gt_idx: usize, gte_idx: usize) -> Result<Option<u32>> {
        let gt_sector = self.gd_entry(gt_idx)?;
        if gt_sector == 0 {
            return Ok(None);
        }
        let entry = self.lookup_grain(gt_idx, gte_idx, gt_sector)?;
        Ok(match self.grain_state(entry) {
            GrainState::At(sector) => Some(sector),
            GrainState::Unallocated | GrainState::Zeroed => None,
        })
    }

    fn write_through(&self, grain_sector: u32, addr: &GrainAddress, src: &[u8]) -> Result<()> {
        let host_off =
            self.grain_host_offset(grain_sector, addr.in_grain, addr.chunk_len as u64)?;
        self.dev_write(host_off, src)?;
        self.dev_flush()
    }

    /// Give this grain a home and land `src` in it.
    ///
    /// Everything from here to the published grain-table entry runs
    /// under [`VmdkReader::allocation`]. It has to: the sequence is
    /// test-then-allocate-then-publish at two levels, and interleaving
    /// two of them loses a writer's data while telling it the write
    /// succeeded.
    ///
    /// The state is re-read under the lock rather than carried in from
    /// the caller's earlier look, because another writer may have
    /// allocated the table, the grain, or both in between — in which
    /// case this becomes an ordinary write-through.
    fn allocate_and_write(&self, addr: &GrainAddress, src: &[u8]) -> Result<()> {
        let _serialised = self.allocation.lock().unwrap();

        // Zero means the whole grain table is unallocated, so one has to
        // exist before the grain inside it can.
        let gt_sector = match self.gd_entry(addr.gt_idx)? {
            0 => self.allocate_grain_table(addr.gt_idx)?,
            s => s,
        };

        let entry = self.lookup_grain(addr.gt_idx, addr.gte_idx, gt_sector)?;
        if let GrainState::At(grain_sector) = self.grain_state(entry) {
            return self.write_through(grain_sector, addr, src);
        }

        // Sparse grain, or one the table marks as all-zero: allocate,
        // zero-pad, write payload, then publish the grain-table entry.
        // The zeroed-grain marker takes this branch because it is not a
        // sector number — following it would write the payload over the
        // embedded descriptor.
        let grain_bytes = self.grain_size_bytes();
        let new_grain_sector = self.allocate_grain()?;
        if addr.in_grain != 0 || (addr.chunk_len as u64) < grain_bytes {
            // Partial-grain write: zero-init the whole grain first so
            // the unwritten head/tail reads as zero (the spec's "absent
            // → zero" semantics carry over to a freshly allocated grain,
            // and a zeroed grain promised zeros there outright).
            let zeros = vec![0u8; grain_bytes as usize];
            self.dev_write((new_grain_sector as u64) * SECTOR_SIZE, &zeros)?;
        }
        self.dev_write((new_grain_sector as u64) * SECTOR_SIZE + addr.in_grain, src)?;
        self.dev_flush()?;
        // Step 2: publish the GT entry.
        self.update_gt_entry(addr.gt_idx, addr.gte_idx, gt_sector, new_grain_sector)
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
    /// How far the file reaches now.
    ///
    /// `FileDevice::size_bytes` is the length read when the file was
    /// opened and never changes again, so on a writable image it stops
    /// being the file's length the first time a grain or a grain table
    /// is allocated at the tail. The allocation cursor is where the
    /// writer has reached, so the later of the two is the real extent.
    fn file_extent(&self) -> u64 {
        let cursor = *self.alloc_cursor.lock().unwrap();
        self.dev
            .size_bytes()
            .max(cursor.saturating_mul(SECTOR_SIZE))
    }

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

    /// How many sectors one grain table occupies.
    fn grain_table_sectors(&self) -> u64 {
        ((self.header.num_gtes_per_gt as u64) * GD_GT_ENTRY_SIZE).div_ceil(SECTOR_SIZE)
    }

    /// Allocate a fresh, zero-filled grain table at the device tail.
    fn allocate_blank_grain_table(&self) -> Result<u32> {
        let gt_sectors = self.grain_table_sectors();
        let sector = self.allocate_sectors(gt_sectors)?;
        if sector > u32::MAX as u64 {
            return Err(Error::Unsupported("grain table sector past u32 range"));
        }
        let zeros = vec![0u8; (gt_sectors * SECTOR_SIZE) as usize];
        self.dev_write(sector * SECTOR_SIZE, &zeros)?;
        self.dev_flush()?;
        Ok(sector as u32)
    }

    /// Publish `gt_sector` into slot `gt_idx` of one grain directory, on
    /// disk and in the cached copy.
    ///
    /// Holds the in-memory lock across both so a concurrent reader never
    /// sees a memory/disk mismatch.
    fn publish_directory_entry(
        &self,
        cached: &Mutex<Vec<u32>>,
        dir_offset_sectors: u64,
        gt_idx: usize,
        gt_sector: u32,
    ) -> Result<()> {
        let mut dir = cached.lock().unwrap();
        if gt_idx >= dir.len() {
            return Err(Error::Corrupt("gt_idx past grain directory"));
        }
        let entry_off = dir_offset_sectors * SECTOR_SIZE + (gt_idx as u64) * GD_GT_ENTRY_SIZE;
        self.dev_write(entry_off, &gt_sector.to_le_bytes())?;
        self.dev_flush()?;
        dir[gt_idx] = gt_sector;
        Ok(())
    }

    /// Allocate a grain table for `gt_idx` in both directories and
    /// return the primary's sector number.
    ///
    /// Crash-safety order: zero-init both tables → publish the
    /// **redundant** directory entry → publish the primary. VMware's
    /// convention is that the redundant copy leads, so a crash leaves
    /// the primary behind rather than ahead, and a recovery tool that
    /// falls back to the redundant copy never finds it claiming a grain
    /// the primary has not got. Either way a crash may leak a grain
    /// table but never produces a wrong-data read: an absent directory
    /// entry reads as zeros.
    fn allocate_grain_table(&self, gt_idx: usize) -> Result<u32> {
        let new_gt_sector = self.allocate_blank_grain_table()?;

        if let Some(rgd) = &self.rgd {
            let redundant = self.allocate_blank_grain_table()?;
            self.publish_directory_entry(rgd, self.header.rgd_offset, gt_idx, redundant)?;
        }
        self.publish_directory_entry(&self.gd, self.header.gd_offset, gt_idx, new_gt_sector)?;

        // Invalidate the GT cache slot if it happened to hold this index
        // (it can't have meaningful contents — the GT was just zeroed —
        // but being defensive avoids a stale-cache surprise).
        let mut cache = self.gt_cache.lock().unwrap();
        if cache.loaded.is_some_and(|(idx, _)| idx == gt_idx) {
            cache.loaded = None;
            cache.entries.clear();
        }

        Ok(new_gt_sector)
    }

    /// The redundant copy of grain table `gt_idx`, if this image has a
    /// redundant directory.
    ///
    /// An image written by a tool that only maintained the primary can
    /// have a primary table where the redundant directory has none.
    /// Rather than skip the mirror — which would leave the copy wrong
    /// for every entry, not just the new one — a table is allocated and
    /// seeded with the primary's current contents, so the redundant copy
    /// becomes correct rather than merely no worse.
    fn redundant_gt_sector(&self, gt_idx: usize, primary_gt_sector: u32) -> Result<Option<u32>> {
        let Some(rgd) = &self.rgd else {
            return Ok(None);
        };
        let existing = {
            let dir = rgd.lock().unwrap();
            *dir.get(gt_idx)
                .ok_or(Error::Corrupt("gt_idx past redundant grain directory"))?
        };
        if existing != 0 {
            return Ok(Some(existing));
        }

        let sector = self.allocate_blank_grain_table()?;
        let len = (self.grain_table_sectors() * SECTOR_SIZE) as usize;
        let mut primary = vec![0u8; len];
        self.dev_read((primary_gt_sector as u64) * SECTOR_SIZE, &mut primary)?;
        self.dev_write((sector as u64) * SECTOR_SIZE, &primary)?;
        self.dev_flush()?;
        self.publish_directory_entry(rgd, self.header.rgd_offset, gt_idx, sector)?;
        Ok(Some(sector))
    }

    /// Overwrite a single grain-table entry, in both copies of the
    /// table, and refresh the in-memory cache if it currently holds
    /// this GT.
    ///
    /// The redundant copy is written first, for the same reason the
    /// redundant directory entry is: a crash between the two writes
    /// should leave the primary behind the redundant copy rather than
    /// ahead of it.
    fn update_gt_entry(
        &self,
        gt_idx: usize,
        gte_idx: usize,
        gt_sector: u32,
        new_grain_sector: u32,
    ) -> Result<()> {
        let bytes = new_grain_sector.to_le_bytes();
        let entry_within = (gte_idx as u64) * GD_GT_ENTRY_SIZE;

        if let Some(redundant) = self.redundant_gt_sector(gt_idx, gt_sector)? {
            self.dev_write((redundant as u64) * SECTOR_SIZE + entry_within, &bytes)?;
            self.dev_flush()?;
        }

        self.dev_write((gt_sector as u64) * SECTOR_SIZE + entry_within, &bytes)?;
        self.dev_flush()?;

        let mut cache = self.gt_cache.lock().unwrap();
        if cache.loaded == Some((gt_idx, gt_sector)) && gte_idx < cache.entries.len() {
            cache.entries[gte_idx] = new_grain_sector;
        }
        Ok(())
    }
}

/// The largest file this reader will consider reading whole as a
/// descriptor.
///
/// A descriptor *file* is a few hundred bytes: a handful of key=value
/// lines and one extent line per extent. The embedded descriptor of a
/// sparse image is bounded by `descriptorSize`, twenty sectors in every
/// image the reference tools write. 64 KiB is far above both and far
/// below anything worth reading into memory on the strength of a guess.
const MAX_DESCRIPTOR_FILE_BYTES: u64 = 64 * 1024;

/// Whether this device begins with a sparse extent header.
///
/// Anything else is either a descriptor file or not a VMDK at all, and
/// the two are told apart by trying to parse it.
fn looks_like_a_sparse_extent(dev: &Arc<dyn BlockDevice>, dev_size: u64) -> Result<bool> {
    if dev_size < HEADER_SIZE as u64 {
        return Ok(false);
    }
    let mut magic = [0u8; 4];
    dev.read_at(0, &mut magic).map_err(fs_core_to_vmdk_error)?;
    Ok(u32::from_le_bytes(magic) == crate::header::MAGIC)
}

/// The verdict on a file that is not a sparse extent.
///
/// Returns [`Error::Unsupported`] naming the create type when the file
/// parses as a descriptor, and [`Error::NotVmdk`] when it does not —
/// which keeps "not a VMDK" meaning what it says. A descriptor that
/// declares `monolithicSparse` reaches this function only as a sidecar
/// pointing at a separate extent, which is a layout this crate does not
/// follow either.
fn describe_descriptor_file(dev: &Arc<dyn BlockDevice>, dev_size: u64) -> Error {
    if dev_size == 0 || dev_size > MAX_DESCRIPTOR_FILE_BYTES {
        return Error::NotVmdk;
    }
    let mut bytes = vec![0u8; dev_size as usize];
    if dev.read_at(0, &mut bytes).is_err() {
        return Error::NotVmdk;
    }
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Error::NotVmdk;
    };
    match Descriptor::parse(text) {
        Err(Error::Unsupported(msg)) => Error::Unsupported(msg),
        // A descriptor that says `monolithicSparse` and is a file of its
        // own describes an extent somewhere else, which is as far out of
        // reach as a flat one.
        Ok(_) => Error::Unsupported(
            "monolithicSparse descriptor file — the sparse extent it names is a separate \
             file, and this crate reads only an image with its descriptor embedded",
        ),
        // No `createType` at all, or anything else the descriptor parser
        // refuses: this is not a VMDK descriptor, and saying so is the
        // honest answer.
        Err(_) => Error::NotVmdk,
    }
}

/// Read one grain directory: `len` bytes of little-endian `u32` sector
/// numbers at byte offset `off`.
fn read_directory(dev: &Arc<dyn BlockDevice>, off: u64, len: usize) -> Result<Vec<u32>> {
    let mut bytes = vec![0u8; len];
    dev.read_at(off, &mut bytes)
        .map_err(fs_core_to_vmdk_error)?;
    Ok(bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
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
