use crate::jev::Client;
use crate::rules::{Rule, Tier};
use crate::units::{self, Decl, Lang};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

// Jev allows 1,200 requests/min; at ~0.3s per request, 5 workers stay near 1,000/min.
const WORKERS: usize = 5;
// attn tests average 380 tokens/KiB (1,070 files, 2026-09-26) and Jev caps state
// at 32k tokens, so 60KB (~22k tokens) leaves room for denser code.
pub const STATE_BUDGET_BYTES: usize = 60 * 1024;

/// Line ranges changed per file; `None` means judge everything in the file.
pub type ChangedLines = HashMap<String, Option<Vec<(usize, usize)>>>;

pub struct FileResult {
    /// Path relative to the repo root, as the rules' globs see it.
    pub path: String,
    /// The test judged, or `None` when the whole file was sent.
    pub unit: Option<UnitRef>,
    /// Bytes of state sent to Jev.
    pub bytes: i64,
    pub helpers: usize,
    pub helpers_omitted: usize,
    pub outcome: FileOutcome,
}

#[derive(Clone)]
pub struct UnitRef {
    pub name: String,
    pub line: usize,
}

pub enum FileOutcome {
    Judged {
        model: String,
        input_tokens: Option<i64>,
        latency_ms: i64,
        judgments: Vec<Judgment>,
    },
    Failed {
        error: String,
        latency_ms: Option<i64>,
    },
}

pub struct Judgment {
    pub rule: Rule,
    pub probability: f64,
    pub tier: Tier,
}

struct Target<'r> {
    path: String,
    unit: Option<UnitRef>,
    state: Value,
    bytes: usize,
    helpers: usize,
    helpers_omitted: usize,
    rules: Vec<&'r Rule>,
}

/// Judges each matching file, one Jev request per test (or per file when it
/// has no tests kass can find). `changed` limits which tests are judged.
pub fn run(
    client: &Client,
    root: &Path,
    rel_paths: &[String],
    rules: &[Rule],
    changed: Option<&ChangedLines>,
) -> Vec<FileResult> {
    let mut results = Vec::new();
    let mut targets = Vec::new();
    let mut siblings = SiblingDecls::default();
    for path in rel_paths {
        let matched: Vec<&Rule> = rules
            .iter()
            .filter(|r| r.matches(Path::new(path)))
            .collect();
        if matched.is_empty() {
            continue;
        }
        let ranges = changed.and_then(|c| c.get(path)).and_then(Option::as_deref);
        match plan_file(root, path, matched, ranges, &mut siblings) {
            Ok(t) => targets.extend(t),
            Err(error) => results.push(FileResult {
                path: path.clone(),
                unit: None,
                bytes: 0,
                helpers: 0,
                helpers_omitted: 0,
                outcome: FileOutcome::Failed {
                    error,
                    latency_ms: None,
                },
            }),
        }
    }

    let queue = Mutex::new(targets.into_iter());
    let judged = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..WORKERS {
            s.spawn(|| {
                loop {
                    let Some(target) = queue.lock().unwrap().next() else {
                        break;
                    };
                    let result = judge(client, target);
                    judged.lock().unwrap().push(result);
                }
            });
        }
    });
    results.extend(judged.into_inner().unwrap());
    results.sort_by(|a, b| {
        (&a.path, a.unit.as_ref().map(|u| u.line)).cmp(&(&b.path, b.unit.as_ref().map(|u| u.line)))
    });
    results
}

