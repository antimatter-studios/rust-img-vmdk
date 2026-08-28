//! Embedded text descriptor for monolithic sparse VMDK.
//!
//! The descriptor is plain ASCII, key=value lines plus the extent
//! description. We only care about three things:
//!
//! - `createType="..."` — must be `monolithicSparse`. Anything else is
//!   reported as [`Error::Unsupported`] so callers can fall back.
//! - The extent line: `RW <sectors> SPARSE "<filename>"`. We stash
//!   sector count and filename for sanity.
//! - The parent linkage: `parentFileNameHint` / `parentCID`. A snapshot
//!   delta or linked clone is `monolithicSparse` like any other image, so
//!   the parent linkage is the only thing that says the data isn't all
//!   here. See [`declares_parent`].
//! - `ddb.*` lines (geometry, etc.) are ignored.

use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct Descriptor {
    pub create_type: String,
    pub extents: Vec<Extent>,
}

#[derive(Debug, Clone)]
pub struct Extent {
    pub access: String,
    pub sectors: u64,
    pub kind: String,
    pub filename: String,
}

impl Descriptor {
    /// Parse the descriptor text (NUL-padded okay). Reject anything the
    /// reader cannot serve truthfully — a `createType` other than
    /// `monolithicSparse`, or a descriptor that declares a parent — so the
    /// caller gets [`Error::Unsupported`] and can fall back.
    pub fn parse(text: &str) -> Result<Self> {
        let mut create_type: Option<String> = None;
        let mut parent_file_name_hint: Option<String> = None;
        let mut parent_cid: Option<String> = None;
        let mut extents: Vec<Extent> = Vec::new();

        for raw in text.lines() {
            let line = raw.trim();
            // Strip trailing NULs (descriptor is sector-padded).
            let line = line.trim_end_matches('\0').trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            if let Some(value) = parse_kv(line, "createType") {
                create_type = Some(value);
                continue;
            }

            if let Some(value) = parse_kv(line, "parentFileNameHint") {
                parent_file_name_hint = Some(value);
                continue;
            }

            if let Some(value) = parse_kv(line, "parentCID") {
                parent_cid = Some(value);
                continue;
            }

            // Extent line: starts with RW / RDONLY / NOACCESS.
            if let Some(extent) = parse_extent(line) {
                extents.push(extent);
                continue;
            }

            // Ignore everything else (ddb.*, encoding, version, own CID, ...).
        }

        let create_type = create_type.ok_or(Error::Corrupt("descriptor missing createType"))?;

        if create_type != "monolithicSparse" {
            // Map known variants to a stable message so callers can log.
            let msg: &'static str = match create_type.as_str() {
                "monolithicFlat" => "monolithicFlat",
                "twoGbMaxExtentSparse" => "twoGbMaxExtentSparse",
                "twoGbMaxExtentFlat" => "twoGbMaxExtentFlat",
                "vmfs" => "vmfs",
                "vmfsSparse" => "vmfsSparse",
                "streamOptimized" => "streamOptimized",
                "fullDevice" => "fullDevice",
                "partitionedDevice" => "partitionedDevice",
                _ => "unknown createType",
            };
            return Err(Error::Unsupported(msg));
        }

        // A delta gets this far because it *is* monolithicSparse. Its data
        // is split between this file and the parent chain, and we can only
        // see this file — so opening it would serve zeros for everything
        // the parent still owns.
        if declares_parent(parent_file_name_hint.as_deref(), parent_cid.as_deref()) {
            return Err(Error::Unsupported(DELTA_UNSUPPORTED));
        }

        Ok(Descriptor {
            create_type,
            extents,
        })
    }
}

/// The `parentCID` every standalone disk carries: the sentinel meaning
/// "no parent". Any other value names a parent this reader cannot reach.
const NO_PARENT_CID: &str = "ffffffff";

/// What [`Error::Unsupported`] says about a delta. The error type carries
/// `&'static str`, so the filename can't be interpolated — the message
/// instead names the fields to look at and the capability that is missing.
const DELTA_UNSUPPORTED: &str = "delta/child disk — the descriptor names a parent \
     (parentFileNameHint/parentCID), so part of the data lives in the parent chain; \
     reading it needs parent-chain following, which this crate does not implement";

