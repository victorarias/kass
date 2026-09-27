use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

/// A stand-in for the Jev endpoint: answers each question id with a fixed
/// probability, or replays queued raw HTTP statuses first.
struct FakeJev {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    statuses: Arc<Mutex<VecDeque<u16>>>,
}

impl FakeJev {
    fn start(answers: BTreeMap<&'static str, f64>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let statuses = Arc::new(Mutex::new(VecDeque::new()));
        let (reqs, stats) = (requests.clone(), statuses.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut len = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; len];
                reader.read_exact(&mut body).unwrap();
                let body: Value = serde_json::from_slice(&body).unwrap();
                reqs.lock().unwrap().push(body.clone());
                let (status, resp) = match stats.lock().unwrap().pop_front() {
                    Some(s) => (s, json!({"detail": "scripted failure"})),
                    None => {
                        let ans: serde_json::Map<String, Value> = body["questions"]
                            .as_object()
                            .unwrap()
                            .keys()
                            .map(|k| (k.clone(), json!({"type": "noul", "noul": answers.get(k.as_str()).copied().unwrap_or(0.1)})))
                            .collect();
                        (
                            200,
                            json!({"model": "jev-fake", "answers": ans, "usage": {"input_tokens": 100, "output_tokens": 5}}),
                        )
                    }
                };
                let resp = resp.to_string();
                write!(stream, "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp}", resp.len()).unwrap();
            }
        });
        FakeJev {
            url,
            requests,
            statuses,
        }
    }
}

struct Env {
    _dir: tempfile::TempDir,
    repo: PathBuf,
    state: PathBuf,
}

const RULES: &str = "\
# no-sleep
globs: **/*_test.go

Does a test in `content` wait by sleeping?

# mock-only
globs: **/*_test.go

Does a test only assert on mocks?

