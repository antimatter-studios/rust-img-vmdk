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
//!
//! A crash mid-allocation may leak a grain or grain table but never
//! produces a wrong-data read.
//!
//! A device flush is an fsync -- `F_FULLFSYNC` on macOS -- so it is
//! issued only where that order depends on it (#42): after a new grain's
//! data and before the entry that points at it, after a zeroed table and
//! before the directory entry that points at it, and between a redundant
//! entry and the primary one it leads. A write into a grain that already
//! exists publishes nothing and issues none, and neither does the last
//! entry of an allocation: [`VmdkReader::flush`] is the caller's
//! durability barrier.
//!
//! ## What a write records about itself
//!
//! Two pieces of per-image bookkeeping belong to the writer, and both
//! are set before any data moves.
//!
//! The descriptor's `CID` is a *content* identifier: a child disk
//! records its parent's value in `parentCID`, so that a child whose
//! remembered value no longer matches can be recognised as stale. This
//! crate refuses to open a child, but nothing stops it opening the
//! **parent** of a chain, so it bumps the identifier on the first write
//! of a session. Bumping it before the write rather than after is
//! deliberate: a crash in between then leaves an identifier that moved
//! and contents that did not, which makes a child refuse — the
//! conservative answer — where the other order leaves changed contents
//! under the identifier the child remembers.
//!
//! `uncleanShutdown` is raised before a write and lowered by a clean
//! `flush`, so it stands over exactly the window in which a crash would
//! leave the image half-written.
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
use crate::header::{offsets, SparseHeader, GTE_ZEROED_GRAIN, HEADER_SIZE};
use fs_core::{BlockDevice, BlockRead, FileDevice};
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

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

/// A stream-optimized grain, inflated, keyed by its host sector and its
/// virtual start.
type InflatedGrain = ((u32, u64), Arc<Vec<u8>>);

pub struct VmdkReader {
    /// The stream-optimized grain inflated last, by its host sector and
    /// its virtual start (#48).
    ///
    /// A caller reading in pieces smaller than a grain would otherwise
    /// inflate the same 64 KiB again for every piece.
    inflated: Mutex<Option<InflatedGrain>>,
    /// Backing device, as the read half. All host-offset reads go
    /// through here.
    ///
    /// `BlockRead` rather than `BlockDevice` because nothing on the
    /// read path needs the write half, and demanding it meant a caller
    /// holding an `Arc<dyn BlockRead>` — which is what this family's
    /// read-only path passes around — could not open an image without
    /// wrapping it first. `CountingDevice`, `SliceReader` and
    /// `OwnedSlice` are all `BlockRead`, and the first of those is the
    /// instrument this family measures drivers with.
    dev: Arc<dyn BlockRead>,
    header: SparseHeader,
    /// The descriptor this image carries, as parsed at open.
    ///
    /// Held rather than dropped. It is the only place the create type and
    /// the extent list are written down, and the checks in
    /// [`descriptor_agrees_with_header`] are checks *of* it; dropping it
    /// left the parse as nothing but a way of failing on a malformed
    /// descriptor, and left a caller wanting either field to re-read and
    /// re-parse the region itself.
    ///
    /// The parent linkage (`parentCID`, `parentFileNameHint`) is **not**
    /// retained: [`Descriptor::parse`] reads it only to refuse an image
    /// that declares a parent, so every descriptor held here has none.
    descriptor: Descriptor,
    /// Cached primary grain directory (one u32 per grain table). Always
    /// small — `ceil(capacity / (grain_size * num_gtes_per_gt))` entries.
    /// Mutex-wrapped because the writer mutates entries in place when
    /// allocating a new grain table.
    gd: Mutex<Vec<u32>>,
    /// Cached redundant grain directory, when the image declares one
    /// ([`SparseHeader::has_redundant_grain_directory`]: the flag and a
    /// nonzero `rgd_offset`). `None` for an image without a redundant
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
    /// The same device again, present only when the image was opened
    /// for writing.
    ///
    /// Kept as an `Option` rather than beside a `bool` so that "can
    /// this be written" is a property of what the type holds: the write
    /// path cannot reach a device at all without going through this
    /// field, where before every write site had to remember to check a
    /// flag first. `CachingDevice` in the shared crate has the same
    /// shape for the same reason.
    writable: Option<Arc<dyn BlockDevice>>,
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
    /// Grain tables whose redundant copy this session has already merged
    /// the primary into; see [`VmdkReader::redundant_gt_sector`].
    redundant_merged: Mutex<std::collections::HashSet<usize>>,
    /// First byte a grain may legally occupy: `over_head`, the format's
    /// own statement of how much metadata precedes the grains. See
    /// [`VmdkReader::grain_host_offset`].
    first_grain_byte: u64,
    /// The metadata whose position the header states: the sparse header,
    /// the descriptor, the grain directory and — when one is live — the
    /// redundant directory. No grain may overlap it, and no grain table
    /// may either.
    fixed_metadata: Extents,
    /// Every grain table a live directory names, primary and redundant,
    /// plus every table this session allocates.
    ///
    /// A scalar floor cannot describe these: qemu puts them under
    /// `over_head`, but this crate allocates them at the tail, above
    /// the grains (#63). An `RwLock` rather than behind `allocation`,
    /// because the read path and the write-through fast path consult it
    /// without taking `allocation`.
    grain_tables: RwLock<Extents>,
    /// Byte offset and length of the embedded descriptor region.
    ///
    /// Kept so the `CID=` line can be rewritten in place on the first
    /// write of a session. It used to be read once, parsed, and dropped,
    /// which is how the content identifier came to be immutable.
    descriptor_extent: (u64, u64),
    /// Whether this session has already bumped the descriptor's `CID`.
    ///
    /// Once per session, not once per write: the identifier says the
    /// contents changed, and a value that changed a thousand times says
    /// no more than one that changed once, at a thousand times the cost.
    content_id_bumped: Mutex<bool>,
    /// Whether the image's `uncleanShutdown` byte is currently raised.
    ///
    /// Unlike the `CID` this goes up and down: it is raised before a
    /// write and lowered by a clean `flush`, so it stands exactly over
    /// the window in which a crash would leave the image half-written.
    /// Tracking it here rather than reading the byte back keeps the
    /// common case — write, write, write, flush — to one raise and one
    /// lower rather than one of each per write.
    unclean_marked: Mutex<bool>,
    /// Held shared by every `write_at` for its whole body, and exclusively
    /// by `flush`.
    ///
    /// `flush` lowers `uncleanShutdown`, which says the writes are
    /// complete. Taking only `unclean_marked` let it lower the marker
    /// while a write was still running, so a crash then left a
    /// half-written image indistinguishable from a clean close (#68).
    /// Writers still do not serialise against each other — they share
    /// the read side — and the lock order is this, then `unclean_marked`,
    /// on both paths. Holding `unclean_marked` across the write instead
    /// would self-deadlock in `mark_modified`.
    writes_in_flight: RwLock<()>,
}

