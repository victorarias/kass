mod check;
mod git;
mod jev;
mod rules;
mod stats;

use anyhow::{Context, Result, bail};
use check::{FileOutcome, FileResult};
use clap::{Parser, Subcommand};
use rules::Tier;
use serde_json::{Value, json};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const KEY_ENV: &str = "TYPESAFE_API_KEY";

/// Fuzzy linter: judges files against plain-language rules with TypeSafe's Jev.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Judge changed files (staged, unstaged or untracked) against the rules.
    Check {
        /// Limit the check to these files or directories.
        paths: Vec<PathBuf>,
        /// Judge every file, changed or not, honoring .gitignore.
        #[arg(long)]
        all: bool,
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
        /// Also print rules that passed.
        #[arg(long)]
        show_passes: bool,
    },
    /// List the rules that apply here and where each comes from.
    Rules,
    /// Summarize recorded judgments.
    Stats {
        /// Include every repo, not just the current one.
        #[arg(long)]
        global: bool,
        #[arg(long)]
        json: bool,
    },
    /// Run as a harness hook, reading the hook event from stdin.
    Hook {
        #[command(subcommand)]
        harness: Harness,
    },
}

#[derive(Subcommand)]
enum Harness {
    /// Claude Code PostToolUse hook for Edit, Write and MultiEdit.
    Claude,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Check {
            paths,
            all,
            json,
            show_passes,
        } => cmd_check(paths, all, json, show_passes),
        Command::Rules => cmd_rules(),
        Command::Stats { global, json } => cmd_stats(global, json),
        Command::Hook {
            harness: Harness::Claude,
        } => cmd_hook_claude(),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("kass: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn env_dir(var: &str, xdg: &str, fallback: &str) -> Result<PathBuf> {
    if let Some(v) = std::env::var_os(var) {
        return Ok(v.into());
    }
    if let Some(v) = std::env::var_os(xdg).filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(v).join("kass"));
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(fallback).join("kass"))
}

fn config_dir() -> Result<PathBuf> {
    env_dir("KASS_CONFIG_DIR", "XDG_CONFIG_HOME", ".config")
}

fn state_dir() -> Result<PathBuf> {
    env_dir("KASS_STATE_DIR", "XDG_STATE_HOME", ".local/state")
}

/// The nearest ancestor holding `.git`, or `start` itself outside a repo.
fn repo_root(start: &Path) -> PathBuf {
    start
        .ancestors()
        .find(|d| d.join(".git").exists())
        .unwrap_or(start)
        .to_path_buf()
}

fn rule_layers(root: &Path) -> Result<Vec<PathBuf>> {
    Ok(vec![
        config_dir()?.join("rules"),
        root.join(".kass").join("rules"),
    ])
}

fn client() -> Result<Option<jev::Client>> {
    let Some(key) = std::env::var(KEY_ENV).ok().filter(|k| !k.is_empty()) else {
        return Ok(None);
    };
    let url = std::env::var("KASS_JEV_URL").unwrap_or_else(|_| jev::DEFAULT_URL.into());
    let model = std::env::var("KASS_MODEL").unwrap_or_else(|_| jev::DEFAULT_MODEL.into());
    Ok(Some(jev::Client::new(url, key, model)?))
}

