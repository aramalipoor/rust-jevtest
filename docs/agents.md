# jevtest: full agent guide

Exact reference for coding agents. Short version: [README](../README.md). Run every command from the
repository root.

## Install

```sh
cargo install jevtest --locked          # provides `cargo jevtest` and `jevtest`
cargo install cargo-nextest --locked    # optional: used automatically when installed
```

- `cargo jevtest …` and `jevtest …` are the same program. Run them from the repo root.
- Runner: default `runner = "auto"` uses cargo-nextest when installed, else `cargo test` with one stderr notice.
  Force one with `--runner nextest|cargo` or `select.runner`.

## Verify

```sh
cargo jevtest doctor
```

`doctor` checks, in order:

| Check | If it fails, do |
|---|---|
| git | Run from inside the repo. `doctor` also shows the change scope `auto` would pick. If no default branch resolves (`origin/HEAD`, `origin/main`, `origin/master`, `main`, `master`), set `changes.default_branch` or pass `--branch BASE` / `--base REV`. |
| cargo metadata: workspace loads | Fix the workspace (`cargo metadata` must succeed). |
| runner present | With the default `runner = "auto"`, a missing cargo-nextest only means `cargo test` is used (one notice). For nextest: `cargo install cargo-nextest --locked`. |
| key found (names the source, never the key) | Optional. See [Key setup](#key-setup). Without it, Jev is skipped and `select.without_jev` decides the selection. |
| one tiny Jev call | Network or key problem. Selection still works with `--offline` (cached answers) or `--no-jev`. |

## Integrate

```sh
cargo jevtest init            # writes jevtest.toml (commented defaults), prints the AGENTS.md block
cargo jevtest init --force    # overwrite an existing jevtest.toml
```

Paste the printed block into `AGENTS.md` (and `CLAUDE.md` if the repo uses one). It is:

```markdown
<!-- jevtest -->
## Tests: use jevtest

- Before you finish a change, run `cargo jevtest run` from the repo root. It runs only the tests your diff can break (cargo-nextest, else cargo test) and exits with the runner's code.
- Red: fix the code (or the spec, and say which) and rerun `cargo jevtest run` until it is green. Never loosen or skip a failing test.
- Why was a test picked or skipped? `cargo jevtest explain <PATTERN>`.
- Machine-readable selection: `cargo jevtest --format json`.
- Changes to `Cargo.toml`, `Cargo.lock`, the toolchain or `.config/nextest.toml` escalate to the full suite on their own; let it run.
- The full suite still runs for releases and in CI.
<!-- /jevtest -->
```

`jevtest.toml` is optional. Lookup order: `--config PATH`, then `jevtest.toml` at the repo root,
then `.config/jevtest.toml`, then built-in defaults. Edit it only to add `[[rule]]`s, `[tests]` lists
or a CI profile. See [Configuration reference](#configuration-reference).

## Daily loop

```sh
cargo jevtest run                         # select, then run the tests; exit code = runner's
cargo jevtest run -- --no-fail-fast       # args after -- go to the runner
cargo jevtest                             # select only: command on stdout, summary on stderr
cargo jevtest --format json               # full report on stdout
cargo jevtest explain store::             # each layer's verdict for candidates matching the pattern
```

Decision rules:

- Before finishing a change: `cargo jevtest run`. If it is green, you are done with tests.
- If it fails: fix and rerun `cargo jevtest run`. Cached Jev answers make the rerun cost nothing.
- If you expect a test to run and it did not: `cargo jevtest explain <name-or-path-fragment>`. If the miss is real, add a `[[rule]]` or a `[tests] always` filterset instead of rerunning everything.
- If the summary says it escalated to a full run, let it run. The reason is in the summary and in the report's escalations.
- If it selected nothing ("nothing worth running", exit 0): the diff touched only ignored paths. Done.
- If the stderr `jevtest: changes = …` line shows the wrong scope: pass a scope flag (next section).
- Need more recall for a risky change: `--top-n 60 --top-fraction 0.5`, `--threshold 0.3`, or `--no-jev` (with the default `--without-jev reach`: every reached test).
- No key and the reach-level run is too big: `--without-jev evidence` (must-runs, static evidence, rules, `tests.always` only).
- Need it faster: `--max-tests N` (drops the lowest-scored picks that are not must-runs).

## Choosing what changed

By default (`auto`) jevtest tests your uncommitted work. If the tree is clean, it tests the branch's commits, or the last commit when you are on the default branch.
Pass one scope flag to choose exactly. Scope flags are mutually exclusive; `--files` narrows any of them.

| Flag | Changes considered |
|---|---|
| (none) / `--changes auto` | auto, below |
| `--uncommitted` | staged + unstaged + untracked vs `HEAD` |
| `--staged` | index vs `HEAD` (pre-commit) |
| `--unstaged` | working tree (+ untracked) vs index |
| `--branch [BASE]` | merge-base(BASE or the default branch, HEAD) .. working tree: committed branch work + uncommitted |
| `--last N` | the last N commits: `HEAD~N..HEAD` |
| `--commit REV` | exactly this commit: `REV^..REV` |
| `--since WHEN` | commits since WHEN (`today`, `midnight`, `6 hours ago`, `2026-10-01`; git date syntax) on first-parent history: base = parent of the oldest such commit, head = `HEAD` |
| `--range A..B` / `--base A [--head B]` | explicit range; `A..` or no `--head` = against the working tree |
| `--files PATH_OR_GLOB...` (repeatable) | restrict the scope's diff to these paths. A listed file with no diff in scope counts as wholly changed, so `--files src/x.rs` alone means "the impact of this file" |

`--changes auto|uncommitted|staged|unstaged|branch|last|since|range` is the long form of the same choice (matches `changes.mode`).

```sh
cargo jevtest run --uncommitted              # only what you are editing now
cargo jevtest run --branch                   # everything on this branch vs the default branch
cargo jevtest run --branch origin/release    # ... vs another base
cargo jevtest run --last 3                   # the last 3 commits
cargo jevtest run --since today              # today's commits
cargo jevtest run --commit abc123            # exactly one commit
cargo jevtest run --files crates/store/src/blob.rs   # the impact of one file
```

`auto` policy:

1. Dirty tree (any staged, unstaged or untracked change not in `paths.ignore`) → uncommitted work. Never narrowed; when it is large, the stderr line says so.
2. Else clean and `HEAD` off the default branch → branch vs the default branch (committed only). Detached `HEAD` counts as off the default branch.
3. Else (clean, on the default branch) → the last commit.
4. Size guard for 2 and 3: if the committed diff exceeds `changes.max_files` (60) files or `changes.max_lines` (3000) changed lines, narrow to commits since `changes.recent` (`"midnight"`); if that is empty or still too large, the last commit.

Default branch: `changes.default_branch = "auto"` → `origin/HEAD`, else `origin/main`, `origin/master`, local `main`, `master`.

jevtest always prints one stderr line saying what it chose and why, e.g.:

```text
jevtest: changes = feat vs origin/main (auto: clean tree, not on origin/main) · 2 files, 6 lines
```

The report JSON carries the same as `changes: {requested, used, what, reason, base, head, files, lines, narrowed_from}`. `cargo jevtest doctor` shows the scope `auto` would pick.

Decision rules:

- Editing, not yet committed: no flag (auto picks `uncommitted`).
- Before opening or updating a PR: `--branch`.
- After committing on the default branch: no flag (auto picks `last 1`), or `--last N` for more.
- The size guard narrowed a branch you want tested whole: `--branch`.
- You know the one file that matters: `--files PATH`.

## Key setup

Jev needs a TypeSafe API key. The key is looked up in this order:

1. `TYPESAFE_API_KEY`
2. `AI_GATEWAY_API_KEY` (Vercel AI Gateway; jevtest then uses `https://ai-gateway.vercel.sh/typesafe/v1` and model `typesafe-ai/jev` unless you set `jev.base_url` or `jev.model`)
3. the key file `~/.config/jevtest/typesafe.key`

```sh
mkdir -p ~/.config/jevtest
printf '%s' "$TYPESAFE_API_KEY" > ~/.config/jevtest/typesafe.key
chmod 600 ~/.config/jevtest/typesafe.key
```

- Never print, log or commit the key. `doctor` names where it found the key, never the value.
- No key, or `--no-jev`: jevtest skips layers 6–7 (with one stderr line saying how to enable Jev when
  the key is missing). It is never an error. `select.without_jev` / `--without-jev` decides what runs:
  - `reach` (default): every test in reached crates. Correct, just larger (a one-crate change in a 1,717-test
    workspace: 91 tests instead of 45).
  - `evidence`: must-runs (`Changed`, and `Direct`/`Helper` under `static_evidence = "must"`) + tests with
    any static evidence + `[[rule]]` matches + `tests.always`. Smaller, lower recall.
- `--offline`: cached Jev answers are used; tests whose questions are not cached are unjudged and selected.

## How selection works

Each layer records its verdict per candidate; `explain` and the JSON report show them.

| # | Layer | What it does | Cost | Config keys |
|---|---|---|---|---|
| 1 | Intake | Diff for the chosen change scope (see [Choosing what changed](#choosing-what-changed)); changed lines on both sides; changed items from old and new source (fn, `Type::method`, struct, enum, trait, const, static, type alias, `macro_rules`, deleted items too) | git + parse | `[changes]`; scope flags |
| 2 | Path policy | `full_run` paths → full run; `ignore` paths dropped; `[[rule]]` matches → must filtersets, whole packages or full run; non-Rust file in a package → `non_rust` policy; unparsable `.rs` → whole package | free | `paths.full_run`, `paths.ignore`, `[[rule]]`, `select.non_rust` |
| 3 | Reach | Changed packages + reverse deps (normal, dev, build) | `cargo metadata` | `select.reach_depth` |
| 4 | Discovery | Tests in reached packages, via `syn`, with spans and source | parse | none |
| 5 | Static evidence | `Changed` (test's own span changed) → must. `Direct` (test names a changed item) and `Helper` (a same-module non-test fn it calls does) → must or boost. `Transitive` (reaches a changed item through the name-based call graph) → boost. Boost = skips screening, +0.2 score (cap 1.0) | parse, no model | `select.static_evidence`, `select.call_graph_depth` |
| 5b | Coverage | With a [coverage map](#coverage-map): `gate` drops tests that execute no changed function and have no other evidence, unless a change is coverage-blind; `must` runs covered tests; `boost` makes them skip screening with +0.3 | free (map built nightly) | `[coverage]` |
| 6 | Screening (Jev stage 1) | One Noul per (package, file, module) group of non-must candidates; groups below `group_threshold` dropped unless a member has evidence | Jev, small | `select.group_threshold`, `jev.max_group_chars` |
| 7 | Judging (Jev stage 2) | One Noul per surviving test per view: `names` (package, module, name; ~50 tokens) and `body` (test source). Score = max over views + boost; ranks per view | Jev, most tokens | `jev.views`, `jev.max_test_chars`, `jev.batch`, `jev.concurrency`, `jev.max_questions`, `jev.max_state_chars` |
| 8 | Policy | selected = must ∪ rules ∪ `tests.always` ∪ top picks per view (`min(top_n, ⌈top_fraction × tests that view ranked⌉)`) ∪ {score ≥ `threshold`} ∪ unjudged; minus `tests.never` unless the test changed; then `max_tests` / `min_tests` by score. Without Jev (no key, `--no-jev`): `without_jev = "reach"` selects every reached test; `"evidence"` selects must ∪ static evidence ∪ rules ∪ `tests.always` | free | `select.top_n`, `select.top_fraction`, `select.threshold`, `select.max_tests`, `select.min_tests`, `select.without_jev`, `tests.always`, `tests.never` |
| 9 | Output | nextest: `cargo nextest run -p P... -E '<expr>'`. cargo: one `cargo test -p P -- name...` per package. `auto`: nextest if installed, else cargo (one stderr notice) | free | `select.runner`; `--format`, `--json` |

If Jev fails (401/402/403/5xx, timeout, connection error, malformed reply), `select.on_jev_error` decides: `reach` (default: select every reached test, exit 0, one stderr line `jevtest: Jev failed: <cause>; selecting every reached test (on_jev_error = reach)`), `full`, or `fail`. Jev never blocks longer than `jev.timeout_secs` in total.

## Coverage map

A per-test coverage map is the strongest static signal: a test that never executes a changed
function cannot change outcome because of that function's body. It costs nothing per run; build it
once (nightly on the default branch) and every selection reads it.

```sh
cargo jevtest coverage build                # all tests; writes .jevtest/coverage.json.gz
cargo jevtest coverage build --out /tmp/cov.json.gz -- -p core --run-ignored all   # extra nextest args after --
cargo jevtest coverage info                 # path, commit, age (commits, days), tests, functions, size
```

`coverage build` needs cargo-nextest and LLVM tools whose major version matches `rustc -vV`'s `LLVM
version` (`rustup component add llvm-tools`, or `coverage.llvm_bin`). It builds with `-C
instrument-coverage` in its own target dir (`target/jevtest-cov`), runs every test in its own process
(`--no-fail-fast`; failing tests are mapped too) with jevtest as the target runner, which gives each
test its own `LLVM_PROFILE_FILE`. It then merges each test's profiles (`llvm-profdata merge
-sparse`), keeps the functions with a non-zero entry count that live in workspace source files, and
names them as jevtest names changed items (`Type::method`, `name`; closures and generic instances fold
into the enclosing item) via one `llvm-cov export` per binary. Tests spawning the workspace's own
binaries are covered too. The map is gzip JSON: `version`, `commit`, `built_at`, `rustc`,
`functions [{file, name, start, end}]`, `tests [{package, binary_id, name, functions}]`.

Policies (`coverage.policy`), applied after static evidence and before Jev:

| Policy | Tests that execute a changed function | Tests the map knows that execute none |
|---|---|---|
| `gate` (default) | go on to screening and judging (evidence `covered(n)`) | dropped (`not-covered`) unless they have static evidence or must run |
| `must` | always run (`covered`) | unchanged |
| `boost` | skip screening, +0.3 score | unchanged |
| `off` | layer skipped | |

The gate steps aside, per package, when a change is coverage-blind: a changed item that is not a
fn or method (struct, enum, const, static, type alias, trait, `macro_rules`, associated const or
type, an impl header whose methods are not all in the map), a `const fn`, a changed fn the map does
not know (new, or no test ran it), a top-level line other than a private `use` (`pub use`, `mod`,
attributes, item macros), a private `use` in a file the map has never seen, anything in a proc-macro
crate, and every whole-package escalation (manifest, build script, non-Rust file, parse failure).
Packages with such changes, and every package that depends on them, are not gated; `blind_items`
in the report says why. A changed private `use` counts as changing every mapped function of its
module's files. Tests the map does not know (new, renamed, or not run when it was built) are never
gated. Changed items match map functions by file and name, not line, so line drift is harmless;
renamed files match through the diff's old path.

Staleness: a map more than `max_age_commits` commits behind the change base, or built on a commit
not in `HEAD`'s history, downgrades `gate`/`must` to `boost` (one summary line). No map: the layer is
skipped with `coverage: no map (cargo jevtest coverage build)`.

Nightly CI recipe (GitHub Actions shape; any CI works):

```yaml
- run: rustup component add llvm-tools && cargo install cargo-nextest jevtest --locked
- run: cargo jevtest coverage build          # on the default branch, nightly
- uses: actions/upload-artifact@v4
  with: { name: jevtest-coverage, path: .jevtest/coverage.json.gz }
```

PR lanes download the artifact to `.jevtest/coverage.json.gz` (or point `coverage.map` at it) before
`cargo jevtest run`. Locally, `cargo jevtest coverage build` on the default branch works the same.

Limits: coverage is from one run, so code reached only under other inputs, timing, features or
environment is not in it; ignored tests are mapped only with `-- --run-ignored all`; tests that abort
before exiting write no profile and stay unknown (never gated); doc tests are not mapped.
Add `.jevtest/` to `.gitignore` unless you want to commit the map.

## Configuration reference

Precedence: defaults < file top level < `[profile.NAME]` < CLI flags. Unknown keys are an error that
names the key.

### `[select]`

| Key | Default | Meaning | CLI |
|---|---|---|---|
| `top_n` | `30` | Top N tests per Jev view are selected | `--top-n N` |
| `top_fraction` | `0.25` | ...but at most this share (0..1) of the tests a view ranked, so small pools stay narrow | `--top-fraction F` |
| `threshold` | `0.5` | Score at or above is always selected | `--threshold F` |
| `group_threshold` | `0.1` | Stage-1 screening cutoff; groups with static evidence are never screened out | `--group-threshold F` |
| `max_tests` | `0` | Cap; `0` = none. Drops lowest-scored non-must picks | `--max-tests N` |
| `min_tests` | `0` | Pad with the next-best scores | |
| `static_evidence` | `"must"` | `must` \| `boost` \| `off`: how `Direct`/`Helper` evidence counts | |
| `call_graph_depth` | `2` | Name-based caller hops inside affected crates | |
| `reach_depth` | `0` | `0` = all transitive reverse deps; N = N hops | |
| `non_rust` | `"whole-package"` | `whole-package` \| `jev`: non-`.rs` files inside a package | |
| `on_jev_error` | `"reach"` | `reach` (every reached test) \| `full` \| `fail` | |
| `without_jev` | `"reach"` | Selection when Jev is not used (no key, `--no-jev`): `reach` (every test in reached crates) \| `evidence` (must-runs + static evidence + rules + `tests.always`) | `--without-jev reach\|evidence` |
| `runner` | `"auto"` | `auto` (nextest if installed, else `cargo test` with one stderr notice) \| `nextest` \| `cargo` | `--runner auto\|nextest\|cargo` |

### `[paths]`

| Key | Default | Meaning |
|---|---|---|
| `ignore` | `["**/*.md", "docs/**", ".github/**"]` | Changes here are ignored |
| `full_run` | `["Cargo.toml", "Cargo.lock", "rust-toolchain", "rust-toolchain.toml", ".cargo/**", ".config/nextest.toml"]` | Changes here escalate to the full suite |

### `[[rule]]` (repeatable)

Path-triggered must-runs. `when` is a list of globs; set any of `run`, `packages`, `full`.

| Key | Default | Meaning |
|---|---|---|
| `when` | none | Globs of changed paths that fire the rule |
| `run` | `[]` | nextest filtersets that must run |
| `packages` | `[]` | Packages run whole |
| `full` | `false` | Escalate to the full suite |

Example: migrations change behaviour no Rust symbol shows.

```toml
[[rule]]
when = ["crates/core/migrations/**"]
run = ["package(=core) & test(/store::/)"]

[[rule]]
when = ["crates/api/fixtures/**", "crates/api/src/generated/**"]
packages = ["api"]          # macro-generated or data-driven tests: run the package whole

[[rule]]
when = ["build.rs", "proto/**"]
full = true
```

### `[tests]`

| Key | Default | Meaning |
|---|---|---|
| `always` | `[]` | nextest filtersets that always run (smoke tests) |
| `never` | `[]` | Quarantine; dropped unless the test itself changed |

### `[jev]`

| Key | Default | Meaning |
|---|---|---|
| `model` | `"jev-latest"` | Model id; env `TYPESAFE_MODEL` overrides |
| `base_url` | `"https://api.typesafe.ai/v1"` | API base URL; env `TYPESAFE_BASE_URL` overrides |
| `key_env` | `["TYPESAFE_API_KEY", "AI_GATEWAY_API_KEY"]` | Env vars searched for the key, in order |
| `key_file` | `"~/.config/jevtest/typesafe.key"` | Key file, used when no env var is set |
| `views` | `["names", "body"]` | Stage-2 views |
| `batch` | `100` | Questions per request |
| `concurrency` | `4` | Parallel requests |
| `max_questions` | `6000` | Upper bound on questions per run |
| `max_state_chars` | `24000` | Diff/context size sent per request |
| `max_test_chars` | `800` | Test source per `body` question |
| `max_group_chars` | `600` | Group description per screening question |
| `timeout_secs` | `30` | Jev time budget; Jev never blocks longer than this in total |
| `cache_dir` | `"~/.cache/jevtest"` | Response cache |

### `[changes]`

Which changes feed selection. See [Choosing what changed](#choosing-what-changed).

| Key | Default | Meaning | CLI |
|---|---|---|---|
| `mode` | `"auto"` | `auto` \| `uncommitted` \| `staged` \| `unstaged` \| `branch` \| `last` \| `since` \| `range` | `--changes MODE` or a scope flag |
| `default_branch` | `"auto"` | `auto` = `origin/HEAD` → `origin/main` → `origin/master` → `main` → `master` | `--branch BASE` |
| `base` | `""` | Base revision for `mode = "range"` | `--base REV`, `--range A..B` |
| `last` | `1` | Commit count for `mode = "last"` | `--last N` |
| `since` | `"midnight"` | Window for `mode = "since"` (git date syntax) | `--since WHEN` |
| `recent` | `"midnight"` | auto's size-guard fallback window | |
| `max_files` | `60` | auto size guard: non-ignored files | |
| `max_lines` | `3000` | auto size guard: changed lines, both sides | |
| `include_untracked` | `true` | Untracked files count as added | |
| `files` | `[]` | Restrict the diff to these paths/globs, like `--files` | `--files PATH_OR_GLOB` |

### `[coverage]`

See [Coverage map](#coverage-map).

| Key | Default | Meaning |
|---|---|---|
| `map` | `".jevtest/coverage.json.gz"` | Map path (repo-relative or absolute); `coverage build` writes it, selection reads it |
| `policy` | `"gate"` | `gate` \| `must` \| `boost` \| `off`: what tests that execute a changed function get, and whether the rest are dropped |
| `max_age_commits` | `200` | A map further behind the change base, or built on a commit not in `HEAD`'s history, downgrades `gate`/`must` to `boost` |
| `llvm_bin` | `""` | Directory with `llvm-profdata` and `llvm-cov`; empty = rustc sysroot (`llvm-tools`), `PATH`, Homebrew `llvm` |
| `jobs` | `0` | Parallel mapping jobs in `coverage build`; `0` = one per core |

### Profiles

`[profile.NAME]` takes any `[select]`, `[changes]`, `[jev]`, `[tests]` or `[coverage]` key, flat. Choose it with `--profile NAME`
or `JEVTEST_PROFILE=NAME`.

```toml
[profile.ci]
top_n = 60
threshold = 0.3
on_jev_error = "full"
```

### Global flags

Change scope: `--changes MODE`, `--uncommitted`, `--staged`, `--unstaged`, `--branch [BASE]`, `--last N`,
`--commit REV`, `--since WHEN`, `--base REV`, `--head REV`, `--range A..B`, `--files PATH_OR_GLOB`.

Other: `--profile NAME`, `--config PATH`,
`--format human|json|command|filter|list`, `--json PATH`, `--no-jev`, `--without-jev reach|evidence`,
`--offline`, `--top-n N`, `--top-fraction F`, `--threshold F`, `--group-threshold F`, `--max-tests N`,
`--runner auto|nextest|cargo`, `-v`.

## Outputs and exit codes

| `--format` | stdout | stderr |
|---|---|---|
| `human` (default) | the runner command | summary |
| `command` | the runner command | |
| `filter` | the nextest filterset | |
| `list` | one selected test per line: `package<TAB>module::test<TAB>file:line` | |
| `json` | the full report | |

`--json PATH` writes the full report to a file with any format. Report fields: `version`, base, head,
`changes` (`requested`, `used`, `what`, `reason`, `base`, `head`, `files`, `lines`, `narrowed_from`),
profile, config path, changed files, escalations, rules fired, changed and reached packages, changed
items, per-stage Jev usage (asked, requests, splits, cache hits, input/output tokens, ms,
`est_cost_usd`), the filter and the command, and one entry per candidate:

| Field | Meaning |
|---|---|
| `package`, `file`, `line`, `module`, `name` | Test identity |
| `reach_depth` | Reverse-dependency hops from a changed package |
| `evidence` `{kind, symbols}` | Static evidence and the changed names it matched; `covered(n)` = no static evidence, but the coverage map says the test executes `n` changed functions |
| `group_noul` | Stage-1 score of the test's group |
| `nouls` `{names, body}` | Stage-2 score per view |
| `score` | Max over views + boost |
| `ranks` `{names, body}` | Rank per view |
| `coverage` `{known, covered}` | Whether the coverage map knows the test, and the changed functions it executes |
| `reasons` | Why it was selected or dropped (`not-covered` = dropped by the coverage gate, `covered` = must under `policy = "must"`) |
| `selected` | Final verdict |

Top-level `coverage`: `map`, `commit`, `age_commits`, `policy_requested`, `policy_used`, `covered`,
`gated_out`, `unknown`, `blind_items` (`{package, item, why}`), `no_gate_packages`, `reason`.

Exit codes:

| Code | Meaning |
|---|---|
| `0` | OK, including "nothing worth running" |
| `1` | Usage, config or git error |
| runner's code | `cargo jevtest run` returns what nextest or cargo test returned |

## CI usage

Use jevtest as a pre-merge fast lane; keep the full suite on main and for releases.

```sh
git fetch --no-tags origin main            # the branch scope needs the base branch
cargo jevtest run --profile ci --branch origin/main --json jevtest-report.json
```

- Set `TYPESAFE_API_KEY` as a CI secret. Without it the lane still runs, at reach level.
- Persist `~/.cache/jevtest` (or `jev.cache_dir`) between runs so reruns of the same diff are free.
- Release and main-branch builds: run the full suite (`cargo nextest run --workspace`), not jevtest.

## Cost and latency

- Price: $0.042 per million input tokens; output tokens are free.
- Measured on a 25-crate production Rust workspace (1,717 tests), three changes: ≈ $0.002 / $0.011 / $0.024 per uncached run (46k / 250k / 578k input tokens); Jev wall time 0.9 / 1.4 / 2.8 s.
- Requests run in parallel (`jev.concurrency`) and batched (`jev.batch`); the diff is billed once per request.
- Responses are cached in `~/.cache/jevtest` by request, so reruns on the same diff cost nothing.
  Clear with `cargo jevtest cache clear`.
- `est_cost_usd` in the report gives the estimate for each run.

## Troubleshooting

| Symptom | Cause | Do |
|---|---|---|
| stderr line says Jev is skipped | No key | See [Key setup](#key-setup). Default `without_jev = "reach"` runs every reached test; `--without-jev evidence` narrows to must-runs, static evidence, rules and `tests.always` |
| stderr notice that `cargo test` is used | `runner = "auto"` and cargo-nextest is missing | Nothing, or `cargo install cargo-nextest --locked` for faster runs and `-E` filtersets |
| runner not found with `--runner nextest` | cargo-nextest missing | `cargo install cargo-nextest --locked`, or `--runner auto` |
| Escalated to a full run | A `paths.full_run` path changed, a `[[rule]]` with `full = true` fired, or Jev failed with `on_jev_error = "full"` | Read the escalations in the summary or `--format json`. Expected after `Cargo.toml`/`Cargo.lock`/toolchain edits; let it run |
| Selection is much larger than expected | No key or `--no-jev` with `without_jev = "reach"`; Jev failed (stderr `jevtest: Jev failed: <cause>; …`) and `on_jev_error = "reach"`; or `--offline` with uncached questions (unjudged → selected) | Check the summary and `cargo jevtest doctor`; add a key, or use `--without-jev evidence` |
| HTTP 400 `max_tokens_exceeded` in `-v` output | Request too large | Nothing: jevtest splits the batch in half recursively and caches the refusal; refusals are not charged. The report counts `splits` |
| No network | Offline | `cargo jevtest run --offline`: cached answers are used; tests with uncached questions are unjudged and selected |
| Expected test not selected | Low score or no static link | `cargo jevtest explain <PATTERN>`; add a `[[rule]]` or `tests.always` entry |
| Config error naming an unknown key (exit 1) | Typo in `jevtest.toml` or a key in the wrong table | Fix the named key |
| Default branch cannot be resolved | No `origin/HEAD`, `origin/main`, `origin/master`, `main` or `master` | Set `changes.default_branch`, or pass `--branch BASE` / `--base REV` |
| Wrong change scope in the `jevtest: changes = …` line | `auto` guessed differently from what you meant | Pass a scope flag (`--uncommitted`, `--branch`, `--last N`, …); see [Choosing what changed](#choosing-what-changed) |
| `coverage:` line says `no gating in [...]` | The diff touches something coverage cannot see (see `blind_items` in `--format json`) | Nothing: those packages are selected as without a map |
| `coverage build` fails on llvm tools | No `llvm-profdata`/`llvm-cov` matching rustc's LLVM major | `rustup component add llvm-tools`, or set `coverage.llvm_bin` |

## Limits

- Static evidence is name-based (syn, no type resolution). Two functions with the same name in
  different modules look the same; a call through a trait object or a closure may not be seen. Jev
  and the top-N policy cover what static evidence misses.
- Tests generated by macros (`rstest` cases, `test-case`, custom harnesses) are invisible to the
  syn scan. Cover them with a `[[rule]]` that runs their package whole (`packages = [...]`).
- Doc tests are not run by nextest. Run `cargo test --doc` separately when you change public docs.
- The figures in the [README](../README.md) are early, small samples from two repositories, not a guarantee of recall.
