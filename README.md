<p align="center">
  <img src="docs/logo.svg" alt="kass" width="360">
</p>

<p align="center"><b>Lint rules in plain English, for the code your agents write.</b></p>

<p align="center">
  <img src="docs/demo.gif" alt="kass catching a test that sleeps, then an agent fixing it after the hook tells it" width="800">
</p>

Some code smells are easy to spot and miserable to catch with a regex. A test
that waits by sleeping. A test that only checks its own mocks. A test that
can't fail. With kass you write each one as a yes/no question in markdown, and
[Jev](https://docs.typesafe.ai), TypeSafe's small judgment model, answers it
with a probability in about 300ms.

On my main project, the sleep rule flags 40 test files.

Run it by hand, in CI, or as a Claude Code hook, so the agent hears about a
smell right after it writes one. kass records every judgment in SQLite too, so
you can check later which rules earn their keep.

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

That's it! [`rules/test-smells.md`](rules/test-smells.md) has four test rules
to start from. Keep what fits and rewrite the rest.

## Examples

Both examples live in [`examples/`](examples) with their rules, so you can run
them yourself: `cd examples/go-worker && kass check --all`.

### A Go test that sleeps inside a helper

In [`examples/go-worker`](examples/go-worker/worker_test.go), a test waits for
a worker pool by calling `waitForJobs()`. That helper sits at the bottom of the
file and calls `time.Sleep`. The test body never sleeps, which is why kass
sends each test along with every helper it calls:

```
$ kass check --all
worker_test.go:9 TestPoolRunsEveryJob
  violation  test-waits-on-sleep  p=0.98 (fail >= 0.85)
3 test(s) judged: 1 violation(s), 0 to double-check, 0 error(s)
```

Swap `waitForJobs()` for `<-p.Done()` and it comes back clean. The same file
has `TestPoolDrainsOnClose`, which already waits on `Done()`.

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
edited and sends violations back to it:

```json
{
  "decision": "block",
  "reason": "kass found likely rule violations. Fix them:\n- worker_test.go:9 TestPoolRunsEveryJob: test-waits-on-sleep (p=0.98): Does a test in `content` wait for something to happen by sleeping, ..."
}
```

## Writing rules

Rules are `*.md` files in `.kass/rules/`, committed next to the code they
judge. Each repo keeps its own, and there's no global rules file. kass reads
the nearest `.kass` directory, so a package in a monorepo can have its own
rules too.

```md
# test-waits-on-sleep
globs: **/*_test.go, **/*.test.ts
fail: 0.85
warn: 0.6

Does a test in `content` wait for something to happen by sleeping, or by
polling in a loop with a timer, instead of waiting on a real signal such as a
channel, event, callback, or returned value?
```

A rule starts with `# <id>`, in lowercase letters, digits and dashes, unique
within the directory. The `key: value` lines come right after the heading.
`globs` is required and relative to the directory holding `.kass`. `fail` and
`warn` are optional thresholds, 0.85 and 0.6 by default. After a blank line
comes the question, phrased so that "yes" means the rule is broken.

A few things I learned writing them:

- Ask one narrow thing per rule. "Is this test bad?" gets you noise.
- Say what to answer when the thing isn't there. My first `test-cannot-fail`
  flagged files with no tests in them until I added "If `content` has none,
  answer no".
- Call the state by its field name (`content`), so Jev knows what you mean.

Jev returns the probability of "yes" for each rule, and kass puts it in a tier:

| Tier | When | What happens |
| --- | --- | --- |
| violation | at or above `fail` | `kass check` exits 1; the hook tells the agent to fix it |
| check | at or above `warn` | reported; the hook asks the agent to double-check |
| pass | below `warn` | shown only with `--show-passes` |

## What Jev sees

For Go, TypeScript and JavaScript, kass parses each file with tree-sitter and
sends one request per test. A 170KB test file fits fine this way, and one bad
test gets its own score instead of blending into the good ones around it.

```json
{"path": "worker_test.go", "test": "TestPoolRunsEveryJob", "content": "func TestPoolRunsEveryJob(...) {...}",
 "helpers": [{"name": "waitForJobs", "path": "worker_test.go", "code": "func waitForJobs() {...}"}]}
```

In Go, kass finds `func TestXxx`, testify suite methods, and Ginkgo's
`It`, `Specify` and `DescribeTable`. In JS it finds `it`, `test` and `specify`
from Jest, Vitest, Jasmine, mocha, node:test, ava and Playwright, including
`.only`, `.skip`, `.each`, `fit`, `xit` and `test.describe`. JS tests are named
by their `describe` path, like `Cart > totals an empty cart`.

Helpers are every declaration the test reaches, followed by name through
whatever those declarations call. That covers functions, types, vars and
consts in the file, setup hooks like `beforeEach`, testify's `SetupTest` and
Ginkgo's `BeforeEach`, and in Go, the other `_test.go` files in the package.
kass drops helpers once a request passes 60KB and tells you how many it left
out.

Code that no recognized test contains or calls goes in its own request,
labeled `outside any test (N lines)`. That's where a test library kass doesn't
know ends up, and helpers that only other files use. So it all gets judged
somewhere.

`kass check` judges a test when its lines, or a same-file helper's lines,
changed since HEAD. `--all` judges everything. Any other file goes whole, as
`{"path": ..., "content": ...}`.

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

Add this to the repo's `.claude/settings.json` or to `~/.claude/settings.json`:

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
through untouched, and kass still records the error in stats. So you can
commit the hook even if some of your team has no key.

## Stats

kass records every run in `~/.local/state/kass/stats.db`. Set
`$XDG_STATE_HOME` or `$KASS_STATE_DIR` to move it. There are three tables:

- `runs` has one row per run. Hook runs keep the harness, session and tool.
- `requests` has one row per test or file, with its name, line, helpers sent
  and dropped, bytes, latency, Jev model version, input tokens and any error.
- `judgments` has one row per rule per request, with the probability, tier,
  thresholds and a hash of the rule text. Reword a rule and you can still tell
  which numbers came from which wording.

`kass stats` summarizes them: latency percentiles, the largest request, tokens
per KiB, and counts per rule.

## Cost and speed

Jev charges only for input tokens, $0.042 per million when I wrote this. On my
main project (about 900 test files), `kass check --all` made 4,350 requests in
4m18s and cost $0.41. Latency was 277ms at p50 and 374ms at p95. Day to day,
`kass check` only judges the tests you touched. The go-worker example is 3
requests.

## Environment

| Variable | Default |
| --- | --- |
| `TYPESAFE_API_KEY` | required to call Jev |
| `KASS_MODEL` | `jev-latest` |
| `KASS_JEV_URL` | `https://api.typesafe.ai` |
| `KASS_STATE_DIR` | `$XDG_STATE_HOME/kass` or `~/.local/state/kass` |

## The demo

I rendered the GIF locally from HTML with
[HyperFrames](https://github.com/heygen-com/hyperframes). Its numbers come
from real runs on `examples/go-worker`, and the source is in
[`docs/demo`](docs/demo).

## License

[MIT](LICENSE)
