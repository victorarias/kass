use anyhow::{Context, Result, bail};
use globset::{Glob, GlobSet, GlobSetBuilder};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const DEFAULT_FAIL: f64 = 0.85;
pub const DEFAULT_WARN: f64 = 0.6;

#[derive(Debug, Clone)]
pub struct Rule {
    pub id: String,
    pub globs: Vec<String>,
    pub fail: f64,
    pub warn: f64,
    pub question: String,
    pub source: PathBuf,
    pub hash: String,
    matcher: GlobSet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Violation,
    Check,
    Pass,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Violation => "violation",
            Tier::Check => "check",
            Tier::Pass => "pass",
        }
    }
}

impl Rule {
    pub fn matches(&self, rel_path: &Path) -> bool {
        self.matcher.is_match(rel_path)
    }

    pub fn tier(&self, probability: f64) -> Tier {
        if probability >= self.fail {
            Tier::Violation
        } else if probability >= self.warn {
            Tier::Check
        } else {
            Tier::Pass
        }
    }
}

/// Parses one rules file. Each rule starts with `# <id>`, then `key: value`
/// lines (globs, fail, warn), a blank line, and the question text.
pub fn parse(source: &Path, text: &str) -> Result<Vec<Rule>> {
    let mut rules = Vec::new();
    let mut current: Option<(String, Vec<&str>)> = None;
    for line in text.lines() {
        if let Some(id) = line.strip_prefix("# ") {
            if let Some((id, body)) = current.take() {
                rules.push(build(source, id, &body)?);
            }
            current = Some((id.trim().to_string(), Vec::new()));
        } else if let Some((_, body)) = current.as_mut() {
            body.push(line);
        } else if !line.trim().is_empty() {
            bail!(
                "{}: text before the first `# <rule-id>` heading: {line:?}",
                source.display()
            );
        }
    }
    if let Some((id, body)) = current {
        rules.push(build(source, id, &body)?);
    }
    Ok(rules)
}

fn build(source: &Path, id: String, body: &[&str]) -> Result<Rule> {
    let at = || format!("{} rule `{id}`", source.display());
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        bail!(
            "{}: rule ids use lowercase letters, digits and dashes",
            at()
        );
    }
    let mut globs = Vec::new();
    let mut fail = DEFAULT_FAIL;
    let mut warn = DEFAULT_WARN;
    let mut lines = body.iter().peekable();
    while let Some(line) = lines.next_if(|l| !l.trim().is_empty()) {
        let (key, value) = line.split_once(':').with_context(|| {
            format!(
                "{}: expected `key: value` before the blank line, got {line:?}",
                at()
            )
        })?;
        let value = value.trim();
        match key.trim() {
            "globs" => globs.extend(
                value
                    .split(',')
                    .map(|g| g.trim().to_string())
                    .filter(|g| !g.is_empty()),
            ),
            "fail" => fail = parse_threshold(value).with_context(at)?,
            "warn" => warn = parse_threshold(value).with_context(at)?,
            other => bail!("{}: unknown key `{other}` (known: globs, fail, warn)", at()),
        }
    }
    let question = lines
        .copied()
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();
    if globs.is_empty() {
        bail!("{}: needs a `globs:` line", at());
    }
    if question.is_empty() {
        bail!("{}: needs a question after the blank line", at());
    }
    if warn > fail {
        bail!("{}: warn={warn} is above fail={fail}", at());
    }
    let mut builder = GlobSetBuilder::new();
    for g in &globs {
        builder.add(Glob::new(g).with_context(|| format!("{}: bad glob {g:?}", at()))?);
    }
    let hash = {
        let mut h = Sha256::new();
        h.update(format!("{globs:?}\n{fail}\n{warn}\n{question}"));
        h.finalize()
            .iter()
            .take(6)
            .map(|b| format!("{b:02x}"))
            .collect()
    };
    Ok(Rule {
        id,
        globs,
        fail,
        warn,
        question,
        source: source.to_path_buf(),
        hash,
        matcher: builder.build()?,
    })
}

