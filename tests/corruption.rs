//! Reader-level corruption and write-persistence tests.
//!
//! Builds a valid 1 MiB monolithicSparse VMDK sector-by-sector, then
//! surgically corrupts a single header field (or the descriptor bytes)
//! to confirm the `open()` path rejects it cleanly. Also covers a
//! write→drop→reopen→read round-trip that the synthetic suite doesn't.
//!
//! Layout (identical to tests/synthetic.rs):
//!
//! ```text
//!   sector 0       sparse extent header
//!   sector 1       embedded descriptor
//!   sector 2       grain directory (1 entry)
//!   sectors 3..6   grain table 0 (512 entries)
//!   sectors 7..134 grain 0 data (64 KiB)
//! ```

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;

use vmdk::header::{HEADER_SIZE, MAGIC};
use vmdk::{Error, VmdkReader};

const SECTOR: u64 = 512;
const CAPACITY_SECTORS: u64 = 2048; // 1 MiB
const GRAIN_SIZE: u64 = 128; // 64 KiB
const NUM_GTES_PER_GT: u32 = 512;
const DESC_OFF_SECTOR: u64 = 1;
const DESC_SIZE_SECTORS: u64 = 1;
const GD_OFF_SECTOR: u64 = 2;
const GT_OFF_SECTOR: u64 = 3;
const GRAIN0_OFF_SECTOR: u64 = 7;
const FILE_LEN: u64 = (GRAIN0_OFF_SECTOR + GRAIN_SIZE) * SECTOR;

// Header field offsets corrupted by individual tests, taken from the
// parser's own table rather than restated here. A test that corrupts
// "the descriptor offset" should corrupt whatever the parser reads as
// the descriptor offset — if the two ever disagree, the test passes
// while corrupting a neighbouring field.
const OFF_DESC_OFFSET: u64 = vmdk::header::offsets::DESCRIPTOR_OFFSET as u64;
const OFF_DESC_SIZE: u64 = vmdk::header::offsets::DESCRIPTOR_SIZE as u64;
const OFF_GD_OFFSET: u64 = vmdk::header::offsets::GD_OFFSET as u64;

fn tmp_path(name: &str) -> TempPath {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!(
        "vmdk_corrupt_{}_{n}_{name}.vmdk",
        std::process::id()
    ));
    TempPath(p)
}

