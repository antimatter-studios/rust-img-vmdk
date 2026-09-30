//! `img.vmdk <image> <verb>`: an errand inside a VMDK disk image, without
//! a hypervisor.
//!
//! The verbs are the shared set for disk images: `info`/`get`, `read`,
//! `write`, `create`, `resize`, `set`. Metadata is JSON (or `--text`); the
//! guest's bytes are raw. `read` with no range streams the whole virtual
//! disk, so converting to a raw image is reading it. A verb the library
//! cannot do still exists and answers `not implemented` with exit status
//! 3, so a script moved between formats fails loudly instead of meaning
//! something else.
//!
//! WHAT THE LIBRARY OPENS: a single-file sparse extent, `monolithicSparse`
//! or `streamOptimized`. Every other layout -- flat, split, a VMFS extent,
//! a child with a parent -- is refused by the library at open with the
//! create type named, and this tool passes that on as `not implemented`.

use std::ffi::OsString;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use clap::{value_parser, Arg, ArgMatches, Command as Cmd};
use vmdk::VmdkReader;

use crate::common::{CliError, Json, Outcome, Tool};

pub const TOOL: Tool = Tool {
    name: "img.vmdk",
    verb: "img",
    section: 1,
    usage_exit: crate::common::output::EXIT_USAGE,
    about: "Report, read and write a VMDK disk image without a hypervisor",
    command,
    run,
};

/// The canonical keys every `img.<fmt>` answers, in the shared order.
/// The format's own fields are nested under `vmdk`.
pub const KEYS: &[&str] = &[
    "format",
    "virtual_size",
    "block_size",
    "backing",
    "dirty",
    "vmdk",
];

/// How much of the guest is moved at a time.
const CHUNK: usize = 1 << 20;

fn command() -> Cmd {
    Cmd::new("img.vmdk")
        .about("Report, read and write a VMDK disk image without a hypervisor")
        .long_about(
            "Work inside a VMDK disk image directly: single-file monolithicSparse and \
             streamOptimized images, zeroed-grain markers, redundant grain directories; no \
             hypervisor and no conversion tool. Flat, split and VMFS layouts, and a child \
             image with a parent, answer `not implemented` naming what the image is.\n\n\
             Metadata is JSON on stdout (--text for people); `read` writes the guest's raw \
             bytes, the whole virtual disk when no range is given, so converting to a raw \
             image is reading it. A failure is {\"error\": \"...\", \"code\": N} on stderr, \
             N being the exit status: 1 failed, 2 wrong command line, 3 not implemented.",
        )
        .arg(
            Arg::new("image")
                .value_name("IMAGE")
                .help("The VMDK image file")
                .value_parser(value_parser!(OsString))
                .required(true),
        )
        .args(crate::common::format_args().map(|a| a.global(true)))
        .subcommand_required(true)
        .subcommand(key_command(
            "info",
            "Report the image's properties, or one of them",
        ))
        .subcommand(key_command(
            "get",
            "The same as info: every property, or one of them",
        ))
        .subcommand(
            Cmd::new("read")
                .about("Write the guest's bytes to stdout, or to a file with -o: the whole disk, or a range")
                .arg(byte_count("offset", "Where to start, in the guest (default 0)"))
                .arg(byte_count(
                    "length",
                    "How many bytes (default: to the end of the virtual disk)",
                ))
                .arg(
                    Arg::new("output")
                        .short('o')
                        .long("output")
                        .value_name("FILE")
                        .value_parser(value_parser!(OsString))
                        .help("Write here instead of stdout (zero runs are left sparse)"),
                )
                .after_help(
                    "Examples:\n  img.vmdk disk.vmdk read -o disk.raw\n  \
                     img.vmdk disk.vmdk read --offset 0 --length 512 | xxd\n  \
                     img.vmdk disk.vmdk read --offset 1M --length 64K > chunk.bin\n\n\
                     Grains never written, and grains marked zeroed, read as zeros; a \
                     streamOptimized image's grains are inflated: the bytes are the ones the \
                     guest sees.",
                ),
        )
        .subcommand(
            Cmd::new("write")
                .about("Write the bytes on stdin into the guest at an offset (not implemented yet)")
                .arg(byte_count("offset", "Where to write, in the guest").required(true))
                .after_help(
                    "Examples:\n  img.vmdk disk.vmdk write --offset 0 < mbr.bin\n\n\
                     Answers `not implemented` (exit 3) in this version.",
                ),
        )
        .subcommand(
            Cmd::new("create")
                .about("Create a new, empty image (not implemented)")
                .arg(Arg::new("size").value_name("SIZE").required(true))
                .after_help(
                    "Examples:\n  img.vmdk new.vmdk create 64M\n\n\
                     Answers `not implemented` (exit 3): the library has no image creator.",
                ),
        )
        .subcommand(
            Cmd::new("set")
                .about("Change a property (not implemented)")
                .arg(Arg::new("key").value_name("KEY").required(true))
                .arg(Arg::new("value").value_name("VALUE").required(true))
                .after_help(
                    "Examples:\n  img.vmdk disk.vmdk set virtual_size 20G\n\n\
                     Answers `not implemented` (exit 3): the library changes no descriptor \
                     field.",
                ),
        )
        .subcommand(
            Cmd::new("resize")
                .about("Grow or shrink the virtual disk (not implemented)")
                .arg(Arg::new("size").value_name("SIZE").required(true))
                .after_help(
                    "Examples:\n  img.vmdk disk.vmdk resize 20G\n\n\
                     Answers `not implemented` (exit 3): the library has no resize.",
                ),
        )
        .after_help(
            "Examples:\n  img.vmdk disk.vmdk info\n  \
             img.vmdk disk.vmdk get virtual_size --text\n  \
             img.vmdk disk.vmdk read -o disk.raw\n  \
             img.vmdk disk.vmdk read --offset 0 --length 512 | xxd",
        )
}

