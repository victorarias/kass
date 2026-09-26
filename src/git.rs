use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::Command;

/// Files that differ from HEAD and still exist: staged, unstaged and
/// untracked (gitignored files excluded), as paths relative to `root`.
pub fn changed_files(root: &Path) -> Result<Vec<String>> {
    if !root.join(".git").exists() {
        bail!(
            "{} is not a git repository, so kass cannot tell what changed; use `kass check --all`",
            root.display()
        );
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .output()
        .context("running git status")?;
    if !out.status.success() {
        bail!(
            "git status failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(parse_porcelain(&String::from_utf8_lossy(&out.stdout))
        .into_iter()
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

#[cfg(test)]
mod tests {
    use super::*;

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
