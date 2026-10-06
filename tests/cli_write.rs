//! `img.vmdk write` refuses input that cannot be written before writing
//! any of it, and leaves the image as it was.
//!
//! The tool reads its input from stdin: a regular file redirected there is
//! measured before a byte is read, and a pipe is read no further than one
//! byte past what fits. What the written bytes look like to another
//! implementation is tests/cli/test-write.sh's question, against qemu-img.

mod common;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use common::{TempPath, WriteAt};
use img_vmdk::header::{HEADER_SIZE, MAGIC};

const TOOL: &str = env!("CARGO_BIN_EXE_rust-img-vmdk");

const SECTOR: u64 = 512;
/// 1 MiB of guest, in one 64 KiB-grain table.
const CAPACITY_SECTORS: u64 = 2048;
const GRAIN_SIZE: u64 = 128;
const NUM_GTES_PER_GT: u32 = 512;
const GD_OFF_SECTOR: u64 = 2;
const GT_OFF_SECTOR: u64 = 3;
const GRAIN0_OFF_SECTOR: u64 = 7;
const VIRTUAL_SIZE: u64 = CAPACITY_SECTORS * SECTOR;

/// A monolithicSparse image whose only allocated grain is grain 0, filled
/// with 0x33; the same layout tests/write.rs builds.
fn one_grain_image(name: &str) -> TempPath {
    let path = TempPath::new(&format!("cli_write_{name}"));
    let mut h = [0u8; HEADER_SIZE];
    h[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    h[4..8].copy_from_slice(&1u32.to_le_bytes());
    h[12..20].copy_from_slice(&CAPACITY_SECTORS.to_le_bytes());
    h[20..28].copy_from_slice(&GRAIN_SIZE.to_le_bytes());
    h[28..36].copy_from_slice(&1u64.to_le_bytes());
    h[36..44].copy_from_slice(&1u64.to_le_bytes());
    h[44..48].copy_from_slice(&NUM_GTES_PER_GT.to_le_bytes());
    h[56..64].copy_from_slice(&GD_OFF_SECTOR.to_le_bytes());
    h[64..72].copy_from_slice(&GRAIN0_OFF_SECTOR.to_le_bytes());
    h[73..77].copy_from_slice(b"\n \r\n");
    let text = format!(
        "# Disk DescriptorFile\nversion=1\nCID=fffffffe\nparentCID=ffffffff\n\
         createType=\"monolithicSparse\"\n\nRW {CAPACITY_SECTORS} SPARSE \"cli.vmdk\"\n"
    );
    let mut gt = vec![0u8; NUM_GTES_PER_GT as usize * 4];
    gt[0..4].copy_from_slice(&(GRAIN0_OFF_SECTOR as u32).to_le_bytes());

    let mut f = std::fs::File::create(&path).unwrap();
    f.set_len((GRAIN0_OFF_SECTOR + GRAIN_SIZE) * SECTOR)
        .unwrap();
    f.write_all_at(&h, 0).unwrap();
    f.write_all_at(text.as_bytes(), SECTOR).unwrap();
    f.write_all_at(
        &(GT_OFF_SECTOR as u32).to_le_bytes(),
        GD_OFF_SECTOR * SECTOR,
    )
    .unwrap();
    f.write_all_at(&gt, GT_OFF_SECTOR * SECTOR).unwrap();
    f.write_all_at(
        &vec![0x33u8; (GRAIN_SIZE * SECTOR) as usize],
        GRAIN0_OFF_SECTOR * SECTOR,
    )
    .unwrap();
    path
}

fn img(args: &[&str]) -> Command {
    let mut cmd = Command::new(TOOL);
    cmd.arg("img").args(args);
    cmd
}

fn write_from_file(image: &Path, offset: &str, input: &Path) -> Output {
    img(&[image.to_str().unwrap(), "write", "--offset", offset])
        .stdin(std::fs::File::open(input).unwrap())
        .output()
        .unwrap()
}

#[test]
fn write_refuses_an_input_past_the_virtual_disk_by_its_length() {
    let image = one_grain_image("past-end");
    let before = std::fs::read(&image).unwrap();

    let input = TempPath::new("cli_write_past_end_input");
    std::fs::File::create(&input)
        .unwrap()
        .set_len(VIRTUAL_SIZE + 1)
        .unwrap();
    let wrote = write_from_file(&image, "0", &input);
    let stderr = String::from_utf8_lossy(&wrote.stderr);
    assert_eq!(
        wrote.status.code(),
        Some(1),
        "an oversized input was accepted: {stderr}"
    );
    // Off Unix stdin is always read as a pipe (see `stdin_file`), so the
    // refusal there is the pipe's: the bytes on stdin outnumber the room.
    assert!(
        stderr.contains("run past") || (cfg!(not(unix)) && stderr.contains("more than")),
        "refused, but not by its length: {stderr}"
    );
    assert!(
        std::fs::read(&image).unwrap() == before,
        "the refused write changed the image"
    );

    // And an input that fits, into the allocated grain, still writes.
    std::fs::write(&input, b"fits").unwrap();
    let wrote = write_from_file(&image, "512", &input);
    assert!(
        wrote.status.success(),
        "{}",
        String::from_utf8_lossy(&wrote.stderr)
    );
    let r = img_vmdk::VmdkReader::open(&image).unwrap();
    let mut back = [0u8; 6];
    r.read_at(511, &mut back).unwrap();
    assert_eq!(&back, b"\x33fits\x33");
    assert_eq!(r.header().unclean_shutdown, 0, "the write left it dirty");
}

/// Input through a pipe has no length to check first; it is read no
/// further than one byte past what fits, and refused before any is written.
#[test]
fn write_refuses_a_piped_input_past_the_virtual_disk_before_writing() {
    let image = one_grain_image("pipe-past-end");
    let before = std::fs::read(&image).unwrap();

    let mut child = img(&[image.to_str().unwrap(), "write", "--offset", "512"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    // More than fits; the tool may stop reading before all of it is sent.
    let _ = stdin.write_all(&vec![0xAB; VIRTUAL_SIZE as usize * 2]);
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(1),
        "an oversized pipe was accepted: {stderr}"
    );
    assert!(stderr.contains("more than"), "{stderr}");
    assert!(
        std::fs::read(&image).unwrap() == before,
        "the refused write changed the image"
    );
}

/// An image given as its own input is refused, and left as it was.
#[cfg(unix)]
#[test]
fn write_refuses_the_image_as_its_own_input() {
    let image = one_grain_image("self");
    let before = std::fs::read(&image).unwrap();
    let wrote = write_from_file(&image, "0", &image);
    let stderr = String::from_utf8_lossy(&wrote.stderr);
    assert!(!wrote.status.success(), "an image was written into itself");
    assert!(stderr.contains("is the image being written"), "{stderr}");
    assert!(
        std::fs::read(&image).unwrap() == before,
        "the refused write changed the image"
    );
}
