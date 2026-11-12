//! Two helpers shared by the agent panel, the token panel and the
//! review-comment store: Claude Code's project-directory slug, and a compact
//! token formatter.

use std::path::Path;

/// Claude Code's per-project session directory name: the path with every
/// non-alphanumeric character replaced by '-'.
pub fn project_slug(root: &Path) -> String {
    root.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
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