fn collect_files(root: &Path, cwd: &Path, paths: &[PathBuf]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let paths = if paths.is_empty() {
        vec![cwd.to_path_buf()]
    } else {
        paths.to_vec()
    };
    for p in paths {
        let abs = cwd.join(&p);
        if !abs.exists() {
            bail!("{} does not exist", p.display());
        }
        for entry in ignore::WalkBuilder::new(&abs).build() {
            let entry = entry?;
            if entry.file_type().is_some_and(|t| t.is_file()) {
                out.push(relative(root, entry.path())?);
            }
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

fn relative(root: &Path, path: &Path) -> Result<String> {
    let canon = path
        .canonicalize()
        .with_context(|| format!("resolving {}", path.display()))?;
    let rel = canon.strip_prefix(root).with_context(|| {
        format!(
            "{} is outside the repo at {}",
            path.display(),
            root.display()
        )
    })?;
    Ok(rel.to_string_lossy().into_owned())
}

fn record_stats(ctx: &stats::RunContext, results: &[FileResult]) {
    let written = state_dir().and_then(|dir| stats::record(&mut stats::open(&dir)?, ctx, results));
    if let Err(e) = written {
        eprintln!("kass: judgments were not recorded in stats: {e:#}");
    }
}

fn cmd_check(
    paths: Vec<PathBuf>,
    all_files: bool,
    as_json: bool,
    show_passes: bool,
) -> Result<ExitCode> {
    let client = client()?.with_context(|| {
        format!("{KEY_ENV} is not set; kass needs a TypeSafe API key to call Jev")
    })?;
    let cwd = std::env::current_dir()?.canonicalize()?;
    let root = repo_root(&cwd);
    let rules = rules::load(&rule_layers(&root)?)?;
    if rules.is_empty() {
        bail!(
            "no rules found; add *.md files to {} or {}",
            config_dir()?.join("rules").display(),
            root.join(".kass/rules").display()
        );
    }
    let files = if !all_files {
        let scopes = paths
            .iter()
            .map(|p| relative(&root, &cwd.join(p)))
            .collect::<Result<Vec<_>>>()?;
        git::changed_files(&root)?
            .into_iter()
            .filter(|f| {
                scopes.is_empty()
                    || scopes
                        .iter()
                        .any(|s| s.is_empty() || f == s || f.starts_with(&format!("{s}/")))
            })
            .collect()
    } else {
        collect_files(&root, &cwd, &paths)?
    };
    let results = check::run(&client, &root, &files, &rules);
    record_stats(
        &stats::RunContext {
            caller: "cli",
            repo: root.display().to_string(),
            tool: Some(if all_files { "check --all" } else { "check" }.into()),
            ..Default::default()
        },
        &results,
    );

    let (mut violations, mut checks, mut errors) = (0, 0, 0);
    for r in &results {
        match &r.outcome {
            FileOutcome::Judged { judgments, .. } => {
                violations += judgments
                    .iter()
                    .filter(|j| j.tier == Tier::Violation)
                    .count();
                checks += judgments.iter().filter(|j| j.tier == Tier::Check).count();
            }
            FileOutcome::Failed { .. } => errors += 1,
        }
    }
    if as_json {
        println!("{}", serde_json::to_string_pretty(&results_json(&results))?);
    } else {
        print_text(&results, show_passes);
        println!(
            "{} file(s) judged: {violations} violation(s), {checks} to double-check, {errors} error(s)",
            results.len()
        );
    }
    Ok(if errors > 0 {
        ExitCode::from(2)
    } else if violations > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

fn results_json(results: &[FileResult]) -> Value {
    results
        .iter()
        .map(|r| match &r.outcome {
            FileOutcome::Judged { model, input_tokens, latency_ms, judgments } => json!({
                "path": r.path, "bytes": r.bytes, "model": model, "input_tokens": input_tokens, "latency_ms": latency_ms,
                "judgments": judgments.iter().map(|j| json!({
                    "rule": j.rule.id, "probability": j.probability, "tier": j.tier.as_str(),
                    "fail": j.rule.fail, "warn": j.rule.warn,
                })).collect::<Vec<_>>(),
            }),
            FileOutcome::Failed { error, .. } => json!({"path": r.path, "bytes": r.bytes, "error": error}),
        })
        .collect()
}

fn print_text(results: &[FileResult], show_passes: bool) {
    for r in results {
        match &r.outcome {
            FileOutcome::Judged { judgments, .. } => {
                let shown: Vec<_> = judgments
                    .iter()
                    .filter(|j| show_passes || j.tier != Tier::Pass)
                    .collect();
                if shown.is_empty() {
                    continue;
                }
                println!("{}", r.path);
                for j in shown {
                    let line = match j.tier {
                        Tier::Violation => format!("fail >= {:.2}", j.rule.fail),
                        Tier::Check | Tier::Pass => format!("warn >= {:.2}", j.rule.warn),
                    };
                    println!(
                        "  {:<9}  {}  p={:.2} ({line})",
                        j.tier.as_str(),
                        j.rule.id,
                        j.probability
                    );
                }
            }
            FileOutcome::Failed { error, .. } => println!("{}\n  error      {error}", r.path),
        }
    }
}

fn cmd_rules() -> Result<ExitCode> {
    let root = repo_root(&std::env::current_dir()?.canonicalize()?);
    for dir in rule_layers(&root)? {
        println!(
            "# layer {}{}",
            dir.display(),
            if dir.exists() { "" } else { " (missing)" }
        );
    }
    for r in rules::load(&rule_layers(&root)?)? {
        println!(
            "\n{}  [{}]  fail={} warn={}  {}",
            r.id,
            r.globs.join(", "),
            r.fail,
            r.warn,
            r.source.display()
        );
        println!("  {}", r.question.replace('\n', "\n  "));
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_stats(global: bool, as_json: bool) -> Result<ExitCode> {
    let repo = (!global)
        .then(|| {
            Ok::<_, anyhow::Error>(
                repo_root(&std::env::current_dir()?.canonicalize()?)
                    .display()
                    .to_string(),
            )
        })
        .transpose()?;
    let dir = state_dir()?;
    let s = stats::summarize(&stats::open(&dir)?, repo.as_deref())?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&s)?);
        return Ok(ExitCode::SUCCESS);
    }
    let opt = |v: Option<i64>| v.map_or("-".into(), |v| v.to_string());
    println!(
        "{} ({})",
        dir.join("stats.db").display(),
        repo.as_deref().unwrap_or("all repos")
    );
    println!(
        "runs {}  requests {}  errors {}",
        s.runs, s.requests, s.errors
    );
    println!(
        "latency ms p50 {}  p95 {}  max {}",
        opt(s.latency_ms_p50),
        opt(s.latency_ms_p95),
        opt(s.latency_ms_max)
    );
    println!(
        "largest file {} bytes, {} input tokens; {} tokens per KiB",
        opt(s.bytes_max),
        opt(s.input_tokens_max),
        s.tokens_per_kib.map_or("-".into(), |v| format!("{v:.0}"))
    );
    if !s.rules.is_empty() {
        println!(
            "\n{:<42} {:>6} {:>9} {:>6} {:>7} {:>8}",
            "rule", "judged", "violation", "check", "mean p", "versions"
        );
        for r in &s.rules {
            println!(
                "{:<42} {:>6} {:>9} {:>6} {:>7.2} {:>8}",
                r.rule_id, r.judged, r.violations, r.checks, r.mean_probability, r.versions
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Fails open: without a key, or on any error, the edit goes through untouched.
fn cmd_hook_claude() -> Result<ExitCode> {
    let Some(client) = client()? else {
        return Ok(ExitCode::SUCCESS);
    };
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let event: Value = serde_json::from_str(&input).context("hook input is not JSON")?;
    let Some(file) = event
        .pointer("/tool_input/file_path")
        .and_then(Value::as_str)
    else {
        return Ok(ExitCode::SUCCESS);
    };
    let file = Path::new(file);
    let Ok(canon) = file.canonicalize() else {
        return Ok(ExitCode::SUCCESS);
    };
    let root = repo_root(canon.parent().unwrap_or(&canon));
    let rel = relative(&root, &canon)?;
    let rules = rules::load(&rule_layers(&root)?)?;
    let results = check::run(&client, &root, &[rel], &rules);
    if results.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }
    let str_field = |p: &str| event.pointer(p).and_then(Value::as_str).map(String::from);
    record_stats(
        &stats::RunContext {
            caller: "hook",
            repo: root.display().to_string(),
            harness: Some("claude".into()),
            session_id: str_field("/session_id"),
            agent_model: None,
            tool: str_field("/tool_name"),
        },
        &results,
    );
    if let Some(out) = claude_hook_output(&results[0]) {
        println!("{out}");
    }
    Ok(ExitCode::SUCCESS)
}

fn claude_hook_output(result: &FileResult) -> Option<Value> {
    let FileOutcome::Judged { judgments, .. } = &result.outcome else {
        if let FileOutcome::Failed { error, .. } = &result.outcome {
            eprintln!("kass: {}: {error}", result.path);
        }
        return None;
    };
    let lines = |tier: Tier| {
        judgments
            .iter()
            .filter(|j| j.tier == tier)
            .map(|j| {
                format!(
                    "- {} (p={:.2}): {}",
                    j.rule.id,
                    j.probability,
                    j.rule.question.replace('\n', " ")
                )
            })
            .collect::<Vec<_>>()
    };
    let (violations, checks) = (lines(Tier::Violation), lines(Tier::Check));
    let mut text = String::new();
    if !violations.is_empty() {
        text += &format!(
            "kass found likely rule violations in {}. Fix them:\n{}\n",
            result.path,
            violations.join("\n")
        );
    }
    if !checks.is_empty() {
        text += &format!(
            "kass is unsure about these rules in {}. Double-check each; if the code is fine, carry on:\n{}\n",
            result.path,
            checks.join("\n")
        );
    }
    if !violations.is_empty() {
        Some(json!({"decision": "block", "reason": text.trim_end()}))
    } else if !checks.is_empty() {
        Some(
            json!({"hookSpecificOutput": {"hookEventName": "PostToolUse", "additionalContext": text.trim_end()}}),
        )
    } else {
        None
    }
}
