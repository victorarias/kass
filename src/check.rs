use crate::jev::Client;
use crate::rules::{Rule, Tier};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

// Jev allows 1,200 requests/min; at ~0.3s per request, 4 workers stay near 13/s.
const WORKERS: usize = 4;

pub struct FileResult {
    /// Path relative to the repo root, as the rules' globs see it.
    pub path: String,
    pub bytes: i64,
    pub outcome: FileOutcome,
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

/// Judges each file against the rules whose globs match it, one Jev request
/// per file. Files no rule matches are skipped and not returned.
pub fn run(client: &Client, root: &Path, rel_paths: &[String], rules: &[Rule]) -> Vec<FileResult> {
    let work: Vec<(&String, Vec<&Rule>)> = rel_paths
        .iter()
        .map(|p| {
            (
                p,
                rules
                    .iter()
                    .filter(|r| r.matches(Path::new(p)))
                    .collect::<Vec<_>>(),
            )
        })
        .filter(|(_, rs)| !rs.is_empty())
        .collect();
    let queue = Mutex::new(work.into_iter());
    let results = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..WORKERS {
            s.spawn(|| {
                loop {
                    let Some((path, matched)) = queue.lock().unwrap().next() else {
                        break;
                    };
                    let result = judge_file(client, root, path, &matched);
                    results.lock().unwrap().push(result);
                }
            });
        }
    });
    let mut results = results.into_inner().unwrap();
    results.sort_by(|a, b| a.path.cmp(&b.path));
    results
}

fn judge_file(client: &Client, root: &Path, path: &str, rules: &[&Rule]) -> FileResult {
    let content = match std::fs::read(root.join(path)) {
        Ok(bytes) => bytes,
        Err(e) => return failed(path, 0, format!("reading file: {e}"), None),
    };
    let bytes = content.len() as i64;
    let Ok(content) = String::from_utf8(content) else {
        return failed(
            path,
            bytes,
            "not UTF-8 text; Jev only reads text".into(),
            None,
        );
    };
    let state = json!({"path": path, "content": content});
    let questions: BTreeMap<String, String> = rules
        .iter()
        .map(|r| (r.id.clone(), r.question.clone()))
        .collect();
    let started = Instant::now();
    let result = client.noul(&state, &questions);
    let latency_ms = started.elapsed().as_millis() as i64;
    match result {
        Ok(eval) => {
            let judgments = rules
                .iter()
                .map(|r| {
                    let p = eval.answers[&r.id];
                    Judgment {
                        rule: (*r).clone(),
                        probability: p,
                        tier: r.tier(p),
                    }
                })
                .collect();
            FileResult {
                path: path.into(),
                bytes,
                outcome: FileOutcome::Judged {
                    model: eval.model,
                    input_tokens: eval.input_tokens,
                    latency_ms,
                    judgments,
                },
            }
        }
        Err(e) => failed(path, bytes, format!("{e:#}"), Some(latency_ms)),
    }
}

fn failed(path: &str, bytes: i64, error: String, latency_ms: Option<i64>) -> FileResult {
    FileResult {
        path: path.into(),
        bytes,
        outcome: FileOutcome::Failed { error, latency_ms },
    }
}
