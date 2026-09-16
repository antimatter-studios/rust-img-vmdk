//! The changelog's shape, checked rather than remembered.
//!
//! `[Unreleased]` grew seven `### Fixed` headings, one per PR that added
//! a section instead of adding to the one already there, and a review bot
//! reported it five times before anyone acted (#71). A convention that
//! five accurate reports did not enforce is not going to enforce itself.

use std::collections::BTreeMap;

fn changelog() -> String {
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/CHANGELOG.md"))
        .expect("read CHANGELOG.md")
}

/// `## [section]` → how many times each `### Heading` appears in it.
fn heading_counts(text: &str) -> Vec<(String, BTreeMap<String, usize>)> {
    let mut out: Vec<(String, BTreeMap<String, usize>)> = Vec::new();
    for line in text.lines() {
        if let Some(section) = line.strip_prefix("## ") {
            out.push((section.trim().to_owned(), BTreeMap::new()));
        } else if let Some(heading) = line.strip_prefix("### ") {
            if let Some((_, counts)) = out.last_mut() {
                *counts.entry(heading.trim().to_owned()).or_default() += 1;
            }
        }
    }
    out
}

#[test]
fn every_release_section_has_each_heading_at_most_once() {
    let sections = heading_counts(&changelog());
    assert!(
        sections.iter().any(|(s, _)| s == "[Unreleased]"),
        "the scan found no [Unreleased] section, so it is not reading the file it guards"
    );
    let repeated: Vec<String> = sections
        .iter()
        .flat_map(|(section, counts)| {
            counts
                .iter()
                .filter(|(_, &n)| n > 1)
                .map(move |(h, n)| format!("{section}: ### {h} x{n}"))
        })
        .collect();
    assert!(
        repeated.is_empty(),
        "add entries under the heading that is already there: {repeated:?}"
    );
}

#[test]
fn the_scan_counts_a_repeated_heading() {
    let text = "## [Unreleased]\n\n### Fixed\n\n- a\n\n### Added\n\n- b\n\n### Fixed\n\n- c\n\n## [0.1.0]\n\n### Fixed\n";
    let sections = heading_counts(text);
    assert_eq!(sections[0].1["Fixed"], 2);
    assert_eq!(sections[1].1["Fixed"], 1);
}

/// Every released section has a link definition, and `[Unreleased]`
/// compares from the newest release rather than an older one.
#[test]
fn the_link_footer_names_every_release_and_unreleased_starts_at_the_newest() {
    let text = changelog();
    let releases: Vec<String> = text
        .lines()
        .filter_map(|l| l.strip_prefix("## ["))
        .filter_map(|l| l.split(']').next())
        .filter(|v| *v != "Unreleased")
        .map(str::to_owned)
        .collect();
    let newest = releases.first().expect("at least one release section");
    let want = format!("[Unreleased]: https://github.com/antimatter-studios/rust-img-vmdk/compare/v{newest}...HEAD");
    assert!(text.lines().any(|l| l == want), "missing or stale: {want}");
    for v in &releases {
        assert!(
            text.lines().any(|l| l.starts_with(&format!("[{v}]: "))),
            "no link definition for [{v}]"
        );
    }
}
