//! Two helpers shared by the agent panel, the token panel and the
//! review-comment store: Claude Code's project-directory slug, and a compact
//! token formatter.

use std::path::Path;

/// Claude Code's per-project session directory name: the path with every
/// non-alphanumeric character replaced by '-'.
pub fn project_slug(root: &Path) -> String {
    root.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

/// A scratch directory of this workspace's own, handed to agents as
/// `$TIDE_TMP`.
///
/// Agents write files. A brief or a code tour is a JSON payload too long to
/// quote comfortably on a command line, so one writes it out and posts it with
/// `-d @file` instead — and left to itself it picks the obvious name. Two
/// sessions both chose `/tmp/brief.json`, the second overwrote the first, and
/// the brief that reached TIDE was for the wrong pull request.
///
/// The folder name is in the path to be recognised in a log; the hash is there
/// because two checkouts of the same repository have the same leaf name.
pub fn scratch(root: &Path) -> std::path::PathBuf {
    let leaf = root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let safe: String =
        leaf.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    let name = match safe.is_empty() {
        true => "workspace".to_string(),
        false => safe,
    };
    let tag: String = crate::jobs::fingerprint(&root.to_string_lossy()).chars().take(6).collect();
    std::path::PathBuf::from("/tmp/tide").join(format!("{name}-{tag}"))
}

/// `1.2M` / `847.3k` / `912` — compact enough for a topbar chip.
pub fn short(n: u64) -> String {
    match n {
        0..=9_999 => n.to_string(),
        10_000..=999_999 => format!("{:.1}k", n as f64 / 1_000.0),
        1_000_000..=999_999_999 => format!("{:.1}M", n as f64 / 1_000_000.0),
        _ => format!("{:.2}G", n as f64 / 1_000_000_000.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_reads_at_a_glance() {
        assert_eq!(short(912), "912");
        assert_eq!(short(847_300), "847.3k");
        assert_eq!(short(2_450_000), "2.5M");
        // a day of cache reads runs past a billion, and "1090.0M" is not a number
        assert_eq!(short(1_090_000_000), "1.09G");
    }

    #[test]
    fn the_slug_matches_claude_codes_project_dir_naming() {
        assert_eq!(project_slug(Path::new("/Users/a/dev/tide")), "-Users-a-dev-tide");
    }
}