/// A set of byte ranges, kept sorted and merged so an overlap query is a
/// binary search: a grain directory can name tens of thousands of
/// tables, and the read path asks once per grain.
#[derive(Debug, Default)]
struct Extents(Vec<(u64, u64)>);

impl Extents {
    /// Add `[start, end)`, merging it with anything it touches.
    fn insert(&mut self, start: u64, end: u64) {
        if start >= end {
            return;
        }
        let i = self.0.partition_point(|&(_, e)| e < start);
        let mut j = i;
        let (mut s, mut e) = (start, end);
        while j < self.0.len() && self.0[j].0 <= e {
            s = s.min(self.0[j].0);
            e = e.max(self.0[j].1);
            j += 1;
        }
        self.0.splice(i..j, [(s, e)]);
    }

    /// Whether `[start, end)` shares a byte with the set.
    fn overlaps(&self, start: u64, end: u64) -> bool {
        let i = self.0.partition_point(|&(_, e)| e <= start);
        self.0.get(i).is_some_and(|&(s, _)| s < end)
    }
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
        Self::open_inner(Arc::new(dev), None)
    }

    /// Open `path` read-write. Errors if the path isn't writable.
    pub fn open_rw<P: AsRef<Path>>(path: P) -> Result<Self> {
        let dev = FileDevice::open_rw(path.as_ref()).map_err(fs_core_to_vmdk_error)?;
        let dev: Arc<dyn BlockDevice> = Arc::new(dev);
        Self::open_inner(dev.clone(), Some(dev))
    }

    /// Open read-only on top of an arbitrary [`BlockRead`]. Used when
    /// the caller already holds a device handle (FSKit-supplied block
    /// resource, slice adapter, counting or caching wrapper, etc.) and
    /// wants the VMDK layer to sit on top of it.
    ///
    /// Takes the read half only. It used to demand `BlockDevice`, so a
    /// caller with an `Arc<dyn BlockRead>` had to wrap it in
    /// `ReadOnlyDevice` first — friction on the path every read-only
    /// consumer takes, in a crate whose primary use is read-only.
    pub fn open_on_device(dev: Arc<dyn BlockRead>) -> Result<Self> {
        Self::open_inner(dev, None)
    }

    /// Open read-write on top of an arbitrary [`BlockDevice`]. The
    /// device must report `is_writable()`; otherwise the call returns
    /// [`Error::ReadOnly`].
    pub fn open_rw_on_device(dev: Arc<dyn BlockDevice>) -> Result<Self> {
        if !dev.is_writable() {
            return Err(Error::ReadOnly);
        }
        Self::open_inner(dev.clone(), Some(dev))
    }

    fn open_inner(dev: Arc<dyn BlockRead>, writable: Option<Arc<dyn BlockDevice>>) -> Result<Self> {
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
        let mut header = SparseHeader::parse(&hdr_bytes)?;
        // A STREAM-OPTIMIZED IMAGE IS NOT WRITTEN (#48). It is append-only
        // by construction: a rewritten grain compresses to a different
        // length and does not fit where the old one was.
        if writable.is_some() && header.is_stream_optimized() {
            return Err(Error::Unsupported(
                "a stream-optimized VMDK is read-only here: its grains are compressed and \
                 append-only, so a grain cannot be rewritten in place",
            ));
        }
        // THE GRAIN DIRECTORY MAY BE IN THE FOOTER. A stream written in one
        // pass does not know where its directory will go when it writes the
        // header, so it says "at the end" (all ones) and repeats the header,
        // with the real offset, one sector before the end-of-stream marker.
        if header.is_stream_optimized() && header.gd_offset == u64::MAX {
            let footer_at = dev_size.checked_sub(2 * SECTOR_SIZE).ok_or(Error::Corrupt(
                "a footer-directory image too short to hold a footer",
            ))?;
            let mut footer = [0u8; HEADER_SIZE];
            dev.read_at(footer_at, &mut footer)
                .map_err(fs_core_to_vmdk_error)?;
            let footer = SparseHeader::parse(&footer)?;
            if footer.gd_offset == u64::MAX || !footer.is_stream_optimized() {
                return Err(Error::Corrupt(
                    "the header puts the grain directory in the footer, and the footer does \
                     not say where it is",
                ));
            }
            header.gd_offset = footer.gd_offset;
            header.rgd_offset = footer.rgd_offset;
        }

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
        // AN EMPTY DESCRIPTOR REGION IS A POSITIVE SIGNAL, NOT A PARSE
        // FAILURE.
        //
        // A split disk's extents each carry a sparse header with a
        // descriptor region reserved and left empty — qemu and VMware
        // both write them that way, because the descriptor for a split
        // disk lives in the sidecar `.vmdk`, not in the extent. Handing
        // those NULs to the descriptor parser produced "descriptor
        // missing createType", a `Corrupt`, about a healthy and complete
        // file.
        //
        // The two verdicts mean opposite things to a caller. `Corrupt`
        // says the file is damaged, which invites a warning, a repair, or
        // a refusal to trust the disk; `Unsupported` says to use a
        // different reader or convert it. Only a descriptor region with
        // *content* that fails to parse deserves the first.
        //
        // BUT THE MESSAGE MUST NOT SAY WHICH OF THE TWO IT IS. A monolithic
        // image whose descriptor was erased is byte-identical here to a
        // split extent: same magic, same nonzero descriptor offset and
        // size, all NUL. Only the sibling filename tells them apart, and a
        // device has no filename. So name both and let the caller, who has
        // the path, decide (#72).
        if desc_text
            .trim_matches(|c: char| c == '\0' || c.is_whitespace())
            .is_empty()
        {
            return Err(Error::Unsupported(
                "the embedded descriptor region is entirely empty: either one extent \
                 of a split disk, whose descriptor is in the sidecar .vmdk beside it, \
                 or an image whose descriptor has been erased",
            ));
        }
        let descriptor = Descriptor::parse(desc_text)?;
        descriptor_agrees_with_header(&descriptor, &header)?;
        // The first write rewrites the CID. If that rewrite would have to
        // guess where the field ends, refuse now — before a caller has
        // committed to a write — rather than on it (#67). A read-only open
        // never touches the CID and does not care.
        if writable.is_some() {
            locate_content_id(&desc_bytes).map_err(Error::Corrupt)?;
        }

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
        let redundant_gd = if !header.has_redundant_grain_directory() {
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
                Err(e) if writable.is_some() => return Err(e),
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
        //
        // BUT A SCALAR FLOOR CANNOT DESCRIBE THE LAYOUT (#63). Folding the
        // directories into it let an `rgd_offset` after the grains lift
        // the floor over every grain, and grain tables were not in it at
        // all, so a grain pointer naming one read the table back as data.
        // So the floor is `over_head` alone, and every other region is an
        // extent a grain must not overlap: the fixed metadata below, and
        // every grain table a live directory names.
        let first_grain_sector = fits(header.over_head, 0).unwrap_or(0).max(1);
        let first_grain_byte = first_grain_sector * SECTOR_SIZE;

        let mut fixed_metadata = Extents::default();
        fixed_metadata.insert(0, SECTOR_SIZE);
        let mut add_fixed = |start_sector: u64, sectors: u64| {
            if let Some(end) = fits(start_sector, sectors) {
                fixed_metadata.insert(start_sector * SECTOR_SIZE, end * SECTOR_SIZE);
            }
        };
        add_fixed(header.descriptor_offset, header.descriptor_size);
        add_fixed(header.gd_offset, gd_sectors);
        if redundant_gd.is_some() {
            add_fixed(header.rgd_offset, gd_sectors);
        }

        let gt_bytes = (header.num_gtes_per_gt as u64) * GD_GT_ENTRY_SIZE;
        let mut grain_tables = Extents::default();
        let rgd_entries = redundant_gd.as_ref().map(|m| m.lock().unwrap().clone());
        for &sector in gd.iter().chain(rgd_entries.iter().flatten()) {
            if sector == 0 {
                continue;
            }
            let start = (sector as u64) * SECTOR_SIZE;
            let end = start + gt_bytes;
            // A table that runs past the file or overlaps the fixed
            // metadata is refused when it is used; it is not a region to
            // protect.
            if end <= dev_size && !fixed_metadata.overlaps(start, end) {
                // TWO ENTRIES, ONE TABLE (review on #100). `insert` merges,
                // so a second entry naming storage an earlier one already
                // names would be silently folded in, and a write through
                // either index would then rewrite the other's mapping. A
                // read never writes a table, so only a writable open is
                // refused. Distinct directories name distinct copies, so any
                // overlap at all is corruption.
                if writable.is_some() && grain_tables.overlaps(start, end) {
                    return Err(Error::Corrupt(
                        "grain tables overlap: two directory entries name the same storage",
                    ));
                }
                grain_tables.insert(start, end);
            }
        }

        Ok(Self {
            dev,
            header,
            descriptor,
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
            redundant_merged: Mutex::new(std::collections::HashSet::new()),
            first_grain_byte,
            fixed_metadata,
            grain_tables: RwLock::new(grain_tables),
            descriptor_extent: (desc_byte_off, desc_byte_len),
            content_id_bumped: Mutex::new(false),
            unclean_marked: Mutex::new(false),
            writes_in_flight: RwLock::new(()),
            inflated: Mutex::new(None),
        })
    }

    pub fn virtual_size(&self) -> u64 {
        self.virtual_size
    }

    /// The descriptor this image carries.
    ///
    /// Its create type and extent list are the image's own account of
    /// what it is, which is worth having beside the header's. It carries
    /// no parent linkage: an image declaring a parent is refused at open,
    /// so there is none to report.
    pub fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }

    pub fn grain_size_bytes(&self) -> u64 {
        self.header.grain_size * SECTOR_SIZE
    }

    pub fn header(&self) -> &SparseHeader {
        &self.header
    }

    /// Whether the image was opened read-write.
    pub fn is_writable(&self) -> bool {
        self.writable.is_some()
    }

    // -- internal device adapters ------------------------------------------

    fn dev_read(&self, off: u64, buf: &mut [u8]) -> Result<()> {
        self.dev.read_at(off, buf).map_err(fs_core_to_vmdk_error)
    }

    fn dev_write(&self, off: u64, buf: &[u8]) -> Result<()> {
        self.writable
            .as_ref()
            .ok_or(Error::ReadOnly)?
            .write_at(off, buf)
            .map_err(fs_core_to_vmdk_error)
    }

    fn dev_flush(&self) -> Result<()> {
        self.writable
            .as_ref()
            .ok_or(Error::ReadOnly)?
            .flush()
            .map_err(fs_core_to_vmdk_error)
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
        // location (gd[gt] != 0 and gt[gte] != 0) → read from disk, or
        // (b) is unallocated → fill destination with zero.
        //
        // ONE DEVICE READ PER RUN, NOT PER GRAIN (#43). Grains a writer
        // laid out back to back -- which is what a converted image is --
        // are one contiguous host range, and reading them grain by grain
        // was one `pread` per 64 KiB. A present grain whose host bytes
        // continue the pending run extends it; anything else reads the
        // run out first. Each grain's pointer is still validated on its
        // own before it joins.
        let mut cursor = offset;
        let mut written: usize = 0;
        // (host offset, start in `buf`, length) of reads not yet issued.
        let mut run: Option<(u64, usize, usize)> = None;

        while cursor < end {
            let GrainAddress {
                in_grain,
                gt_idx,
                gte_idx,
                chunk_len,
            } = self.grain_address(cursor, end);
            let gt_sector = self.gd_entry(gt_idx)?;
            let state = if gt_sector == 0 {
                // Whole grain table unallocated — region reads as zero.
                GrainState::Unallocated
            } else {
                self.grain_state(self.lookup_grain(gt_idx, gte_idx, gt_sector)?)
            };

            match state {
                GrainState::Unallocated | GrainState::Zeroed => {
                    if let Some(pending) = run.take() {
                        self.read_run(buf, pending)?;
                    }
                    buf[written..written + chunk_len].fill(0);
                }
                GrainState::At(grain_sector) if self.header.is_stream_optimized() => {
                    if let Some(pending) = run.take() {
                        self.read_run(buf, pending)?;
                    }
                    let grain = self.inflated_grain(grain_sector, cursor - in_grain)?;
                    let from = in_grain as usize;
                    buf[written..written + chunk_len]
                        .copy_from_slice(&grain[from..from + chunk_len]);
                }
                GrainState::At(grain_sector) => {
                    let host_off =
                        self.grain_host_offset(grain_sector, in_grain, chunk_len as u64)?;
                    run = match run {
                        Some((at, from, len)) if at + len as u64 == host_off => {
                            Some((at, from, len + chunk_len))
                        }
                        other => {
                            if let Some(pending) = other {
                                self.read_run(buf, pending)?;
                            }
                            Some((host_off, written, chunk_len))
                        }
                    };
                }
            }

            cursor += chunk_len as u64;
            written += chunk_len;
        }
        if let Some(pending) = run {
            self.read_run(buf, pending)?;
        }

        Ok(())
    }

    /// Issue one pending run of [`Self::read_at`]: `len` bytes from host
    /// offset `at` into `buf[from..]`.
    fn read_run(&self, buf: &mut [u8], (at, from, len): (u64, usize, usize)) -> Result<()> {
        self.dev_read(at, &mut buf[from..from + len])
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

    /// A stream-optimized grain, inflated: the whole grain, whose virtual
    /// start is `virtual_start`.
    ///
    /// At the grain's host sector is a marker -- the grain's virtual LBA
    /// in sectors (u64) and the compressed length (u32) -- then that many
    /// bytes of zlib. A length of zero marks a metadata record (a grain
    /// table, the directory, the footer), which a grain-table entry must
    /// not name. The LBA has to be this grain's: a pointer at another
    /// grain's record would otherwise serve its bytes here. The inflated
    /// length has to be one grain, less only for the disk's last grain.
    ///
    /// The cache is keyed by the virtual start as well as the host sector:
    /// a record that two table entries name was checked against only the
    /// first of them, and the second must fail the marker check rather
    /// than be served the first one's bytes.
    fn inflated_grain(&self, grain_sector: u32, virtual_start: u64) -> Result<Arc<Vec<u8>>> {
        if let Some((key, grain)) = self.inflated.lock().unwrap().as_ref() {
            if *key == (grain_sector, virtual_start) {
                return Ok(grain.clone());
            }
        }
        const MARKER: u64 = 12;
        let at = self.grain_host_offset(grain_sector, 0, MARKER)?;
        let mut marker = [0u8; MARKER as usize];
        self.dev_read(at, &mut marker)?;
        let lba = u64::from_le_bytes(marker[..8].try_into().unwrap());
        let size = u64::from(u32::from_le_bytes(marker[8..].try_into().unwrap()));
        if size == 0 {
            return Err(Error::Corrupt(
                "a grain table entry names a metadata marker, not a compressed grain",
            ));
        }
        if lba.checked_mul(SECTOR_SIZE) != Some(virtual_start) {
            return Err(Error::Corrupt(
                "a compressed grain's marker names a different grain than the table entry \
                 that points at it",
            ));
        }
        let grain_bytes = self.grain_size_bytes();
        // No zlib stream of one grain is longer than zlib's own
        // `deflateBound` for it (the bound for any compression settings),
        // so a longer record is corrupt -- and is refused before its
        // image-controlled length becomes an allocation.
        let bound =
            grain_bytes + (grain_bytes >> 5) + (grain_bytes >> 7) + (grain_bytes >> 11) + 7 + 6;
        if size > bound {
            return Err(Error::Corrupt(
                "a compressed grain's record is longer than any zlib stream of one grain",
            ));
        }
        let payload_at = self.grain_host_offset(grain_sector, MARKER, size)?;
        let mut compressed = vec![0u8; size as usize];
        self.dev_read(payload_at, &mut compressed)?;

        let expected = grain_bytes.min(self.virtual_size - virtual_start) as usize;
        let mut grain = vec![0u8; grain_bytes as usize];
        let mut inflate = flate2::Decompress::new(true);
        let status = inflate
            .decompress(&compressed, &mut grain, flate2::FlushDecompress::Finish)
            .map_err(|_| Error::Corrupt("a compressed grain is not a valid zlib stream"))?;
        if status != flate2::Status::StreamEnd {
            return Err(Error::Corrupt(
                "a compressed grain inflates past one grain, or its stream is truncated",
            ));
        }
        let produced = inflate.total_out() as usize;
        if produced < expected {
            return Err(Error::Corrupt(
                "a compressed grain inflates to less than one grain",
            ));
        }
        let grain = Arc::new(grain);
        *self.inflated.lock().unwrap() = Some(((grain_sector, virtual_start), grain.clone()));
        Ok(grain)
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
        if self.fixed_metadata.overlaps(off, end)
            || self.grain_tables.read().unwrap().overlaps(off, end)
        {
            return Err(Error::Corrupt(
                "grain pointer lands inside the image's metadata",
            ));
        }
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
            // Past EOF is not the only way a table pointer is wrong. One
            // naming the descriptor or a directory passes that bound, and
            // a write allocating a grain in it publishes the entry over
            // that metadata (#64).
            if self.fixed_metadata.overlaps(off, end) {
                return Err(Error::Corrupt("grain table overlaps the image's metadata"));
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
    ///   growing), with a device flush only where a later step depends on
    ///   an earlier one being durable; see the module doc. Call
    ///   [`VmdkReader::flush`] for durability.
    pub fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        if !self.is_writable() {
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

        // Spans `mark_modified` as well as the loop: a flush slipping in
        // between the two would lower the marker just raised.
        let _in_flight = self.writes_in_flight.read().unwrap();

        // BEFORE ANY DATA MOVES, NOT AFTER.
        //
        // A crash between the stamp and the write leaves an image whose
        // content identifier changed and whose contents did not: a
        // snapshot child sees a mismatch and refuses, which is the
        // conservative wrong answer. Stamping afterwards would leave the
        // opposite -- changed contents under the identifier the child
        // remembers -- which is the failure this exists to prevent.
        self.mark_modified()?;

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

    /// No flush: nothing is published, so there is nothing to order the
    /// write against, and durability is the caller's `flush` (#42).
    fn write_through(&self, grain_sector: u32, addr: &GrainAddress, src: &[u8]) -> Result<()> {
        let host_off =
            self.grain_host_offset(grain_sector, addr.in_grain, addr.chunk_len as u64)?;
        self.dev_write(host_off, src)
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
        // Resolve (and check) the redundant table before a grain is
        // allocated, so a bad redundant pointer fails the write without
        // leaking a grain.
        self.redundant_gt_sector(addr.gt_idx, gt_sector)?;
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
        if !self.is_writable() {
            return Ok(());
        }
        // Wait for writes in flight, and hold new ones off, before the
        // device flush as well as before the marker comes down.
        let _quiesced = self.writes_in_flight.write().unwrap();
        self.dev_flush()?;
        // A clean sync is the caller saying its writes are complete, so
        // the unclean-shutdown marker comes down. A crash between a write
        // and the next flush leaves it standing, which is exactly what it
        // is for: the module doc already admits a crash mid-allocation
        // may leak a grain, and without the marker such an image is
        // byte-indistinguishable from one closed cleanly.
        let mut marked = self.unclean_marked.lock().unwrap();
        if *marked {
            self.dev_write(offsets::UNCLEAN_SHUTDOWN as u64, &[0u8])?;
            self.dev_flush()?;
            *marked = false;
        }
        Ok(())
    }

    /// Record that the image is being modified: bump the descriptor's
    /// content identifier if this session has not yet, and raise
    /// `uncleanShutdown` if it is not already up.
    ///
    /// Raised on the first write rather than at open, so that opening an
    /// image read-write and only reading from it does not modify the
    /// file.
    fn mark_modified(&self) -> Result<()> {
        {
            let mut bumped = self.content_id_bumped.lock().unwrap();
            if !*bumped {
                self.bump_content_id()?;
                *bumped = true;
            }
        }
        let mut marked = self.unclean_marked.lock().unwrap();
        if !*marked {
            self.dev_write(offsets::UNCLEAN_SHUTDOWN as u64, &[1u8])?;
            self.dev_flush()?;
            *marked = true;
        }
        Ok(())
    }

    /// Rewrite the descriptor's `CID=` line with a fresh value.
    ///
    /// The new value is always eight hex digits. The field it replaces is
    /// usually eight too, and then only those bytes change. But
    /// `qemu-img` writes the CID as an unpadded `%x`, so about one image
    /// in sixteen has fewer — and writing eight bytes in place over a
    /// seven-digit field ate its newline and glued `parentCID` onto it
    /// (#67). A short field is therefore widened: everything after it
    /// moves right into the region's NUL padding, and the region keeps
    /// the length `descriptorSize` declares. [`locate_content_id`] refuses
    /// a field it cannot rewrite exactly, and `open_rw` has already
    /// checked that.
    ///
    /// A descriptor with no `CID` line is left alone. Every image the
    /// reference producers write has one, and inserting one would move
    /// every byte after it — a much larger operation to perform on the
    /// strength of a field being absent.
    fn bump_content_id(&self) -> Result<()> {
        let (off, len) = self.descriptor_extent;
        let mut bytes = vec![0u8; len as usize];
        self.dev_read(off, &mut bytes)?;
        let Some(field) = locate_content_id(&bytes).map_err(Error::Corrupt)? else {
            return Ok(());
        };
        let current = std::str::from_utf8(&bytes[field.start..field.start + field.len])
            .ok()
            .and_then(|s| u32::from_str_radix(s, 16).ok())
            .unwrap_or(0);
        let next = next_content_id(current);
        let mut rewritten = format!("{next:08x}").into_bytes();
        rewritten.extend_from_slice(&bytes[field.start + field.len..field.content_end]);
        self.dev_write(off + field.start as u64, &rewritten)?;
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
        // Protected before anything can name it.
        self.grain_tables
            .write()
            .unwrap()
            .insert(sector * SECTOR_SIZE, (sector + gt_sectors) * SECTOR_SIZE);
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
        let len = (self.grain_table_sectors() * SECTOR_SIZE) as usize;
        let mut merged = self.redundant_merged.lock().unwrap();
        if existing != 0 {
            // Written to without any check, so an entry naming sector 1
            // put four bytes of a sector number over the descriptor
            // (#64). The same two bounds the primary table gets, checked
            // before the merge below reads and rewrites the whole table.
            let off = (existing as u64) * SECTOR_SIZE;
            let end = off + self.grain_table_sectors() * SECTOR_SIZE;
            if end > self.file_extent() {
                return Err(Error::Corrupt("redundant grain table extends past EOF"));
            }
            if self.fixed_metadata.overlaps(off, end) {
                return Err(Error::Corrupt(
                    "redundant grain table overlaps the image's metadata",
                ));
            }
            if !merged.contains(&gt_idx) {
                self.merge_primary_into_redundant(primary_gt_sector, existing, len)?;
                merged.insert(gt_idx);
            }
            return Ok(Some(existing));
        }

        let sector = self.allocate_blank_grain_table()?;
        let mut primary = vec![0u8; len];
        self.dev_read((primary_gt_sector as u64) * SECTOR_SIZE, &mut primary)?;
        self.dev_write((sector as u64) * SECTOR_SIZE, &primary)?;
        self.dev_flush()?;
        self.publish_directory_entry(rgd, self.header.rgd_offset, gt_idx, sector)?;
        merged.insert(gt_idx);
        Ok(Some(sector))
    }

    /// Fill every entry the redundant table lacks and the primary has.
    ///
    /// A redundant table can exist and be incomplete: this crate at
    /// 0.3.5 and earlier published redundant directory entries without
    /// keeping their tables in step, and so may any other producer. Used
    /// as-is, the first new entry mirrored into such a table left it
    /// claiming that grain and denying every older one, so a recovery
    /// tool falling back to it read holes where live data is (#65).
    ///
    /// A MERGE, NOT A COPY. The redundant table is written first on every
    /// update, so after a crash it may legitimately hold entries the
    /// primary lacks; those are the recovery data the copy exists for.
    /// Nothing nonzero in it is ever cleared or replaced. Done once per
    /// table per session, under [`VmdkReader::redundant_merged`], because
    /// this runs on every grain-table update.
    fn merge_primary_into_redundant(
        &self,
        primary_gt_sector: u32,
        redundant_gt_sector: u32,
        len: usize,
    ) -> Result<()> {
        let mut primary = vec![0u8; len];
        self.dev_read((primary_gt_sector as u64) * SECTOR_SIZE, &mut primary)?;
        let mut redundant = vec![0u8; len];
        self.dev_read((redundant_gt_sector as u64) * SECTOR_SIZE, &mut redundant)?;
        let mut changed = false;
        for (r, p) in redundant
            .chunks_exact_mut(GD_GT_ENTRY_SIZE as usize)
            .zip(primary.chunks_exact(GD_GT_ENTRY_SIZE as usize))
        {
            if r == [0u8; 4] && p != [0u8; 4] {
                r.copy_from_slice(p);
                changed = true;
            }
        }
        if changed {
            self.dev_write((redundant_gt_sector as u64) * SECTOR_SIZE, &redundant)?;
            self.dev_flush()?;
        }
        Ok(())
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

        // No flush after the primary entry: nothing later depends on it
        // being durable first, and the caller's `flush` makes it so (#42).
        self.dev_write((gt_sector as u64) * SECTOR_SIZE + entry_within, &bytes)?;

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
fn looks_like_a_sparse_extent(dev: &Arc<dyn BlockRead>, dev_size: u64) -> Result<bool> {
    if dev_size < HEADER_SIZE as u64 {
        return Ok(false);
    }
    let magic = head_magic(dev)?;
    // An ESXi vmfsSparse extent is a sparse extent too — a delta disk or
    // redo log — laid out differently from the header outwards. Saying
    // "not a VMDK image" about one is the same wrong verdict the flat
    // layouts used to get, arriving by a different route, so it is named
    // here with the message the descriptor path already gives the same
    // `createType`.
    if magic == crate::header::MAGIC_VMFS_SPARSE {
        return Err(Error::Unsupported(VMFS_SPARSE_UNSUPPORTED));
    }
    Ok(magic == crate::header::MAGIC)
}

/// The refusal for an ESXi `vmfsSparse` extent.
///
/// The descriptor and the header describe the same disk, or the image
/// is refused.
///
/// Both are in the file and both say how big the disk is, by
/// independent routes: `header.capacity` in sectors, and the extent
/// line's sector count. Comparing them is free on every open and is
/// exactly what a truncated or half-converted file gets wrong. It is
/// also what makes "this reader handles monolithicSparse" true rather
/// than aspirational — the extent list is the part of the descriptor
/// that says whether the whole disk is in this file.
///
/// A descriptor with several extents is a split image whose remaining
/// extents live in sibling files this reader does not open. Nothing
/// refused it before, so such a file was read as though extent 0 were
/// the whole disk: every offset past the first extent resolves through
/// a grain directory that does not describe it, and the result is
/// zeros or wrong bytes with no error either way.
///
/// Measured against the reference tool, whose `monolithicSparse`
/// images carry exactly one `SPARSE` extent whose sector count equals
/// `capacity` — including for capacities that are not a whole number
/// of grains, where a rounded extent length would have been the
/// plausible alternative:
///
/// ```text
/// 1M       cap=2048    RW 2048 SPARSE
/// 1234567  cap=2412    RW 2412 SPARSE
/// 3000000  cap=5860    RW 5860 SPARSE
/// 64M      cap=131072  RW 131072 SPARSE
/// ```
fn descriptor_agrees_with_header(desc: &Descriptor, header: &SparseHeader) -> Result<()> {
    let extent = match desc.extents.as_slice() {
        [one] => one,
        // A descriptor with no extent line at all describes no data.
        // `Corrupt` rather than `Unsupported`: there is no other reader
        // that would do better with it.
        [] => {
            return Err(Error::Corrupt(
                "descriptor declares no extent, so nothing says where the disk's data is",
            ))
        }
        _ => {
            return Err(Error::Unsupported(
                "a descriptor with more than one extent — the rest of the disk is in                  sibling files this crate does not open, and reading only the first                  would serve zeros or wrong bytes for everything past it",
            ))
        }
    };

    if !extent.kind.eq_ignore_ascii_case("SPARSE") {
        return Err(Error::Unsupported(
            "the descriptor's extent is not SPARSE, so the data is not laid out the way              a sparse extent's grain directory describes",
        ));
    }

    // THE LAYOUT IS STATED TWICE, and the two must agree. A descriptor
    // saying `streamOptimized` over an uncompressed header, or the
    // reverse, is read (or written) under one of two layouts with nothing
    // to say which is true.
    if (desc.create_type == "streamOptimized") != header.is_stream_optimized() {
        return Err(Error::Corrupt(
            "the descriptor's createType and the header's compression disagree about \
             whether this is a stream-optimized image",
        ));
    }

    if extent.sectors != header.capacity {
        return Err(Error::Corrupt(
            "the descriptor's extent length and the header's capacity disagree about              how big this disk is",
        ));
    }

    Ok(())
}

/// The same string [`Descriptor::parse`] returns for
/// `createType="vmfsSparse"`. One layout, one message, whichever file
/// named it — a corruption test asserts the two stay equal.
const VMFS_SPARSE_UNSUPPORTED: &str = "vmfsSparse";

fn head_magic(dev: &Arc<dyn BlockRead>) -> Result<u32> {
    let mut magic = [0u8; 4];
    dev.read_at(0, &mut magic).map_err(fs_core_to_vmdk_error)?;
    Ok(u32::from_le_bytes(magic))
}

/// The verdict on a file that is not a sparse extent.
///
/// Returns [`Error::Unsupported`] naming the create type when the file
/// parses as a descriptor, and [`Error::NotVmdk`] when it does not —
/// which keeps "not a VMDK" meaning what it says. A descriptor that
/// declares `monolithicSparse` reaches this function only as a sidecar
/// pointing at a separate extent, which is a layout this crate does not
/// follow either.
fn describe_descriptor_file(dev: &Arc<dyn BlockRead>, dev_size: u64) -> Error {
    if dev_size == 0 || dev_size > MAX_DESCRIPTOR_FILE_BYTES {
        return Error::NotVmdk;
    }
    let mut bytes = vec![0u8; dev_size as usize];
    // A failed read determined nothing about the bytes, so it is reported
    // as the I/O error it is — not as "not a VMDK", which would discard
    // the cause and tell a probing caller to move on (#69). Non-UTF-8
    // content below is a real verdict and stays `NotVmdk`.
    if let Err(e) = dev.read_at(0, &mut bytes) {
        return fs_core_to_vmdk_error(e);
    }
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Error::NotVmdk;
    };
    // `Descriptor::parse` needs only a `createType` line, so without this
    // any text file naming one was called an unsupported VMDK — which tells
    // a probing caller to stop looking (#70). Checked here and not in
    // `parse`, which the embedded-descriptor path shares.
    if !is_structurally_a_descriptor_file(text) {
        return Error::NotVmdk;
    }
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

/// Whether `text` has the shape of a VMDK descriptor file, beyond merely
/// naming a `createType`.
///
/// Every descriptor the reference producers write opens with the
/// `# Disk DescriptorFile` banner, carries `version=` and `CID=`, and
/// names at least one extent. Required here: the banner as the first
/// non-empty line **or** both `version` and `CID` keys, **and** one
/// parseable extent line. A banner found anywhere would accept a config
/// file that merely quotes it; a hand-written descriptor without the
/// banner still passes on its keys. Failing this says "not a VMDK", the
/// direction that lets a caller keep probing.
fn is_structurally_a_descriptor_file(text: &str) -> bool {
    let mut lines = text
        .lines()
        .map(|l| l.trim_matches(|c: char| c == '\0' || c.is_whitespace()));
    let banner = lines
        .clone()
        .find(|l| !l.is_empty())
        .is_some_and(|l| l == "# Disk DescriptorFile");
    let has_key = |want: &str| {
        text.lines()
            .filter_map(|l| l.split_once('='))
            .any(|(key, _)| key.trim() == want)
    };
    let names_an_extent = lines.any(|l| crate::descriptor::parse_extent(l).is_some());
    (banner || (has_key("version") && has_key("CID"))) && names_an_extent
}

/// How many hex digits a descriptor's `CID` value has.
const CID_DIGITS: usize = 8;

/// Where a descriptor's `CID=` value is, as [`locate_content_id`] found it.
struct ContentIdField {
    /// Byte offset of the first hex digit.
    start: usize,
    /// How many hex digits the value has: one to [`CID_DIGITS`].
    len: usize,
    /// One past the region's last non-NUL byte — the end of the text that
    /// has to move when a short value is widened.
    content_end: usize,
}

/// Find the descriptor's `CID=` value and check it can be rewritten as
/// eight digits without losing a byte.
///
/// `Ok(None)` when there is no `CID=` line at all: every image the
/// reference producers write has one, and inserting one would move every
/// byte after it on the strength of a field being absent.
///
/// Matched at the start of a line so that `parentCID=` — which is the
/// *other* half of the pair and must not move — cannot be mistaken for
/// it.
///
/// The value must be one to eight hex digits ended by a line end, a NUL,
/// or the end of the region. The previous matcher checked only that
/// eight bytes remained, so a short field had its terminator and the
/// start of the next line overwritten (#67). Anything else — non-hex,
/// too long, trailing text, or a short value with no NUL padding left to
/// widen into — is refused rather than guessed at.
fn locate_content_id(region: &[u8]) -> std::result::Result<Option<ContentIdField>, &'static str> {
    let mut at_line_start = true;
    let mut found = None;
    for i in 0..region.len() {
        if at_line_start && region[i..].starts_with(b"CID=") {
            found = Some(i + 4);
            break;
        }
        at_line_start = region[i] == b'\n' || region[i] == b'\r';
    }
    let Some(start) = found else {
        return Ok(None);
    };
    let len = region[start..]
        .iter()
        .take_while(|b| b.is_ascii_hexdigit())
        .count();
    let terminated = matches!(region.get(start + len), None | Some(b'\n' | b'\r' | b'\0'));
    if len == 0 || len > CID_DIGITS || !terminated {
        return Err("descriptor CID is not one to eight hex digits on its own line");
    }
    let content_end = region
        .iter()
        .rposition(|&b| b != 0)
        .map_or(0, |last| last + 1)
        .max(start + len);
    if content_end + (CID_DIGITS - len) > region.len() {
        return Err("descriptor CID is too short to widen and the descriptor region has no room");
    }
    Ok(Some(ContentIdField {
        start,
        len,
        content_end,
    }))
}

/// The next content identifier, given the current one.
///
/// It has to differ from the value it replaces — a `CID` that stayed the
/// same is the whole defect — and it must not be `ffffffff`, which is
/// the sentinel a `parentCID` uses for "no parent". Beyond that the
/// format asks nothing of it: it is an identifier, not a counter, and
/// nothing derives meaning from its ordering.
///
/// Mixing the clock with the old value rather than incrementing keeps
/// two images that started from the same `CID` from staying in step, and
/// avoids a dependency for the sake of eight hex digits.
fn next_content_id(current: u32) -> u32 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() ^ (d.as_secs() as u32))
        .unwrap_or(0);
    let mut next = current.wrapping_mul(1_664_525).wrapping_add(1_013_904_223) ^ nanos;
    while next == current || next == 0xFFFF_FFFF || next == 0 {
        next = next.wrapping_add(1);
    }
    next
}

/// Read one grain directory: `len` bytes of little-endian `u32` sector
/// numbers at byte offset `off`.
fn read_directory(dev: &Arc<dyn BlockRead>, off: u64, len: usize) -> Result<Vec<u32>> {
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

#[cfg(test)]
mod extents_tests {
    use super::Extents;

    #[test]
    fn inserts_merge_and_overlap_is_half_open() {
        let mut x = Extents::default();
        x.insert(100, 200);
        x.insert(300, 400);
        x.insert(150, 310); // bridges both
        x.insert(500, 600);
        x.insert(600, 700); // touches: merged
        assert_eq!(x.0, vec![(100, 400), (500, 700)]);

        assert!(!x.overlaps(0, 100), "end is exclusive");
        assert!(x.overlaps(0, 101));
        assert!(!x.overlaps(400, 500), "the gap between two extents");
        assert!(x.overlaps(399, 400));
        assert!(x.overlaps(650, 651));
        assert!(!x.overlaps(700, 800));
        assert!(x.overlaps(0, 10_000), "a range covering everything");
    }
}