/// Whether the descriptor says this image is a snapshot delta or linked
/// clone: it names a parent file, or carries a `parentCID` other than the
/// [`NO_PARENT_CID`] sentinel. Absent or empty values mean standalone, and
/// so does a `0x`-prefixed spelling of the sentinel — a false positive here
/// would refuse a perfectly ordinary disk.
fn declares_parent(hint: Option<&str>, cid: Option<&str>) -> bool {
    let names_a_parent_file = hint.map(str::trim).is_some_and(|h| !h.is_empty());

    let carries_a_child_cid = cid.map(str::trim).is_some_and(|c| {
        let c = c
            .strip_prefix("0x")
            .or_else(|| c.strip_prefix("0X"))
            .unwrap_or(c);
        !c.is_empty() && !c.eq_ignore_ascii_case(NO_PARENT_CID)
    });

    names_a_parent_file || carries_a_child_cid
}

/// Match `key="value"` (or `key=value`) and return the value if `key`
/// matches.
fn parse_kv(line: &str, key: &str) -> Option<String> {
    let (k, v) = line.split_once('=')?;
    if k.trim() != key {
        return None;
    }
    let v = v.trim().trim_matches('"');
    Some(v.to_string())
}

/// Parse `RW 2048 SPARSE "image.vmdk"` style line.
fn parse_extent(line: &str) -> Option<Extent> {
    let mut parts = line.split_whitespace();
    let access = parts.next()?;
    if !matches!(access, "RW" | "RDONLY" | "NOACCESS") {
        return None;
    }
    let sectors_s = parts.next()?;
    let kind = parts.next()?;
    let filename_raw = parts.next()?;
    let sectors: u64 = sectors_s.parse().ok()?;
    let filename = filename_raw.trim_matches('"').to_string();
    Some(Extent {
        access: access.to_string(),
        sectors,
        kind: kind.to_string(),
        filename,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_monolithic_sparse() {
        let text = "# Disk DescriptorFile\n\
                    version=1\n\
                    CID=fffffffe\n\
                    parentCID=ffffffff\n\
                    createType=\"monolithicSparse\"\n\
                    \n\
                    RW 2048 SPARSE \"test.vmdk\"\n\
                    \n\
                    ddb.geometry.cylinders = \"2\"\n";
        let d = Descriptor::parse(text).unwrap();
        assert_eq!(d.create_type, "monolithicSparse");
        assert_eq!(d.extents.len(), 1);
        assert_eq!(d.extents[0].sectors, 2048);
        assert_eq!(d.extents[0].kind, "SPARSE");
        assert_eq!(d.extents[0].filename, "test.vmdk");
    }

    #[test]
    fn rejects_flat() {
        let text = "createType=\"monolithicFlat\"\nRW 2048 FLAT \"x.vmdk\" 0\n";
        assert!(matches!(
            Descriptor::parse(text),
            Err(Error::Unsupported("monolithicFlat"))
        ));
    }

    #[test]
    fn missing_create_type_is_corrupt() {
        let text = "version=1\nRW 2048 SPARSE \"x.vmdk\"\n";
        assert!(matches!(Descriptor::parse(text), Err(Error::Corrupt(_))));
    }

    #[test]
    fn rejects_every_known_unsupported_variant_with_stable_message() {
        for (ct, msg) in [
            ("monolithicFlat", "monolithicFlat"),
            ("twoGbMaxExtentSparse", "twoGbMaxExtentSparse"),
            ("twoGbMaxExtentFlat", "twoGbMaxExtentFlat"),
            ("vmfs", "vmfs"),
            ("vmfsSparse", "vmfsSparse"),
            ("streamOptimized", "streamOptimized"),
            ("fullDevice", "fullDevice"),
            ("partitionedDevice", "partitionedDevice"),
        ] {
            let text = format!("createType=\"{ct}\"\n");
            match Descriptor::parse(&text) {
                Err(Error::Unsupported(got)) => assert_eq!(got, msg, "for {ct}"),
                other => panic!("expected Unsupported({msg}) for {ct}, got {other:?}"),
            }
        }
    }

    #[test]
    fn unknown_create_type_maps_to_stable_message() {
        let text = "createType=\"someFutureType\"\n";
        match Descriptor::parse(text) {
            Err(Error::Unsupported(msg)) => assert_eq!(msg, "unknown createType"),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn tolerates_nul_padding_and_comment_lines() {
        // Descriptor sector is NUL-padded on disk; comments and blank
        // lines must be skipped.
        let mut text = String::from(
            "# Disk DescriptorFile\n\
             createType=\"monolithicSparse\"\n\
             \n\
             RW 2048 SPARSE \"x.vmdk\"\n",
        );
        text.push_str("\0\0\0\0\0\0");
        let d = Descriptor::parse(&text).unwrap();
        assert_eq!(d.create_type, "monolithicSparse");
        assert_eq!(d.extents.len(), 1);
    }

    #[test]
    fn parses_readonly_and_noaccess_extent_access_modes() {
        let text = "createType=\"monolithicSparse\"\n\
                    RDONLY 2048 SPARSE \"a.vmdk\"\n\
                    NOACCESS 1024 SPARSE \"b.vmdk\"\n";
        let d = Descriptor::parse(text).unwrap();
        assert_eq!(d.extents.len(), 2);
        assert_eq!(d.extents[0].access, "RDONLY");
        assert_eq!(d.extents[1].access, "NOACCESS");
    }

    #[test]
    fn ignores_malformed_extent_line_with_non_numeric_sectors() {
        // "RW xyz SPARSE ..." has a non-numeric sector count; parse_extent
        // returns None and the line is silently ignored, leaving zero
        // extents (but a valid createType).
        let text = "createType=\"monolithicSparse\"\nRW xyz SPARSE \"x.vmdk\"\n";
        let d = Descriptor::parse(text).unwrap();
        assert_eq!(d.create_type, "monolithicSparse");
        assert!(d.extents.is_empty());
    }

    #[test]
    fn accepts_create_type_without_quotes() {
        let d = Descriptor::parse("createType=monolithicSparse\n").unwrap();
        assert_eq!(d.create_type, "monolithicSparse");
    }

    #[test]
    fn refuses_delta_disk_named_by_parent_file_name_hint() {
        // A VMware snapshot delta is `monolithicSparse` too — the only
        // thing that distinguishes it is the parent linkage. Opening it as
        // a standalone disk reads every unowned grain as zeros, so it must
        // be refused rather than half-read.
        let text = "# Disk DescriptorFile\n\
                    version=1\n\
                    CID=fffffffe\n\
                    parentCID=fffffffd\n\
                    createType=\"monolithicSparse\"\n\
                    parentFileNameHint=\"base.vmdk\"\n\
                    \n\
                    RW 2048 SPARSE \"base-000001.vmdk\"\n";
        match Descriptor::parse(text) {
            Err(Error::Unsupported(msg)) => {
                assert!(
                    msg.contains("parent"),
                    "message must name the parent: {msg}"
                )
            }
            other => panic!("expected Unsupported for a delta disk, got {other:?}"),
        }
    }

    #[test]
    fn refuses_delta_disk_identified_only_by_parent_cid() {
        // Some producers omit the hint but still record a child CID.
        let text = "createType=\"monolithicSparse\"\n\
                    parentCID=1a2b3c4d\n\
                    RW 2048 SPARSE \"child.vmdk\"\n";
        match Descriptor::parse(text) {
            Err(Error::Unsupported(msg)) => {
                assert!(
                    msg.contains("parent"),
                    "message must name the parent: {msg}"
                )
            }
            other => panic!("expected Unsupported for a child CID, got {other:?}"),
        }
    }

    #[test]
    fn standalone_disk_with_sentinel_parent_cid_still_parses() {
        // `ffffffff` is the "no parent" sentinel every standalone VMware
        // disk carries, in either spelling; refusing it would break every
        // ordinary image.
        for cid in ["ffffffff", "FFFFFFFF", "0xffffffff"] {
            let text = format!(
                "createType=\"monolithicSparse\"\nparentCID={cid}\nRW 2048 SPARSE \"x.vmdk\"\n"
            );
            let d = Descriptor::parse(&text)
                .unwrap_or_else(|e| panic!("parentCID={cid} must parse, got {e:?}"));
            assert_eq!(d.extents.len(), 1);
        }
    }

    #[test]
    fn empty_parent_file_name_hint_is_not_a_parent() {
        let text = "createType=\"monolithicSparse\"\n\
                    parentFileNameHint=\"\"\n\
                    RW 2048 SPARSE \"x.vmdk\"\n";
        let d = Descriptor::parse(text).unwrap();
        assert_eq!(d.extents.len(), 1);
    }
}
