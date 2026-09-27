use crate::check::{FileOutcome, FileResult};
use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde::Serialize;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const SCHEMA: &str = "
CREATE TABLE runs (
  id INTEGER PRIMARY KEY,
  started_at_ms INTEGER NOT NULL,
  caller TEXT NOT NULL,       -- cli | hook
  repo TEXT NOT NULL,
  harness TEXT,               -- claude, codex, ... (hook runs)
  session_id TEXT,
  agent_model TEXT,
  tool TEXT                   -- the harness tool that triggered the run
);
CREATE TABLE requests (
  id INTEGER PRIMARY KEY,
  run_id INTEGER NOT NULL REFERENCES runs(id),
  path TEXT NOT NULL,
  bytes INTEGER NOT NULL,
  latency_ms INTEGER,
  jev_model TEXT,
  input_tokens INTEGER,
  error TEXT
);
CREATE TABLE judgments (
  request_id INTEGER NOT NULL REFERENCES requests(id),
  rule_id TEXT NOT NULL,
  rule_hash TEXT NOT NULL,
  rule_source TEXT NOT NULL,
  probability REAL NOT NULL,
  tier TEXT NOT NULL,
  fail_threshold REAL NOT NULL,
  warn_threshold REAL NOT NULL
);
CREATE INDEX judgments_rule ON judgments(rule_id);
";

// Version 2: requests judge one test at a time, with the helpers it calls.
const MIGRATE_V2: &str = "
ALTER TABLE requests ADD COLUMN test TEXT;       -- NULL when the whole file was sent
ALTER TABLE requests ADD COLUMN line INTEGER;
ALTER TABLE requests ADD COLUMN helpers INTEGER;
ALTER TABLE requests ADD COLUMN helpers_omitted INTEGER;
";

#[derive(Default)]
pub struct RunContext {
    pub caller: &'static str,
    pub repo: String,
    pub harness: Option<String>,
    pub session_id: Option<String>,
    pub agent_model: Option<String>,
    pub tool: Option<String>,
}

pub fn open(dir: &Path) -> Result<Connection> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join("stats.db");
    let conn = Connection::open(&path).with_context(|| format!("opening {}", path.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 1 {
        conn.execute_batch(&format!("BEGIN; {SCHEMA} PRAGMA user_version = 1; COMMIT;"))?;
    }
    if version < 2 {
        conn.execute_batch(&format!(
            "BEGIN; {MIGRATE_V2} PRAGMA user_version = 2; COMMIT;"
        ))?;
    }
    Ok(conn)
}

pub fn record(conn: &mut Connection, ctx: &RunContext, results: &[FileResult]) -> Result<()> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as i64;
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO runs (started_at_ms, caller, repo, harness, session_id, agent_model, tool) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![now, ctx.caller, ctx.repo, ctx.harness, ctx.session_id, ctx.agent_model, ctx.tool],
    )?;
    let run_id = tx.last_insert_rowid();
    for r in results {
        let (test, line) = (
            r.unit.as_ref().map(|u| &u.name),
            r.unit.as_ref().map(|u| u.line as i64),
        );
        let (helpers, omitted) = (r.helpers as i64, r.helpers_omitted as i64);
        let (latency_ms, model, input_tokens, error, judgments) = match &r.outcome {
            FileOutcome::Judged {
                model,
                input_tokens,
                latency_ms,
                judgments,
            } => (
                Some(*latency_ms),
                Some(model.as_str()),
                *input_tokens,
                None,
                judgments.as_slice(),
            ),
            FileOutcome::Failed { error, latency_ms } => {
                (*latency_ms, None, None, Some(error.as_str()), &[][..])
            }
        };
        tx.execute(
            "INSERT INTO requests (run_id, path, bytes, latency_ms, jev_model, input_tokens, error, test, line, helpers, helpers_omitted)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![run_id, r.path, r.bytes, latency_ms, model, input_tokens, error, test, line, helpers, omitted],
        )?;
        let request_id = tx.last_insert_rowid();
        for j in judgments {
            tx.execute(
                "INSERT INTO judgments VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    request_id,
                    j.rule.id,
                    j.rule.hash,
                    j.rule.source.display().to_string(),
                    j.probability,
                    j.tier.as_str(),
                    j.rule.fail,
                    j.rule.warn
                ],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

#[derive(Serialize)]
pub struct Summary {
    pub runs: i64,
    pub requests: i64,
    pub errors: i64,
    pub latency_ms_p50: Option<i64>,
    pub latency_ms_p95: Option<i64>,
    pub latency_ms_max: Option<i64>,
    pub bytes_max: Option<i64>,
    pub input_tokens_max: Option<i64>,
    /// Receipt for any future file-size limit: tokens Jev counted per byte sent.
    pub tokens_per_kib: Option<f64>,
    pub rules: Vec<RuleSummary>,
}

#[derive(Serialize)]
pub struct RuleSummary {
    pub rule_id: String,
    pub versions: i64,
    pub judged: i64,
    pub violations: i64,
    pub checks: i64,
    pub mean_probability: f64,
}

pub fn summarize(conn: &Connection, repo: Option<&str>) -> Result<Summary> {
    // Every query filters through the runs of the chosen repo (or all repos).
    let runs_filter = "(SELECT id FROM runs WHERE ?1 IS NULL OR repo = ?1)";
    let runs: i64 = conn.query_row(
        "SELECT count(*) FROM runs WHERE ?1 IS NULL OR repo = ?1",
        [repo],
        |r| r.get(0),
    )?;
    let (requests, errors, bytes_max, input_tokens_max, tokens_per_kib) = conn.query_row(
        &format!(
            "SELECT count(*), count(error), max(CASE WHEN error IS NULL THEN bytes END), max(input_tokens),
                    1024.0 * sum(input_tokens) / nullif(sum(CASE WHEN input_tokens IS NOT NULL THEN bytes END), 0)
             FROM requests WHERE run_id IN {runs_filter}"
        ),
        [repo],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;
    let mut latencies: Vec<i64> = conn
        .prepare(&format!("SELECT latency_ms FROM requests WHERE error IS NULL AND run_id IN {runs_filter} ORDER BY latency_ms"))?
        .query_map([repo], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    latencies.sort_unstable();
    let pct = |p: f64| {
        latencies
            .get(((latencies.len() as f64 - 1.0) * p).round() as usize)
            .copied()
    };
    let rules = conn
        .prepare(&format!(
            "SELECT j.rule_id, count(DISTINCT j.rule_hash), count(*),
                    sum(j.tier = 'violation'), sum(j.tier = 'check'), avg(j.probability)
             FROM judgments j JOIN requests q ON q.id = j.request_id
             WHERE q.run_id IN {runs_filter}
             GROUP BY j.rule_id ORDER BY j.rule_id"
        ))?
        .query_map([repo], |r| {
            Ok(RuleSummary {
                rule_id: r.get(0)?,
                versions: r.get(1)?,
                judged: r.get(2)?,
                violations: r.get(3)?,
                checks: r.get(4)?,
                mean_probability: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(Summary {
        runs,
        requests,
        errors,
        latency_ms_p50: pct(0.5),
        latency_ms_p95: pct(0.95),
        latency_ms_max: latencies.last().copied(),
        bytes_max,
        input_tokens_max,
        tokens_per_kib,
        rules,
    })
}
