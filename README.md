# kass

A fuzzy linter. You write rules as plain-language yes/no questions, and kass
asks TypeSafe's [Jev](https://docs.typesafe.ai) whether each file breaks them.
It runs from the command line or as a coding-agent hook, and it records every
judgment so you can tell later whether the rules are worth keeping.

## Setup

Set `TYPESAFE_API_KEY` in your environment. `kass check` fails without it,
naming the variable. Hooks do nothing without it, so kass can be installed
where some people have no key.

```sh
cargo install --path .
mkdir -p .kass/rules && cp <kass>/rules/*.md .kass/rules/   # in the repo to lint
```

## Rules

Rules are `*.md` files in `<repo>/.kass/rules/`, committed with the code they
judge. There are no global rules: each repo says what it cares about. Rule ids
must be unique across the directory.

```md
# test-waits-on-sleep
globs: **/*_test.go, **/*.test.ts
fail: 0.85
warn: 0.6

Does a test in `content` wait for something by sleeping instead of on a real signal?
```

- **Heading:** each rule starts with `# <id>`, using lowercase letters, digits and dashes.
- **Key lines:** `key: value` lines follow the heading directly.
  - `globs:` is required. Globs are relative to the repo root.
  - `fail` and `warn` are optional thresholds (defaults 0.85 and 0.6).
- **Question:** after a blank line, the question. Write it so that "yes" means
  the rule is broken.

All rules matching a file go in one request. Keep each rule narrow and
judgeable from the state alone.

## What Jev sees

For Go, TypeScript and JavaScript, kass parses the file with tree-sitter and
sends one request per test, so a large file never overflows Jev's context and
one bad test doesn't dilute the score of the others:

```json
{"path": "pkg/foo_test.go", "test": "TestFoo", "content": "func TestFoo(...) {...}",
 "helpers": [{"name": "waitReady", "path": "pkg/util_test.go", "code": "func waitReady(...) {...}"}]}
```

- **Tests:** Go `func TestXxx`; JS `it`/`test` calls, named by their `describe`
  path (Playwright's `test.describe` and `test.step` included).
- **Helpers:** every declaration the test reaches, transitively: functions,
  types, vars and consts from the same file, JS `beforeEach`/`afterEach` hooks
  and enclosing `describe` scopes, and for Go, other `_test.go` files in the package.
  Helpers that would push the state past 60KB are left out and counted.
- **Changed tests only:** `kass check` and the hook judge a test only when its
  lines, or a same-file helper's lines, changed since HEAD. `--all` judges every test.

Any other file, or a file with no tests kass can find, is sent whole as
`{"path": ..., "content": ...}`. Either way a question can refer to `content`.

## Tiers

Each rule gets a probability of "yes":

| Tier | Condition | What happens |
| --- | --- | --- |
| violation | at or above `fail` | `kass check` exits 1; the hook blocks and tells the agent to fix it |
| check | at or above `warn` | reported; the hook asks the agent to double-check |
| pass | below `warn` | shown only with `--show-passes` |

## Commands

```sh
kass check [paths...]                    # changed files: staged, unstaged, untracked (git only)
kass check --all [paths...]              # every file under paths (default: cwd), honoring .gitignore
           [--json] [--show-passes]      # exits 0 clean, 1 violations, 2 errors
kass rules                               # rules in effect here and their files
kass stats [--global] [--json]           # recorded judgments for this repo (or all)
kass hook claude                         # Claude Code PostToolUse hook (stdin JSON)
```

## Claude Code hook

Add to `~/.claude/settings.json`:

```json
{
  "hooks": {
    "PostToolUse": [
      { "matcher": "Edit|Write|MultiEdit", "hooks": [{ "type": "command", "command": "kass hook claude" }] }
    ]
  }
}
```

The hook fails open. If the key is missing, no rule matches, or Jev errors, the
edit goes through untouched. Errors are still recorded in stats.

## Stats

Every run is written to `~/.local/state/kass/stats.db` (SQLite, or
`$XDG_STATE_HOME/kass/`, or `KASS_STATE_DIR`). There are three tables:

- **`runs`:** who ran kass. Hook runs record the harness, session id and triggering tool.
- **`requests`:** one per test (or whole file), with the test's name and line,
  helpers sent and omitted, bytes, latency, Jev model version, input tokens, and any error.
- **`judgments`:** one per rule per request, with probability, tier, thresholds, and
  a hash of the rule, so a stat can be traced to the exact rule text.

`kass stats` summarizes them. "Tokens per KiB" is the receipt for any future
file-size limit (Jev's cap is 32k tokens for state plus the longest question).

## Environment

| Variable | Default |
| --- | --- |
| `TYPESAFE_API_KEY` | required to call Jev |
| `KASS_MODEL` | `jev-latest` |
| `KASS_JEV_URL` | `https://api.typesafe.ai` |
| `KASS_STATE_DIR` | `$XDG_STATE_HOME/kass` or `~/.local/state/kass` |