# docs-only
globs: **/*.md

Is this doc stale?
";

fn setup() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let (repo, state) = (base.join("repo"), base.join("state"));
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::create_dir_all(repo.join("pkg")).unwrap();
    std::fs::create_dir_all(repo.join(".kass/rules")).unwrap();
    std::fs::write(repo.join(".kass/rules/tests.md"), RULES).unwrap();
    std::fs::write(
        repo.join("pkg/foo_test.go"),
        "func TestFoo(t *testing.T) { time.Sleep(time.Second) }",
    )
    .unwrap();
    std::fs::write(repo.join("pkg/foo.go"), "package pkg").unwrap();
    Env {
        _dir: dir,
        repo,
        state,
    }
}

fn kass(env: &Env, jev: &FakeJev, key: bool, args: &[&str], stdin: Option<&str>) -> Output {
    kass_in(&env.repo, env, jev, key, args, stdin)
}

fn kass_in(
    dir: &Path,
    env: &Env,
    jev: &FakeJev,
    key: bool,
    args: &[&str],
    stdin: Option<&str>,
) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kass"));
    cmd.args(args)
        .current_dir(dir)
        .env("KASS_STATE_DIR", &env.state)
        .env("KASS_JEV_URL", &jev.url)
        .env_remove("TYPESAFE_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if key {
        cmd.env("TYPESAFE_API_KEY", "test-key");
    }
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.unwrap_or("").as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn stats(env: &Env, jev: &FakeJev) -> Value {
    let out = kass(env, jev, false, &["stats", "--json"], None);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn rule<'a>(stats: &'a Value, id: &str) -> &'a Value {
    stats["rules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["rule_id"] == id)
        .unwrap()
}

#[test]
fn check_tiers_exit_code_and_records_stats() {
    let env = setup();
    let jev = FakeJev::start(BTreeMap::from([("no-sleep", 0.97), ("mock-only", 0.7)]));
    let out = kass(&env, &jev, true, &["check", "--all"], None);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{stdout}");
    assert!(stdout.contains("pkg/foo_test.go:1 TestFoo\n"), "{stdout}");
    assert!(stdout.contains("violation  no-sleep  p=0.97"), "{stdout}");
    assert!(stdout.contains("check      mock-only  p=0.70"), "{stdout}");
    assert!(
        stdout.contains("1 test(s) judged: 1 violation(s), 1 to double-check, 0 error(s)"),
        "{stdout}"
    );

    // One request for the one matching file, carrying both matching rules.
    let reqs = jev.requests.lock().unwrap().clone();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0]["state"]["path"], "pkg/foo_test.go");
    assert_eq!(
        reqs[0]["questions"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["mock-only", "no-sleep"]
    );

    let s = stats(&env, &jev);
    assert_eq!(
        (
            s["runs"].as_i64(),
            s["requests"].as_i64(),
            s["errors"].as_i64()
        ),
        (Some(1), Some(1), Some(0))
    );
    assert_eq!(rule(&s, "no-sleep")["violations"], 1);
    assert_eq!(rule(&s, "mock-only")["checks"], 1);
}

#[test]
fn check_without_key_names_the_variable() {
    let env = setup();
    let jev = FakeJev::start(BTreeMap::new());
    let out = kass(&env, &jev, false, &["check", "--all"], None);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("TYPESAFE_API_KEY is not set"));
}

#[test]
fn api_errors_are_reported_and_recorded() {
    let env = setup();
    let jev = FakeJev::start(BTreeMap::new());
    jev.statuses.lock().unwrap().push_back(422);
    let out = kass(&env, &jev, true, &["check", "--all"], None);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(2), "{stdout}");
    assert!(stdout.contains("Jev returned HTTP 422"), "{stdout}");
    assert_eq!(stats(&env, &jev)["errors"], 1);
}

#[test]
fn overload_is_retried() {
    let env = setup();
    let jev = FakeJev::start(BTreeMap::new());
    jev.statuses.lock().unwrap().extend([529, 429]);
    let out = kass(&env, &jev, true, &["check", "--all"], None);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(jev.requests.lock().unwrap().len(), 3);
}

fn hook_event(file: &Path) -> String {
    json!({"session_id": "sess-1", "hook_event_name": "PostToolUse", "tool_name": "Edit", "tool_input": {"file_path": file}}).to_string()
}

#[test]
fn hook_blocks_on_violations_and_records_the_session() {
    let env = setup();
    let jev = FakeJev::start(BTreeMap::from([("no-sleep", 0.97), ("mock-only", 0.7)]));
    let out = kass(
        &env,
        &jev,
        true,
        &["hook", "claude"],
        Some(&hook_event(&env.repo.join("pkg/foo_test.go"))),
    );
    assert!(out.status.success());
    let resp: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(resp["decision"], "block");
    let reason = resp["reason"].as_str().unwrap();
    assert!(
        reason.contains("- pkg/foo_test.go:1 TestFoo: no-sleep (p=0.97)"),
        "{reason}"
    );
    assert!(
        reason.contains("Double-check each") && reason.contains("TestFoo: mock-only (p=0.70)"),
        "{reason}"
    );

    let db = rusqlite::Connection::open(env.state.join("stats.db")).unwrap();
    let row: (String, String, String, String) = db
        .query_row(
            "SELECT caller, harness, session_id, tool FROM runs",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        row,
        (
            "hook".into(),
            "claude".into(),
            "sess-1".into(),
            "Edit".into()
        )
    );
}

#[test]
fn hook_adds_context_when_only_unsure() {
    let env = setup();
    let jev = FakeJev::start(BTreeMap::from([("mock-only", 0.7)]));
    let out = kass(
        &env,
        &jev,
        true,
        &["hook", "claude"],
        Some(&hook_event(&env.repo.join("pkg/foo_test.go"))),
    );
    let resp: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(resp.get("decision").is_none());
    assert!(
        resp["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("mock-only")
    );
}

#[test]
fn hook_is_silent_without_key_or_matching_rules_or_on_errors() {
    let env = setup();
    let jev = FakeJev::start(BTreeMap::from([("no-sleep", 0.97)]));
    let test_file = hook_event(&env.repo.join("pkg/foo_test.go"));
    let no_key = kass(&env, &jev, false, &["hook", "claude"], Some(&test_file));
    let no_rules = kass(
        &env,
        &jev,
        true,
        &["hook", "claude"],
        Some(&hook_event(&env.repo.join("pkg/foo.go"))),
    );
    assert!(jev.requests.lock().unwrap().is_empty());
    jev.statuses.lock().unwrap().push_back(500);
    let failing = kass(&env, &jev, true, &["hook", "claude"], Some(&test_file));
    for out in [no_key, no_rules, failing] {
        assert!(out.status.success());
        assert!(
            out.stdout.is_empty(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
    assert_eq!(stats(&env, &jev)["errors"], 1);
}

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn changed_judges_staged_unstaged_and_untracked_files_only() {
    let env = setup();
    std::fs::remove_dir_all(env.repo.join(".git")).unwrap();
    git(&env.repo, &["init", "-q"]);
    for f in ["a", "b", "c", "clean", "old", "gone"] {
        std::fs::write(env.repo.join(format!("pkg/{f}_test.go")), f).unwrap();
    }
    git(&env.repo, &["add", "."]);
    git(
        &env.repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "init",
        ],
    );
    std::fs::write(env.repo.join("pkg/a_test.go"), "unstaged").unwrap();
    std::fs::write(env.repo.join("pkg/b_test.go"), "staged").unwrap();
    git(&env.repo, &["add", "pkg/b_test.go"]);
    std::fs::create_dir_all(env.repo.join("other")).unwrap();
    std::fs::write(env.repo.join("other/new_test.go"), "untracked").unwrap();
    git(&env.repo, &["mv", "pkg/old_test.go", "pkg/renamed_test.go"]);
    git(&env.repo, &["rm", "-q", "pkg/gone_test.go"]);

    let jev = FakeJev::start(BTreeMap::new());
    let judged = |args: &[&str]| {
        jev.requests.lock().unwrap().clear();
        let out = kass(&env, &jev, true, args, None);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let mut paths: Vec<String> = jev
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| r["state"]["path"].as_str().unwrap().to_string())
            .collect();
        paths.sort();
        paths
    };
    assert_eq!(
        judged(&["check"]),
        [
            "other/new_test.go",
            "pkg/a_test.go",
            "pkg/b_test.go",
            "pkg/renamed_test.go"
        ]
    );
    assert_eq!(
        judged(&["check", "pkg"]),
        ["pkg/a_test.go", "pkg/b_test.go", "pkg/renamed_test.go"]
    );
}

#[test]
fn default_check_outside_git_points_to_all() {
    let env = setup();
    std::fs::remove_dir_all(env.repo.join(".git")).unwrap();
    let jev = FakeJev::start(BTreeMap::new());
    let out = kass(&env, &jev, true, &["check"], None);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("is not a git repository") && stderr.contains("kass check --all"),
        "{stderr}"
    );
}

const HELPERS_TEST: &str = "package pkg

func waitReady(t *testing.T) { time.Sleep(time.Second) }

func TestA(t *testing.T) {
	waitReady(t)
}

func TestB(t *testing.T) {
	if got := 1; got != 1 {
		t.Fatal(got)
	}
}
";

fn judged_tests(jev: &FakeJev) -> Vec<(String, String, Vec<String>)> {
    let mut out: Vec<_> = jev
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|r| {
            let s = &r["state"];
            let helpers = s["helpers"]
                .as_array()
                .map(|h| {
                    h.iter()
                        .map(|h| h["name"].as_str().unwrap().to_string())
                        .collect()
                })
                .unwrap_or_default();
            (
                s["path"].as_str().unwrap().to_string(),
                s["test"].as_str().unwrap_or("<file>").to_string(),
                helpers,
            )
        })
        .collect();
    out.sort();
    out
}

#[test]
fn each_test_is_judged_alone_with_the_helpers_it_calls() {
    let env = setup();
    std::fs::write(env.repo.join("pkg/foo_test.go"), HELPERS_TEST).unwrap();
    std::fs::write(
        env.repo.join("pkg/helpers_test.go"),
        "package pkg\n\nfunc unusedHelper() {}\n",
    )
    .unwrap();
    std::fs::write(
        env.repo.join("pkg/mixed_test.go"),
        "package pkg\n\nvar _ = Scenario(\"unknown library\", func() {})\n\nfunc TestC(t *testing.T) {}\n",
    )
    .unwrap();
    std::fs::write(
        env.repo.join("pkg/main_test.go"),
        "package pkg\n\nfunc TestMain(m *testing.M) { m.Run() }\n",
    )
    .unwrap();
    let jev = FakeJev::start(BTreeMap::new());
    let out = kass(&env, &jev, true, &["check", "--all", "--json"], None);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        judged_tests(&jev),
        [
            (
                "pkg/foo_test.go".into(),
                "TestA".into(),
                vec!["waitReady".to_string()]
            ),
            ("pkg/foo_test.go".into(), "TestB".into(), vec![]),
            ("pkg/helpers_test.go".into(), "<file>".into(), vec![]),
            ("pkg/main_test.go".into(), "<file>".into(), vec![]),
            ("pkg/mixed_test.go".into(), "<file>".into(), vec![]),
            ("pkg/mixed_test.go".into(), "TestC".into(), vec![]),
        ]
    );
    let json: Value = serde_json::from_slice(&out.stdout).unwrap();
    let a = json
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["test"] == "TestA")
        .unwrap();
    assert_eq!(
        (a["line"].as_i64(), a["helpers"].as_i64()),
        (Some(5), Some(1))
    );
    let rest = json
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["path"] == "pkg/mixed_test.go" && r["test"] != "TestC")
        .unwrap();
    assert_eq!(
        (rest["test"].as_str(), rest["line"].as_i64()),
        (Some("outside any test (1 lines)"), Some(3))
    );
}

#[test]
fn changed_check_judges_only_tests_whose_lines_or_helpers_changed() {
    let env = setup();
    std::fs::remove_dir_all(env.repo.join(".git")).unwrap();
    std::fs::write(env.repo.join("pkg/foo_test.go"), HELPERS_TEST).unwrap();
    git(&env.repo, &["init", "-q"]);
    git(&env.repo, &["add", "."]);
    git(
        &env.repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "init",
        ],
    );
    let jev = FakeJev::start(BTreeMap::new());
    let run = |content: String| {
        std::fs::write(env.repo.join("pkg/foo_test.go"), content).unwrap();
        jev.requests.lock().unwrap().clear();
        let out = kass(&env, &jev, true, &["check"], None);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        judged_tests(&jev)
            .into_iter()
            .map(|(_, t, _)| t)
            .collect::<Vec<_>>()
    };
    assert_eq!(run(HELPERS_TEST.replace("got != 1", "got != 2")), ["TestB"]);
    assert_eq!(
        run(HELPERS_TEST.replace("time.Second", "time.Minute")),
        ["TestA"]
    );
    assert_eq!(run(HELPERS_TEST.to_string()), Vec::<String>::new());
}

#[test]
fn a_nested_kass_directory_is_its_own_root_inside_the_repo() {
    let env = setup();
    std::fs::remove_dir_all(env.repo.join(".git")).unwrap();
    let svc = env.repo.join("svc");
    std::fs::create_dir_all(svc.join("pkg")).unwrap();
    std::fs::create_dir_all(svc.join(".kass/rules")).unwrap();
    std::fs::write(svc.join(".kass/rules/tests.md"), RULES).unwrap();
    std::fs::write(env.repo.join("pkg/foo_test.go"), HELPERS_TEST).unwrap();
    std::fs::write(svc.join("pkg/foo_test.go"), HELPERS_TEST).unwrap();
    git(&env.repo, &["init", "-q"]);
    git(&env.repo, &["add", "."]);
    git(
        &env.repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "init",
        ],
    );
    let changed = HELPERS_TEST.replace("got != 1", "got != 2");
    std::fs::write(env.repo.join("pkg/foo_test.go"), &changed).unwrap();
    std::fs::write(svc.join("pkg/foo_test.go"), &changed).unwrap();

    let jev = FakeJev::start(BTreeMap::new());
    let out = kass_in(&svc.join("pkg"), &env, &jev, true, &["check"], None);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        judged_tests(&jev),
        [("pkg/foo_test.go".to_string(), "TestB".to_string(), vec![])]
    );
}
