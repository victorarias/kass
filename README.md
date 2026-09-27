<p align="center">
  <img src="docs/logo.svg" alt="kass" width="360">
</p>

<p align="center"><b>Lint rules in plain English, for the code your agents write.</b></p>

<p align="center">
  <img src="docs/demo.gif" alt="kass catching a test that sleeps, then an agent fixing it after the hook tells it" width="800">
</p>

Some code smells are obvious to a person and painful to express as a regex: a
test that waits by sleeping, a test that only checks its own mocks, a test that
can't fail. kass lets you write those rules as yes/no questions in markdown,
and asks [Jev](https://docs.typesafe.ai) (TypeSafe's small, calibrated
judgment model) to answer each one with a probability, in about 300ms.

It's built for the loop where agents write most of the code: run it by hand,
in CI, or as a Claude Code hook so the agent hears about a smell the moment it
writes one. And every judgment lands in SQLite, so you can tell later which
rules earn their keep.

## Getting started

You need a Rust toolchain and a TypeSafe API key.

```sh
cargo install --git https://github.com/victorarias/kass
export TYPESAFE_API_KEY=...          # from typesafe.ai

cd your-repo
mkdir -p .kass/rules
curl -sfL -o .kass/rules/test-smells.md \
  https://raw.githubusercontent.com/victorarias/kass/main/rules/test-smells.md

kass check          # what changed since HEAD (staged, unstaged, untracked)
kass check --all    # everything
```

That's it! [`rules/test-smells.md`](rules/test-smells.md) is a starter pack of
four test rules. Keep the ones that fit, rewrite the rest, add your own.

## Examples

Both examples are in [`examples/`](examples) with their rules, so you can run
them yourself (`cd examples/go-worker && kass check --all`).

### A Go test that sleeps, hidden in a helper

[`examples/go-worker`](examples/go-worker/worker_test.go) has a test that
waits for a worker pool with `waitForJobs()`, a helper at the bottom of the
file that calls `time.Sleep`. The test itself never sleeps, so kass sends the
test together with every helper it reaches:

```
$ kass check --all
worker_test.go:9 TestPoolRunsEveryJob
  violation  test-waits-on-sleep  p=0.98 (fail >= 0.85)
3 test(s) judged: 1 violation(s), 0 to double-check, 0 error(s)
```

Swap `waitForJobs()` for `<-p.Done()` and it comes back clean. The same test
file has `TestPoolDrainsOnClose`, which already waits the right way.

### A TypeScript test that can't fail

[`examples/ts-cart`](examples/ts-cart/cart.test.ts) has a Vitest test that
calls the code and checks nothing:

```
$ kass check --all
cart.test.ts:24 Cart > totals an empty cart
  violation  test-cannot-fail  p=0.96 (fail >= 0.85)
3 test(s) judged: 1 violation(s), 0 to double-check, 0 error(s)
```

### Your agent, as it writes

As a Claude Code `PostToolUse` hook, kass judges the file the agent just
edited and hands violations straight back to it:

```json
{
  "decision": "block",
  "reason": "kass found likely rule violations. Fix them:\n- worker_test.go:9 TestPoolRunsEveryJob: test-waits-on-sleep (p=0.98): Does a test in `content` wait for something to happen by sleeping, ..."
}
```

## Writing rules

Rules are `*.md` files in `.kass/rules/`, committed next to the code they
judge. There are no global rules: each repo says what it cares about. The
nearest `.kass` directory wins, so a package in a monorepo can keep its own.

```md
# test-waits-on-sleep
globs: **/*_test.go, **/*.test.ts
fail: 0.85
warn: 0.6

Does a test in `content` wait for something to happen by sleeping, or by
polling in a loop with a timer, instead of waiting on a real signal such as a
channel, event, callback, or returned value?
```

- **Heading:** `# <id>`, lowercase letters, digits and dashes. Ids are unique per directory.
- **Key lines** follow the heading directly:
  - `globs:` (required) are relative to the directory holding `.kass`.
  - `fail` and `warn` are optional thresholds (defaults 0.85 and 0.6).
- **Question:** after a blank line. Write it so "yes" means the rule is broken.

A few things we learned writing them:

- One narrow judgment per rule. "Is this test bad?" gets you noise.
- Say what to do when the thing isn't there. Our first `test-cannot-fail`
  flagged files with no tests at all, until the question said "If `content`
  has none, answer no".
- Refer to the state by name (`content`), so Jev knows what you mean.

Each rule gets a probability of "yes", and a tier:

| Tier | When | What happens |
| --- | --- | --- |
| violation | at or above `fail` | `kass check` exits 1; the hook tells the agent to fix it |
| check | at or above `warn` | reported; the hook asks the agent to double-check |
| pass | below `warn` | shown only with `--show-passes` |

## What Jev sees

For Go, TypeScript and JavaScript, kass parses each file with tree-sitter and
sends one request per test. A big test file never overflows Jev's context, and
one bad test doesn't drown in the good ones around it.

```json
{"path": "worker_test.go", "test": "TestPoolRunsEveryJob", "content": "func TestPoolRunsEveryJob(...) {...}",
 "helpers": [{"name": "waitForJobs", "path": "worker_test.go", "code": "func waitForJobs() {...}"}]}
```

- **Tests** kass recognizes:
  - Go: `func TestXxx`, testify suite methods, and Ginkgo `It`/`Specify`/`DescribeTable`.
  - JS: `it`/`test`/`specify` from Jest, Vitest, Jasmine, mocha, node:test,
    ava and Playwright, including `.only`/`.skip`/`.each`, `fit`/`xit` and
    `test.describe`. They're named by their `describe` path, like `Cart > totals an empty cart`.
- **Helpers:** every declaration the test reaches, followed transitively by
  name. That's functions, types, vars and consts in the file, setup hooks
  (`beforeEach`, `SetupTest`, Ginkgo's `BeforeEach`) and the blocks around the
  test, plus other `_test.go` files in a Go package. Helpers past a 60KB
  budget are left out, and the output says how many.
- **Outside any test:** code no recognized test contains or reaches (a test
  library kass doesn't know, a helper only other files use) goes in its own
  request, labeled `outside any test (N lines)`. Nothing goes unjudged.
- **Only what changed:** `kass check` judges a test when its lines, or a
  helper's lines in the same file, changed since HEAD. `--all` judges everything.

Any other file is sent whole, as `{"path": ..., "content": ...}`.

## Commands

```sh
kass check [paths...]           # changed files (staged, unstaged, untracked)
kass check --all [paths...]     # every file under paths, honoring .gitignore
           [--json] [--show-passes]
                                # exits 0 clean, 1 violations, 2 errors
kass rules                      # the rules in effect here
kass stats [--global] [--json]  # what kass has judged in this repo (or everywhere)
kass hook claude                # Claude Code PostToolUse hook (reads the event on stdin)
```

## Claude Code hook

Add this to `.claude/settings.json` (the repo's) or `~/.claude/settings.json`:

```json
{
  "hooks": {
    "PostToolUse": [
      { "matcher": "Edit|Write|MultiEdit", "hooks": [{ "type": "command", "command": "kass hook claude" }] }
    ]
  }
}
```

The hook fails open. With no key, no matching rule, or Jev down, the edit goes
through untouched, and errors still land in stats. So it's safe to commit the
hook for a team where not everyone has a key.

## Stats

Every run is recorded in `~/.local/state/kass/stats.db` (or
`$XDG_STATE_HOME/kass/`, or `$KASS_STATE_DIR`):

- **`runs`:** who ran kass; hook runs keep the harness, session and tool.
- **`requests`:** one per test or file: name, line, helpers sent and omitted,
  bytes, latency, Jev model version, input tokens, errors.
- **`judgments`:** one per rule per request: probability, tier, thresholds,
  and a hash of the rule text, so a number traces back to the exact wording.

`kass stats` summarizes them: latency percentiles, the largest request, tokens
per KiB, and per-rule counts.

## Cost and speed

Jev charges for input tokens only ($0.042 per million at the time of writing).
Measured on a codebase with about 900 test files, `kass check --all` made
4,350 requests in 4m18s and cost $0.41, with a p50 latency of 277ms and a p95
of 374ms. Day to day, `kass check` only judges the tests you touched, so a run
is a handful of requests and well under a cent.

## Environment

| Variable | Default |
| --- | --- |
| `TYPESAFE_API_KEY` | required to call Jev |
| `KASS_MODEL` | `jev-latest` |
| `KASS_JEV_URL` | `https://api.typesafe.ai` |
| `KASS_STATE_DIR` | `$XDG_STATE_HOME/kass` or `~/.local/state/kass` |

## The demo

The GIF above is rendered locally from HTML with
[HyperFrames](https://github.com/heygen-com/hyperframes). Its numbers come
from real runs on `examples/go-worker`. The source is in [`docs/demo`](docs/demo).

## License

[MIT](LICENSE)