fn plan_file<'r>(
    root: &Path,
    path: &str,
    rules: Vec<&'r Rule>,
    ranges: Option<&[(usize, usize)]>,
    siblings: &mut SiblingDecls,
) -> Result<Vec<Target<'r>>, String> {
    let bytes = std::fs::read(root.join(path)).map_err(|e| format!("reading file: {e}"))?;
    let content =
        String::from_utf8(bytes).map_err(|_| "not UTF-8 text; Jev only reads text".to_string())?;
    let parsed =
        Lang::for_path(path).and_then(|lang| Some((lang, units::parse(lang, path, &content)?)));
    let Some((lang, parsed)) = parsed.filter(|(_, p)| !p.units.is_empty()) else {
        let state = json!({"path": path, "content": content});
        let bytes = content.len();
        return Ok(vec![Target {
            path: path.into(),
            unit: None,
            state,
            bytes,
            helpers: 0,
            helpers_omitted: 0,
            rules,
        }]);
    };
    let extra: &[Decl] = if lang == Lang::Go {
        siblings.get(root, path)
    } else {
        &[]
    };

    let mut out = Vec::new();
    for (i, unit) in parsed.units.iter().enumerate() {
        let helpers = units::helpers_for(&parsed, i, extra);
        let touched = |start: usize, end: usize| {
            ranges.is_none_or(|rs| rs.iter().any(|&(a, b)| a <= end && start <= b))
        };
        let same_file_helper_touched = helpers
            .iter()
            .any(|h| h.path == path && touched(h.start_line, h.end_line));
        if !touched(unit.start_line, unit.end_line) && !same_file_helper_touched {
            continue;
        }
        let mut size = path.len() + unit.name.len() + unit.code.len();
        let mut packed = Vec::new();
        for h in &helpers {
            if size + h.code.len() > STATE_BUDGET_BYTES {
                continue;
            }
            size += h.code.len();
            packed.push(json!({"name": h.name, "path": h.path, "code": h.code}));
        }
        let omitted = helpers.len() - packed.len();
        out.push(Target {
            path: path.into(),
            unit: Some(UnitRef { name: unit.name.clone(), line: unit.start_line }),
            state: json!({"path": path, "test": unit.name, "content": unit.code, "helpers": packed}),
            bytes: size,
            helpers: packed.len(),
            helpers_omitted: omitted,
            rules: rules.clone(),
        });
    }
    Ok(out)
}

/// Go helpers often live in sibling `_test.go` files of the same package.
#[derive(Default)]
struct SiblingDecls {
    by_dir: HashMap<String, Vec<(String, Vec<Decl>)>>,
    scratch: Vec<Decl>,
}

impl SiblingDecls {
    fn get(&mut self, root: &Path, path: &str) -> &[Decl] {
        let dir = Path::new(path)
            .parent()
            .map(|d| d.to_string_lossy().into_owned())
            .unwrap_or_default();
        let files = self.by_dir.entry(dir.clone()).or_insert_with(|| {
            let Ok(entries) = std::fs::read_dir(root.join(&dir)) else {
                return Vec::new();
            };
            let mut files: Vec<(String, Vec<Decl>)> = entries
                .filter_map(|e| e.ok())
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let rel = if dir.is_empty() {
                        name.clone()
                    } else {
                        format!("{dir}/{name}")
                    };
                    let src = name
                        .ends_with("_test.go")
                        .then(|| std::fs::read_to_string(e.path()).ok())
                        .flatten()?;
                    Some((rel.clone(), units::decls_only(Lang::Go, &rel, &src)))
                })
                .collect();
            files.sort_by(|a, b| a.0.cmp(&b.0));
            files
        });
        self.scratch = files
            .iter()
            .filter(|(f, _)| f != path)
            .flat_map(|(_, d)| d.iter().cloned())
            .collect();
        &self.scratch
    }
}

fn judge(client: &Client, t: Target) -> FileResult {
    let questions: BTreeMap<String, String> = t
        .rules
        .iter()
        .map(|r| (r.id.clone(), r.question.clone()))
        .collect();
    let started = Instant::now();
    let result = client.noul(&t.state, &questions);
    let latency_ms = started.elapsed().as_millis() as i64;
    let outcome = match result {
        Ok(eval) => FileOutcome::Judged {
            judgments: t
                .rules
                .iter()
                .map(|r| {
                    let p = eval.answers[&r.id];
                    Judgment {
                        rule: (*r).clone(),
                        probability: p,
                        tier: r.tier(p),
                    }
                })
                .collect(),
            model: eval.model,
            input_tokens: eval.input_tokens,
            latency_ms,
        },
        Err(e) => FileOutcome::Failed {
            error: format!("{e:#}"),
            latency_ms: Some(latency_ms),
        },
    };
    FileResult {
        path: t.path,
        unit: t.unit,
        bytes: t.bytes as i64,
        helpers: t.helpers,
        helpers_omitted: t.helpers_omitted,
        outcome,
    }
}