fn parse_threshold(value: &str) -> Result<f64> {
    let v: f64 = value
        .parse()
        .with_context(|| format!("threshold {value:?} is not a number"))?;
    if !(0.0..=1.0).contains(&v) {
        bail!("threshold {v} is outside 0..=1");
    }
    Ok(v)
}

/// Loads every `*.md` in `dir`; a missing directory means no rules.
pub fn load(dir: &Path) -> Result<Vec<Rule>> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .collect();
    files.sort();
    let mut by_id: BTreeMap<String, Rule> = BTreeMap::new();
    for file in files {
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("reading {}", file.display()))?;
        for rule in parse(&file, &text)? {
            if let Some(prev) = by_id.get(&rule.id) {
                bail!(
                    "rule `{}` is defined in both {} and {}",
                    rule.id,
                    prev.source.display(),
                    file.display()
                );
            }
            by_id.insert(rule.id.clone(), rule);
        }
    }
    Ok(by_id.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
# no-sleep-in-tests
globs: **/*_test.go, **/*.test.ts
fail: 0.9

Does a test in `content` wait by sleeping?

# mock-only
globs: **/*_test.go
warn: 0.5

Does a test only assert on mocks?
";

    #[test]
    fn parses_rules_with_defaults_and_overrides() {
        let rules = parse(Path::new("r.md"), SAMPLE).unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].id, "no-sleep-in-tests");
        assert_eq!(rules[0].fail, 0.9);
        assert_eq!(rules[0].warn, DEFAULT_WARN);
        assert_eq!(
            rules[0].question,
            "Does a test in `content` wait by sleeping?"
        );
        assert!(rules[0].matches(Path::new("internal/x/foo_test.go")));
        assert!(rules[0].matches(Path::new("app/a.test.ts")));
        assert!(!rules[0].matches(Path::new("internal/x/foo.go")));
        assert_eq!(rules[1].tier(0.55), Tier::Check);
        assert_eq!(rules[1].tier(0.85), Tier::Violation);
        assert_eq!(rules[1].tier(0.2), Tier::Pass);
    }

    #[test]
    fn hash_changes_with_the_question() {
        let a = parse(Path::new("r.md"), "# a\nglobs: *\n\nQ one?").unwrap();
        let b = parse(Path::new("r.md"), "# a\nglobs: *\n\nQ two?").unwrap();
        assert_ne!(a[0].hash, b[0].hash);
    }

    #[test]
    fn errors_name_the_file_and_rule() {
        let cases = [
            ("# Bad Id\nglobs: *\n\nQ?", "lowercase"),
            ("# a\n\nQ?", "needs a `globs:` line"),
            ("# a\nglobs: *\n", "needs a question"),
            (
                "# a\nglobs: *\nfail: 0.5\nwarn: 0.7\n\nQ?",
                "warn=0.7 is above fail=0.5",
            ),
            ("# a\nglobs: *\nfial: 0.5\n\nQ?", "unknown key `fial`"),
            ("stray\n# a\nglobs: *\n\nQ?", "before the first"),
        ];
        for (text, want) in cases {
            let err = format!("{:#}", parse(Path::new("r.md"), text).unwrap_err());
            assert!(err.contains(want), "{text:?}: {err}");
            assert!(err.contains("r.md"), "{text:?}: {err}");
        }
    }

    #[test]
    fn loads_every_file_and_rejects_duplicate_ids() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "# x\nglobs: *\n\nX?").unwrap();
        std::fs::write(dir.path().join("b.md"), "# y\nglobs: *\n\nY?").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "ignored").unwrap();
        let ids: Vec<_> = load(dir.path())
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(ids, ["x", "y"]);

        std::fs::write(dir.path().join("c.md"), "# x\nglobs: *\n\nAgain?").unwrap();
        let err = load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("defined in both"), "{err}");
        assert!(load(&dir.path().join("missing")).unwrap().is_empty());
    }
}