fn byte_count(id: &'static str, help: &'static str) -> Arg {
    Arg::new(id)
        .long(id)
        .value_name("BYTES")
        .help(help)
        .value_parser(super::size::parse)
}

fn key_command(name: &'static str, about: &'static str) -> Cmd {
    Cmd::new(name)
        .about(about)
        .arg(
            Arg::new("key")
                .value_name("KEY")
                .help(format!("One of: {} (or vmdk.<field>)", KEYS.join(", "))),
        )
        .after_help(format!(
            "Examples:\n  img.vmdk disk.vmdk {name}\n  \
             img.vmdk disk.vmdk {name} virtual_size --text\n  \
             img.vmdk disk.vmdk {name} vmdk.create_type"
        ))
}

fn run(matches: &ArgMatches) -> Result<Outcome, CliError> {
    let image = Path::new(
        matches
            .get_one::<OsString>("image")
            .expect("clap requires the image"),
    );
    let (verb, sub) = matches.subcommand().expect("clap requires a verb");
    match verb {
        "info" | "get" => get(image, sub.get_one::<String>("key").map(String::as_str)),
        "read" => read(
            image,
            sub.get_one::<u64>("offset").copied(),
            sub.get_one::<u64>("length").copied(),
            sub.get_one::<OsString>("output").map(PathBuf::from),
        ),
        "write" => Err(CliError::not_implemented(
            "write: this version of img.vmdk does not write yet",
        )),
        "create" => Err(CliError::not_implemented(
            "create: this library has no VMDK image creator",
        )),
        "set" => Err(CliError::not_implemented(
            "set: this library changes no VMDK descriptor field",
        )),
        "resize" => Err(CliError::not_implemented(
            "resize: this library cannot resize a VMDK image",
        )),
        other => unreachable!("clap knows no verb {other}"),
    }
}

