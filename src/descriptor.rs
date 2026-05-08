//! Embedded text descriptor for monolithic sparse VMDK.
//!
//! The descriptor is plain ASCII, key=value lines plus the extent
//! description. We only care about three things:
//!
//! - `createType="..."` — must be `monolithicSparse`. Anything else is
//!   reported as [`Error::Unsupported`] so callers can fall back.
//! - The extent line: `RW <sectors> SPARSE "<filename>"`. We stash
//!   sector count and filename for sanity.
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
    /// Parse the descriptor text (NUL-padded okay). Reject anything
    /// that isn't monolithicSparse so the caller can return Unsupported.
    pub fn parse(text: &str) -> Result<Self> {
        let mut create_type: Option<String> = None;
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

            // Extent line: starts with RW / RDONLY / NOACCESS.
            if let Some(extent) = parse_extent(line) {
                extents.push(extent);
                continue;
            }

            // Ignore everything else (ddb.*, encoding, version, CID, ...).
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

        Ok(Descriptor {
            create_type,
            extents,
        })
    }
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
}
