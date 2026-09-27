use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::Command;

/// Where `root` sits inside its git repository ("" at the top, "pkg/a/" in a
/// subdirectory), or `None` outside one.
pub fn prefix(root: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--show-prefix"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Files under `root` that differ from HEAD and still exist: staged, unstaged
/// and untracked (gitignored files excluded), as paths relative to `root`.
pub fn changed_files(root: &Path) -> Result<Vec<String>> {
    let Some(prefix) = prefix(root) else {
        bail!(
            "{} is not a git repository, so kass cannot tell what changed; use `kass check --all`",
            root.display()
        );
    };
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--",
            ".",
        ])
        .output()
        .context("running git status")?;
    if !out.status.success() {
        bail!(
            "git status failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    // Porcelain paths are relative to the repository's top, not to `root`.
    Ok(parse_porcelain(&String::from_utf8_lossy(&out.stdout))
        .into_iter()
        .filter_map(|p| p.strip_prefix(&prefix).map(str::to_string))
        .filter(|p| root.join(p).is_file())
        .collect())
}

/// Parses `git status --porcelain=v1 -z`. A rename or copy entry is followed
/// by its source path, which is skipped.
fn parse_porcelain(out: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut entries = out.split('\0').filter(|e| !e.is_empty());
    while let Some(entry) = entries.next() {
        let (status, path) = entry.split_at(3.min(entry.len()));
        if status.starts_with('R') || status.starts_with('C') {
            entries.next();
        }
        paths.push(path.to_string());
    }
    paths.sort();
    paths.dedup();
    paths
}

/// Line ranges each file changed relative to HEAD, from `git diff -U0`.
/// Files absent from the diff (untracked, or no HEAD yet) map to `None`:
/// every line counts as changed.
pub fn changed_lines(root: &Path, files: &[String]) -> Result<crate::check::ChangedLines> {
    let mut out: crate::check::ChangedLines = files.iter().map(|f| (f.clone(), None)).collect();
    if files.is_empty() {
        return Ok(out);
    }
    let has_head = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--verify", "--quiet", "HEAD"])
        .output()
        .context("running git rev-parse")?
        .status
        .success();
    if !has_head {
        return Ok(out);
    }
    let diff = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "diff",
            "-U0",
            "--relative",
            "--no-color",
            "--no-ext-diff",
            "HEAD",
            "--",
        ])
        .args(files)
        .output()
        .context("running git diff")?;
    if !diff.status.success() {
        bail!(
            "git diff failed: {}",
            String::from_utf8_lossy(&diff.stderr).trim()
        );
    }
    for (path, ranges) in parse_diff(&String::from_utf8_lossy(&diff.stdout)) {
        if let Some(entry) = out.get_mut(&path) {
            *entry = Some(ranges);
        }
    }
    Ok(out)
}

/// New-side line ranges per file from a `-U0` diff. A pure deletion marks the
/// lines on either side of the cut.
fn parse_diff(diff: &str) -> Vec<(String, Vec<(usize, usize)>)> {
    let mut out: Vec<(String, Vec<(usize, usize)>)> = Vec::new();
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            out.push((path.to_string(), Vec::new()));
        } else if let Some(hunk) = line.strip_prefix("@@ ") {
            let Some(new) = hunk.split(' ').find_map(|p| p.strip_prefix('+')) else {
                continue;
            };
            let (start, count) = match new.split_once(',') {
                Some((s, c)) => (s.parse().unwrap_or(0), c.parse().unwrap_or(0)),
                None => (new.parse().unwrap_or(0), 1),
            };
            let range = if count == 0 {
                (start.max(1), start + 1)
            } else {
                (start, start + count - 1)
            };
            if let Some((_, ranges)) = out.last_mut() {
                ranges.push(range);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_hunks_become_new_side_ranges() {
        let diff = "diff --git a/a.go b/a.go\n--- a/a.go\n+++ b/a.go\n@@ -3 +3 @@ x\n-a\n+b\n@@ -10,0 +11,4 @@\n+x\n@@ -20,2 +24,0 @@\n-y\n";
        assert_eq!(
            parse_diff(diff),
            [("a.go".to_string(), vec![(3, 3), (11, 14), (24, 25)])]
        );
    }

    #[test]
    fn parses_all_change_kinds_and_skips_rename_sources() {
        let out =
            " M a.go\0M  b.go\0MM c.go\0R  new.go\0old.go\0?? d/e.go\0 D gone.go\0A  f g.go\0";
        assert_eq!(
            parse_porcelain(out),
            [
                "a.go", "b.go", "c.go", "d/e.go", "f g.go", "gone.go", "new.go"
            ]
        );
    }
}