/// The library's refusal of something the image is — a flat, split or
/// VMFS layout, a child image with a parent — is a verb this tool cannot
/// do (exit 3); anything else failed.
fn vmdk_error(image: &Path, e: vmdk::Error) -> CliError {
    match e {
        vmdk::Error::Unsupported(_) => {
            CliError::not_implemented(format!("{}: {e}", image.display()))
        }
        other => CliError::failed(format!("{}: {other}", image.display())),
    }
}

/// The image, opened read-only: nothing `info` or `read` does writes it.
fn open(image: &Path) -> Result<VmdkReader, CliError> {
    VmdkReader::open(image).map_err(|e| vmdk_error(image, e))
}

/// The envelope: the shared keys first, the format's own under `vmdk`.
fn envelope(r: &VmdkReader) -> Json {
    let h = r.header();
    let d = r.descriptor();
    Json::object([
        ("format", Json::from("vmdk")),
        ("virtual_size", Json::from(r.virtual_size())),
        ("block_size", Json::from(r.grain_size_bytes())),
        // A child image (one naming a parent) is refused at open, so every
        // image this reports on stands alone.
        ("backing", Json::Null),
        // `uncleanShutdown`: a writer raised it and did not get to lower
        // it, so the image may be half-written.
        ("dirty", Json::from(h.unclean_shutdown != 0)),
        (
            "vmdk",
            Json::object([
                ("create_type", Json::from(d.create_type.as_str())),
                ("version", Json::from(h.version)),
                (
                    "compression",
                    Json::from(if h.is_stream_optimized() {
                        "deflate"
                    } else {
                        "none"
                    }),
                ),
                ("zeroed_grain", Json::from(h.uses_zeroed_grain_marker())),
                (
                    "redundant_grain_directory",
                    Json::from(h.has_redundant_grain_directory()),
                ),
                ("grain_table_entries", Json::from(h.num_gtes_per_gt)),
                ("extents", Json::from(d.extents.len() as u64)),
            ]),
        ),
    ])
}

fn get(image: &Path, key: Option<&str>) -> Result<Outcome, CliError> {
    let reader = open(image)?;
    let all = envelope(&reader);
    let Some(key) = key else {
        return Ok(Outcome::report(all));
    };
    let mut value = Some(&all);
    for part in key.split('.') {
        value = value.and_then(|v| v.get(part));
    }
    let Some(value) = value else {
        return Err(CliError::usage(format!(
            "no key {key:?}; the keys are {} (and vmdk.<field>)",
            KEYS.join(", ")
        )));
    };
    let text = value.to_text();
    Ok(Outcome::report(Json::object([(key, value.clone())])).with_text(text))
}

/// The range a `read` covers: `offset` (default 0) for `length` bytes
/// (default: to the end), refused whole if any of it is past the end, so
/// nothing is written for a range that cannot be served.
fn range(size: u64, offset: Option<u64>, length: Option<u64>) -> Result<(u64, u64), CliError> {
    let offset = offset.unwrap_or(0);
    if offset > size {
        return Err(CliError::failed(format!(
            "--offset {offset} is past the end of the {size}-byte virtual disk"
        )));
    }
    let length = length.unwrap_or(size - offset);
    if offset.checked_add(length).is_none_or(|end| end > size) {
        return Err(CliError::failed(format!(
            "{length} bytes at {offset} run past the end of the {size}-byte virtual disk"
        )));
    }
    Ok((offset, length))
}