/// RAII temp-file path: removes the backing file on drop so a panicking
/// assertion can't leak fixtures into the temp dir across CI runs.
struct TempPath(PathBuf);
impl std::ops::Deref for TempPath {
    type Target = std::path::Path;
    fn deref(&self) -> &std::path::Path {
        &self.0
    }
}
impl AsRef<std::path::Path> for TempPath {
    fn as_ref(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn build_header() -> [u8; HEADER_SIZE] {
    let mut h = [0u8; HEADER_SIZE];
    h[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    h[4..8].copy_from_slice(&1u32.to_le_bytes());
    h[12..20].copy_from_slice(&CAPACITY_SECTORS.to_le_bytes());
    h[20..28].copy_from_slice(&GRAIN_SIZE.to_le_bytes());
    h[28..36].copy_from_slice(&DESC_OFF_SECTOR.to_le_bytes());
    h[36..44].copy_from_slice(&DESC_SIZE_SECTORS.to_le_bytes());
    h[44..48].copy_from_slice(&NUM_GTES_PER_GT.to_le_bytes());
    h[56..64].copy_from_slice(&GD_OFF_SECTOR.to_le_bytes());
    h[64..72].copy_from_slice(&GRAIN0_OFF_SECTOR.to_le_bytes());
    h
}

fn descriptor_sector() -> [u8; SECTOR as usize] {
    let text = "createType=\"monolithicSparse\"\nRW 2048 SPARSE \"corrupt.vmdk\"\n";
    let mut s = [0u8; SECTOR as usize];
    s[..text.len()].copy_from_slice(text.as_bytes());
    s
}

/// A snapshot delta's descriptor: same `createType` as the baseline, but
/// it names a parent, so the grains it doesn't own live in that parent.
fn delta_descriptor_sector() -> [u8; SECTOR as usize] {
    let text = "createType=\"monolithicSparse\"\n\
                parentCID=fffffffd\n\
                parentFileNameHint=\"corrupt.vmdk\"\n\
                RW 2048 SPARSE \"corrupt-000001.vmdk\"\n";
    let mut s = [0u8; SECTOR as usize];
    s[..text.len()].copy_from_slice(text.as_bytes());
    s
}

/// Write a valid monolithicSparse VMDK with grain 0 allocated.
fn build_valid(path: &std::path::Path) {
    let mut f = File::create(path).unwrap();
    f.set_len(FILE_LEN).unwrap();
    write_at(&mut f, 0, &build_header());
    write_at(&mut f, DESC_OFF_SECTOR * SECTOR, &descriptor_sector());

    let mut gd = [0u8; SECTOR as usize];
    gd[0..4].copy_from_slice(&(GT_OFF_SECTOR as u32).to_le_bytes());
    write_at(&mut f, GD_OFF_SECTOR * SECTOR, &gd);

    let mut gt = vec![0u8; NUM_GTES_PER_GT as usize * 4];
    gt[0..4].copy_from_slice(&(GRAIN0_OFF_SECTOR as u32).to_le_bytes());
    write_at(&mut f, GT_OFF_SECTOR * SECTOR, &gt);

    let grain: Vec<u8> = (0..(GRAIN_SIZE * SECTOR))
        .map(|i| (i & 0xFF) as u8)
        .collect();
    write_at(&mut f, GRAIN0_OFF_SECTOR * SECTOR, &grain);
}

fn write_at(f: &mut File, off: u64, buf: &[u8]) {
    f.seek(SeekFrom::Start(off)).unwrap();
    f.write_all(buf).unwrap();
}

fn patch(path: &std::path::Path, off: u64, bytes: &[u8]) {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    write_at(&mut f, off, bytes);
    f.flush().unwrap();
}

#[test]
fn valid_image_opens_and_reads_grain() {
    let path = tmp_path("baseline");
    build_valid(&path);
    let r = VmdkReader::open(&path).unwrap();
    assert_eq!(r.virtual_size(), CAPACITY_SECTORS * SECTOR);
    let mut buf = [0u8; 8];
    r.read_at(0, &mut buf).unwrap();
    assert_eq!(buf, [0, 1, 2, 3, 4, 5, 6, 7]);
}

#[test]
fn missing_descriptor_offset_is_unsupported() {
    let path = tmp_path("no_desc");
    build_valid(&path);
    patch(&path, OFF_DESC_OFFSET, &0u64.to_le_bytes());
    match VmdkReader::open(&path) {
        Err(Error::Unsupported(_)) => {}
        other => panic!("expected Unsupported, got {:?}", other.err()),
    }
}

#[test]
fn descriptor_extending_past_eof_is_corrupt() {
    let path = tmp_path("desc_oob");
    build_valid(&path);
    patch(&path, OFF_DESC_SIZE, &100_000u64.to_le_bytes());
    match VmdkReader::open(&path) {
        Err(Error::Corrupt(_)) => {}
        other => panic!("expected Corrupt, got {:?}", other.err()),
    }
}

#[test]
fn zero_grain_directory_offset_is_corrupt() {
    let path = tmp_path("gd_zero");
    build_valid(&path);
    patch(&path, OFF_GD_OFFSET, &0u64.to_le_bytes());
    match VmdkReader::open(&path) {
        Err(Error::Corrupt(_)) => {}
        other => panic!("expected Corrupt, got {:?}", other.err()),
    }
}

#[test]
fn grain_directory_past_eof_is_corrupt() {
    let path = tmp_path("gd_oob");
    build_valid(&path);
    patch(&path, OFF_GD_OFFSET, &100_000u64.to_le_bytes());
    match VmdkReader::open(&path) {
        Err(Error::Corrupt(_)) => {}
        other => panic!("expected Corrupt, got {:?}", other.err()),
    }
}

#[test]
fn non_utf8_descriptor_is_corrupt() {
    let path = tmp_path("desc_binary");
    build_valid(&path);
    // Overwrite the descriptor sector with invalid UTF-8 bytes.
    patch(&path, DESC_OFF_SECTOR * SECTOR, &[0xFFu8; 64]);
    match VmdkReader::open(&path) {
        Err(Error::Corrupt(_)) => {}
        other => panic!("expected Corrupt, got {:?}", other.err()),
    }
}

#[test]
fn write_into_sparse_grain_persists_across_reopen() {
    let path = tmp_path("persist");
    build_valid(&path);

    // Grain 0 is allocated; grain 1 (virtual offset 64 KiB) is sparse.
    // Writing into it must allocate a grain and survive a reopen.
    let virt = GRAIN_SIZE * SECTOR; // 64 KiB — start of grain 1
    let payload = [0x5Au8; 4096];
    {
        let r = VmdkReader::open_rw(&path).unwrap();
        r.write_at(virt + 1024, &payload).unwrap();
        r.flush().unwrap();
    }

    let r2 = VmdkReader::open(&path).unwrap();
    let mut got = [0u8; 4096];
    r2.read_at(virt + 1024, &mut got).unwrap();
    assert_eq!(got, payload);
    // Rest of the freshly-allocated grain reads as zero.
    let mut head = [0u8; 1024];
    r2.read_at(virt, &mut head).unwrap();
    assert!(head.iter().all(|&b| b == 0));
}

#[test]
fn delta_disk_declaring_a_parent_is_refused_at_open() {
    // The image is structurally a perfectly good monolithicSparse VMDK —
    // only the descriptor's parent linkage says the data isn't all here.
    // Opening it would report the full virtual size and serve zeros for
    // every grain the delta doesn't own, which a caller cannot detect.
    let path = tmp_path("delta");
    build_valid(&path);
    patch(&path, DESC_OFF_SECTOR * SECTOR, &delta_descriptor_sector());
    match VmdkReader::open(&path) {
        Err(Error::Unsupported(msg)) => {
            assert!(
                msg.contains("parent"),
                "message must name the parent: {msg}"
            )
        }
        Ok(r) => panic!(
            "delta disk opened as a standalone {}-byte image — every grain it \
             does not own would silently read as zeros",
            r.virtual_size()
        ),
        Err(other) => panic!("expected Unsupported for a delta disk, got {other:?}"),
    }
}

const OFF_GRAIN_SIZE: u64 = vmdk::header::offsets::GRAIN_SIZE as u64;
const OFF_NUM_GTES_PER_GT: u64 = vmdk::header::offsets::NUM_GTES_PER_GT as u64;
const OFF_VERSION: u64 = vmdk::header::offsets::VERSION as u64;

/// `grain_size` is a sector count, and the reader turns it into bytes
/// by multiplying by 512. The only thing checked was that it was not
/// zero -- but any multiple of 2^55 multiplies out to exactly zero, and
/// the byte size is then used as a divisor.
///
/// Division by zero panics whatever the profile: unlike an overflow it
/// does not depend on `overflow-checks`, which this crate has off in
/// release. So this is a panic in the shipped library, out of a plain
/// `read_at`, from a header field.
#[test]
fn a_grain_size_that_multiplies_out_to_zero_is_refused_at_open() {
    let path = tmp_path("grain_size_wraps");
    build_valid(&path);
    patch(&path, OFF_GRAIN_SIZE, &(1u64 << 55).to_le_bytes());

    let outcome = VmdkReader::open(&path);
    assert!(
        outcome.is_err(),
        "a grain size of 2^55 sectors -- 2^64 bytes, which is zero -- was accepted"
    );
}

/// A grain table is read whole into a buffer of `num_gtes_per_gt * 4`,
/// and the field is a `u32` checked only for being non-zero, so it can
/// ask for 16 GiB. The grain *directory* ten lines earlier is required
/// to fit inside the file; the table is not.
#[test]
fn a_grain_table_claiming_more_than_the_file_is_refused_by_name() {
    let path = tmp_path("gt_past_file");
    build_valid(&path);
    patch(&path, OFF_NUM_GTES_PER_GT, &0x4000_0000u32.to_le_bytes());

    // The open may well succeed -- the grain directory is unchanged --
    // so the refusal has to come when the table is loaded.
    let why = match VmdkReader::open(&path) {
        Err(e) => format!("{e:?}"),
        Ok(r) => {
            let mut buf = [0u8; 512];
            format!("{:?}", r.read_at(0, &mut buf).err())
        }
    };
    assert!(
        why.contains("grain table"),
        "a grain table of 4 GiB in a 68 KiB file was refused as {why}, which \
         means the buffer was allocated and read first"
    );
}

/// The read path bounds a grain table before it touches it. The write
/// path creates one, so no lookup precedes it, and it sized both the
/// allocation and its zero-fill buffer straight from `num_gtes_per_gt`.
///
/// Opening a small image read-write and writing one sector into an
/// unallocated region therefore allocated whatever the header asked
/// for: measured with `num_gtes_per_gt = 0x0100_0000`, a 512-byte write
/// into a 68 KiB image produced a 64 MiB file and a 64 MiB zero buffer.
/// The image causing it need not be malicious — a truncated download
/// reaches the same place.
#[test]
fn a_one_sector_write_does_not_allocate_whatever_the_header_asks_for() {
    let path = tmp_path("gt_alloc_bomb");
    build_valid(&path);
    // Empty the grain-directory slot so the write has to *create* a
    // grain table rather than look one up.
    patch(&path, GD_OFF_SECTOR * SECTOR, &0u32.to_le_bytes());
    patch(&path, OFF_NUM_GTES_PER_GT, &0x0100_0000u32.to_le_bytes());

    let before = std::fs::metadata(&path).unwrap().len();
    let why = match VmdkReader::open_rw(&path) {
        Err(e) => format!("{e:?}"),
        Ok(r) => format!("{:?}", r.write_at(0, &[0xABu8; 512]).err()),
    };
    let after = std::fs::metadata(&path).unwrap().len();
    assert!(
        why.contains("grain table"),
        "a grain table of 64 MiB in a 68 KiB image was refused as {why}, which \
         means it was allocated and zero-filled first"
    );

    assert_eq!(
        before, after,
        "the image grew from {before} to {after} bytes for a 512-byte write"
    );
}

/// The sparse-extent header states a format revision, and the revision
/// changes what the rest of the file means. It was parsed and never
/// compared to anything, so an image declaring any revision at all was
/// read with version-1 semantics.
///
/// Version 3 is the stream-optimized revision: compressed grains, grain
/// markers, and a footer that replaces the header at the end of the
/// file. None of that is a layout this crate can walk. It happened to be
/// refused today by a *second* field — `compressAlgorithm` — which
/// leaves the refusal resting on the one place the format states the
/// fact twice. A version-3 image with `compressAlgorithm` left at zero,
/// which the field's own definition permits, walked straight through.
#[test]
fn a_sparse_extent_revision_we_cannot_read_is_refused_by_its_version() {
    let path = tmp_path("version_3");
    build_valid(&path);
    patch(&path, OFF_VERSION, &3u32.to_le_bytes());

    match VmdkReader::open(&path) {
        Err(Error::Unsupported(msg)) => assert!(
            msg.contains("version 3"),
            "the refusal must name the revision, got {msg:?}"
        ),
        Err(other) => panic!("expected Unsupported, got {other}"),
        Ok(_) => panic!("read a stream-optimized extent with version-1 semantics"),
    }
}

/// Versions 1 and 2 are both ordinary monolithic sparse extents, and
/// both are read here. `qemu-img` writes 1 for a plain image and 2 when
/// the image uses the zeroed-grain marker — which this crate reads, and
/// has a cross-validation test for — so refusing everything but 1 would
/// refuse images the suite itself produces.
#[test]
fn the_revisions_we_do_read_are_both_accepted() {
    for version in [1u32, 2] {
        let path = tmp_path(&format!("version_{version}"));
        build_valid(&path);
        patch(&path, OFF_VERSION, &version.to_le_bytes());

        let r = VmdkReader::open(&path)
            .unwrap_or_else(|e| panic!("version {version} must be readable, got {e}"));
        let mut buf = [0u8; 8];
        r.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, [0, 1, 2, 3, 4, 5, 6, 7]);
    }
}

/// An ESXi `vmfsSparse` extent — a delta or redo log — is a VMDK sparse
/// extent with a different four-byte magic, `COWD`, and a differently
/// laid out header. Failing the `KDMV` test made it "not a VMDK image",
/// which is the same wrong verdict the flat layouts used to get, by a
/// different route.
///
/// `descriptor.rs` has carried a stable `vmfsSparse` message all along
/// and nothing could reach it, because the header rejected the file
/// first. The two paths now give the same answer.
#[test]
fn an_esxi_vmfs_sparse_extent_is_refused_as_vmfs_sparse_not_as_not_a_vmdk() {
    let path = tmp_path("cowd");
    build_valid(&path);
    patch(&path, 0, b"COWD");

    let from_header = match VmdkReader::open(&path) {
        Err(Error::Unsupported(msg)) => msg,
        Err(other) => panic!("expected Unsupported, got {other}"),
        Ok(_) => panic!("read a vmfsSparse extent as if it were monolithicSparse"),
    };

    let from_descriptor = match vmdk::descriptor::Descriptor::parse("createType=\"vmfsSparse\"\n") {
        Err(Error::Unsupported(msg)) => msg,
        other => panic!("expected Unsupported from the descriptor, got {other:?}"),
    };
    assert_eq!(
        from_header, from_descriptor,
        "the same layout must be refused with the same message whichever file names it"
    );
}

/// A split disk's extents each carry a sparse header with a descriptor
/// region reserved and left empty, because the descriptor for a split
/// disk lives in the sidecar `.vmdk`. The parser saw a run of NULs, no
/// `createType`, and answered `Corrupt` — about a healthy, complete
/// file.
///
/// `Corrupt` and `Unsupported` are the two verdicts this crate offers
/// and they mean opposite things: one says the file is damaged, which
/// invites a warning, a repair, or a refusal to trust the disk; the
/// other says use a different reader or convert it. Only a descriptor
/// region with content that fails to parse deserves the first.
#[test]
fn an_empty_descriptor_region_is_a_split_extent_not_a_corrupt_image() {
    let path = tmp_path("empty_descriptor");
    build_valid(&path);
    patch(&path, DESC_OFF_SECTOR * SECTOR, &[0u8; SECTOR as usize]);

    match VmdkReader::open(&path) {
        Err(Error::Unsupported(msg)) => assert!(
            msg.contains("extent"),
            "the refusal must say this is one extent of a split image, got {msg:?}"
        ),
        Err(other) => panic!("expected Unsupported, got {other}"),
        Ok(_) => panic!("opened an extent of a split image as if it were a whole disk"),
    }
}

/// The distinction the change rests on: a descriptor region with content
/// the parser cannot make sense of is still `Corrupt`. Widening the
/// empty case must not turn every unreadable descriptor into "this is a
/// split image".
#[test]
fn a_descriptor_region_with_unparseable_content_is_still_corrupt() {
    let path = tmp_path("junk_descriptor");
    build_valid(&path);
    let mut junk = [0u8; SECTOR as usize];
    junk[..21].copy_from_slice(b"this is not an image\n");
    patch(&path, DESC_OFF_SECTOR * SECTOR, &junk);

    match VmdkReader::open(&path) {
        Err(Error::Corrupt(msg)) => assert!(msg.contains("createType"), "got {msg:?}"),
        Err(other) => panic!("expected Corrupt, got {other}"),
        Ok(_) => panic!("opened an image whose descriptor says nothing"),
    }
}

// ---------------------------------------------------------------------------
// The descriptor against the header
// ---------------------------------------------------------------------------

/// Replace the descriptor sector with `text`, NUL-padded.
fn patch_descriptor(path: &std::path::Path, text: &str) {
    let mut s = [0u8; SECTOR as usize];
    s[..text.len()].copy_from_slice(text.as_bytes());
    patch(path, DESC_OFF_SECTOR * SECTOR, &s);
}

/// A descriptor listing more than one extent is refused.
///
/// The rest of such a disk lives in sibling files this reader does not
/// open. Nothing refused it before, so the image was read as though
/// extent 0 were the whole disk: every offset past the first extent
/// resolves through a grain directory that does not describe it, and
/// the result is zeros or wrong bytes with no error either way.
#[test]
fn a_descriptor_naming_more_than_one_extent_is_refused() {
    let path = tmp_path("two_extents");
    build_valid(&path);
    patch_descriptor(
        &path,
        "createType=\"monolithicSparse\"\n\
         RW 2048 SPARSE \"corrupt-s001.vmdk\"\n\
         RW 2048 SPARSE \"corrupt-s002.vmdk\"\n",
    );
    match VmdkReader::open(&path) {
        Err(Error::Unsupported(m)) => assert!(
            m.contains("more than one extent"),
            "refused, but not for the extent list: {m}"
        ),
        Ok(_) => panic!("a two-extent descriptor opened"),
        Err(e) => panic!("a two-extent descriptor gave {e:?}"),
    }
}

/// A descriptor with no extent line at all describes no data.
#[test]
fn a_descriptor_naming_no_extent_is_refused() {
    let path = tmp_path("no_extent");
    build_valid(&path);
    patch_descriptor(&path, "createType=\"monolithicSparse\"\n");
    match VmdkReader::open(&path) {
        Err(Error::Corrupt(m)) => assert!(
            m.contains("no extent"),
            "refused, but not for the missing extent: {m}"
        ),
        Ok(_) => panic!("a descriptor with no extent opened"),
        Err(e) => panic!("a descriptor with no extent gave {e:?}"),
    }
}

/// The extent has to be the kind of extent a sparse header describes.
///
/// A `FLAT` extent is a raw span at an offset, not a grain directory.
/// The header says one thing and the extent line says another, and the
/// grain walk would read the flat data as though it were grains.
#[test]
fn a_descriptor_whose_extent_is_not_sparse_is_refused() {
    let path = tmp_path("flat_extent");
    build_valid(&path);
    patch_descriptor(
        &path,
        "createType=\"monolithicSparse\"\nRW 2048 FLAT \"corrupt.vmdk\" 0\n",
    );
    match VmdkReader::open(&path) {
        Err(Error::Unsupported(m)) => assert!(
            m.contains("not SPARSE"),
            "refused, but not for the extent kind: {m}"
        ),
        Ok(_) => panic!("a FLAT extent opened"),
        Err(e) => panic!("a FLAT extent gave {e:?}"),
    }
}

/// The two statements of the disk's size have to agree, and it is an
/// inequality in either direction that says so.
///
/// `header.capacity` and the extent line's sector count describe the
/// same length by independent routes, so a disagreement is the file
/// contradicting itself — what a truncated or half-converted image
/// looks like.
///
/// Three cases rather than one, because a disagreement has a direction
/// and a comparison can be written to see only one of them. An extent
/// longer than the capacity and an extent shorter than it are both
/// refused here; a check written as `>` passes the short case, and a
/// check written as `<` passes the long one. The third patches the
/// header rather than the descriptor, so the test does not depend on
/// which side of the comparison was disturbed.
#[test]
fn a_descriptor_and_header_that_disagree_about_the_size_are_refused() {
    // Descriptor side, too long: the extent claims twice the capacity.
    let path = tmp_path("extent_too_long");
    build_valid(&path);
    patch_descriptor(
        &path,
        "createType=\"monolithicSparse\"\nRW 4096 SPARSE \"corrupt.vmdk\"\n",
    );
    match VmdkReader::open(&path) {
        Err(Error::Corrupt(m)) => assert!(
            m.contains("disagree about"),
            "refused, but not for the size: {m}"
        ),
        Ok(_) => panic!("an extent twice the capacity opened"),
        Err(e) => panic!("an extent twice the capacity gave {e:?}"),
    }

    // Descriptor side, too short: the extent claims half of it. An
    // extent shorter than the disk is the truncated-conversion shape,
    // and it is the case a one-sided comparison lets through.
    let path = tmp_path("extent_too_short");
    build_valid(&path);
    patch_descriptor(
        &path,
        "createType=\"monolithicSparse\"\nRW 1024 SPARSE \"corrupt.vmdk\"\n",
    );
    match VmdkReader::open(&path) {
        Err(Error::Corrupt(m)) => assert!(
            m.contains("disagree about"),
            "refused, but not for the size: {m}"
        ),
        Ok(_) => panic!("an extent half the capacity opened"),
        Err(e) => panic!("an extent half the capacity gave {e:?}"),
    }

    // Header side: capacity halved, descriptor left alone.
    let path = tmp_path("capacity_halved");
    build_valid(&path);
    patch(&path, 12, &(CAPACITY_SECTORS / 2).to_le_bytes());
    match VmdkReader::open(&path) {
        Err(Error::Corrupt(m)) => assert!(
            m.contains("disagree about"),
            "refused, but not for the size: {m}"
        ),
        Ok(_) => panic!("a halved capacity opened"),
        Err(e) => panic!("a halved capacity gave {e:?}"),
    }
}

/// The descriptor is kept, so a caller can ask what the image says it
/// is without re-reading and re-parsing the region.
#[test]
fn the_descriptor_is_held_on_the_reader() {
    let path = tmp_path("descriptor_held");
    build_valid(&path);
    let r = VmdkReader::open(&path).unwrap();
    let d = r.descriptor();
    assert_eq!(d.create_type, "monolithicSparse");
    assert_eq!(d.extents.len(), 1);
    assert_eq!(d.extents[0].sectors, CAPACITY_SECTORS);
    assert_eq!(d.extents[0].kind, "SPARSE");
    assert_eq!(d.extents[0].filename, "corrupt.vmdk");
}