/// Stream the guest's bytes. Each chunk is read before it is written, so
/// an image that turns out unreadable part-way stops with status 1; what
/// can be refused up front (no image, a range past the end) is refused
/// before a byte is written. `-o FILE` writes `FILE.partial` and renames
/// it, so FILE is never left half written, and skips runs of zeros so a
/// mostly empty disk makes a sparse file.
fn read(
    image: &Path,
    offset: Option<u64>,
    length: Option<u64>,
    output: Option<PathBuf>,
) -> Result<Outcome, CliError> {
    let r = &open(image)?;
    let (offset, length) = range(r.virtual_size(), offset, length)?;
    let mut buf = vec![0u8; CHUNK];
    match output {
        None => {
            let mut out = std::io::stdout().lock();
            let mut at = offset;
            let end = offset + length;
            while at < end {
                let n = CHUNK.min((end - at) as usize);
                r.read_at(at, &mut buf[..n])
                    .map_err(|e| vmdk_error(image, e))?;
                if let Err(e) = out.write_all(&buf[..n]) {
                    // A closed pipe (`| head -c 512`) is the reader's
                    // choice, not a failure.
                    if e.kind() == std::io::ErrorKind::BrokenPipe {
                        return Ok(Outcome::done());
                    }
                    return Err(CliError::failed(format!("write stdout: {e}")));
                }
                at += n as u64;
            }
            if let Err(e) = out.flush() {
                if e.kind() != std::io::ErrorKind::BrokenPipe {
                    return Err(CliError::failed(format!("write stdout: {e}")));
                }
            }
        }
        Some(dest) => {
            let mut partial = dest.as_os_str().to_owned();
            partial.push(".partial");
            let partial = PathBuf::from(partial);
            let copied = (|| -> Result<(), CliError> {
                let io = |e: std::io::Error| {
                    CliError::failed(format!("write {}: {e}", partial.display()))
                };
                let mut f = std::fs::File::create(&partial).map_err(io)?;
                let mut at = offset;
                let end = offset + length;
                while at < end {
                    let n = CHUNK.min((end - at) as usize);
                    r.read_at(at, &mut buf[..n])
                        .map_err(|e| vmdk_error(image, e))?;
                    if buf[..n].iter().all(|b| *b == 0) {
                        f.seek(SeekFrom::Current(n as i64)).map_err(io)?;
                    } else {
                        f.write_all(&buf[..n]).map_err(io)?;
                    }
                    at += n as u64;
                }
                // A trailing run of zeros was skipped, not written: the
                // length is set, not implied.
                f.set_len(length).map_err(io)?;
                f.sync_all().map_err(io)
            })();
            if let Err(e) = copied {
                let _ = std::fs::remove_file(&partial);
                return Err(e);
            }
            std::fs::rename(&partial, &dest)
                .map_err(|e| CliError::failed(format!("rename to {}: {e}", dest.display())))?;
        }
    }
    Ok(Outcome::done())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_range_defaults_to_the_whole_disk_and_refuses_the_far_side() {
        assert_eq!(range(1000, None, None).ok(), Some((0, 1000)));
        assert_eq!(range(1000, Some(10), None).ok(), Some((10, 990)));
        assert_eq!(range(1000, None, Some(10)).ok(), Some((0, 10)));
        assert_eq!(range(1000, Some(1000), None).ok(), Some((1000, 0)));
        assert!(range(1000, Some(1001), None).is_err());
        assert!(range(1000, Some(990), Some(11)).is_err());
        assert!(range(1000, Some(u64::MAX), Some(2)).is_err());
    }

    #[test]
    fn what_the_library_refuses_is_not_implemented_and_the_rest_failed() {
        let p = Path::new("x.vmdk");
        assert_eq!(
            vmdk_error(p, vmdk::Error::Unsupported("a split sparse extent")).code,
            crate::common::output::EXIT_UNSUPPORTED
        );
        assert!(
            vmdk_error(p, vmdk::Error::Unsupported("a split sparse extent"))
                .message
                .starts_with("not implemented: x.vmdk: ")
        );
        assert_eq!(
            vmdk_error(p, vmdk::Error::NotVmdk).code,
            crate::common::output::EXIT_FAILED
        );
        assert_eq!(
            vmdk_error(p, vmdk::Error::Corrupt("grain directory")).code,
            crate::common::output::EXIT_FAILED
        );
    }
}
