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
| git | Run from inside the repo. If `origin/HEAD`, `origin/main` and `origin/master` are all missing, pass `--base REV` or set `select.base`. |
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

Diff selection:

| You want | Flags |
|---|---|
| Working tree (incl. untracked `.rs`) vs merge-base with `origin/HEAD` | none (default) |
| Staged changes only (pre-commit) | `--staged` |
| A commit range | `--base REV --head REV` |

Decision rules:

- Before finishing a change: `cargo jevtest run`. If it is green, you are done with tests.
- If it fails: fix and rerun `cargo jevtest run`. Cached Jev answers make the rerun cost nothing.
- If you expect a test to run and it did not: `cargo jevtest explain <name-or-path-fragment>`. If the miss is real, add a `[[rule]]` or a `[tests] always` filterset instead of rerunning everything.
- If the summary says it escalated to a full run, let it run. The reason is in the summary and in the report's escalations.
- If it selected nothing ("nothing worth running", exit 0): the diff touched only ignored paths. Done.
- Need more recall for a risky change: `--top-n 60`, `--threshold 0.3`, or `--no-jev` (with the default `--without-jev reach`: every reached test).
- No key and the reach-level run is too big: `--without-jev evidence` (must-runs, static evidence, rules, `tests.always` only).
- Need it faster: `--max-tests N` (drops the lowest-scored picks that are not must-runs).

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
    workspace: 91 tests instead of 28).
  - `evidence`: must-runs (`Changed`, and `Direct`/`Helper` under `static_evidence = "must"`) + tests with
    any static evidence + `[[rule]]` matches + `tests.always`. Smaller, lower recall.
- `--offline`: cached Jev answers are used; tests whose questions are not cached are unjudged and selected.

## How selection works

Each layer records its verdict per candidate; `explain` and the JSON report show them.

| # | Layer | What it does | Cost | Config keys |
|---|---|---|---|---|
| 1 | Intake | Diff (range, working tree incl. untracked, or staged); changed lines on both sides; changed items from old and new source (fn, `Type::method`, struct, enum, trait, const, static, type alias, `macro_rules`, deleted items too) | git + parse | `select.base`, `select.include_untracked`; `--base`, `--head`, `--staged` |
| 2 | Path policy | `full_run` paths → full run; `ignore` paths dropped; `[[rule]]` matches → must filtersets, whole packages or full run; non-Rust file in a package → `non_rust` policy; unparsable `.rs` → whole package | free | `paths.full_run`, `paths.ignore`, `[[rule]]`, `select.non_rust` |
| 3 | Reach | Changed packages + reverse deps (normal, dev, build) | `cargo metadata` | `select.reach_depth` |
| 4 | Discovery | Tests in reached packages, via `syn`, with spans and source | parse | none |
| 5 | Static evidence | `Changed` (test's own span changed) → must. `Direct` (test names a changed item) and `Helper` (a same-module non-test fn it calls does) → must or boost. `Transitive` (reaches a changed item through the name-based call graph) → boost. Boost = skips screening, +0.2 score (cap 1.0) | parse, no model | `select.static_evidence`, `select.call_graph_depth` |
| 6 | Screening (Jev stage 1) | One Noul per (package, file, module) group of non-must candidates; groups below `group_threshold` dropped unless a member has evidence | Jev, small | `select.group_threshold`, `jev.max_group_chars` |
| 7 | Judging (Jev stage 2) | One Noul per surviving test per view: `names` (package, module, name; ~50 tokens) and `body` (test source). Score = max over views + boost; ranks per view | Jev, most tokens | `jev.views`, `jev.max_test_chars`, `jev.batch`, `jev.concurrency`, `jev.max_questions`, `jev.max_state_chars` |
| 8 | Policy | selected = must ∪ rules ∪ `tests.always` ∪ top-`top_n` per view ∪ {score ≥ `threshold`} ∪ unjudged; minus `tests.never` unless the test changed; then `max_tests` / `min_tests` by score. Without Jev (no key, `--no-jev`): `without_jev = "reach"` selects every reached test; `"evidence"` selects must ∪ static evidence ∪ rules ∪ `tests.always` | free | `select.top_n`, `select.threshold`, `select.max_tests`, `select.min_tests`, `select.without_jev`, `tests.always`, `tests.never` |
| 9 | Output | nextest: `cargo nextest run -p P... -E '<expr>'`. cargo: one `cargo test -p P -- name...` per package. `auto`: nextest if installed, else cargo (one stderr notice) | free | `select.runner`; `--format`, `--json` |

If Jev fails (401/402/403/5xx, timeout, connection error, malformed reply), `select.on_jev_error` decides: `reach` (default: select every reached test, exit 0, one stderr line `jevtest: Jev failed: <cause>; selecting every reached test (on_jev_error = reach)`), `full`, or `fail`. Jev never blocks longer than `jev.timeout_secs` in total.

## Configuration reference

Precedence: defaults < file top level < `[profile.NAME]` < CLI flags. Unknown keys are an error that
names the key.

### `[select]`

| Key | Default | Meaning | CLI |
|---|---|---|---|
| `base` | `"auto"` | Base revision. `auto` = merge-base(HEAD, `origin/HEAD` \| `origin/main` \| `origin/master`) | `--base REV` |
| `include_untracked` | `true` | Untracked `.rs` files count as added (working-tree mode only) | |
| `top_n` | `30` | Top N tests per Jev view are selected | `--top-n N` |
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

### Profiles

`[profile.NAME]` takes any `[select]`, `[jev]` or `[tests]` key, flat. Choose it with `--profile NAME`
or `JEVTEST_PROFILE=NAME`.

```toml
[profile.ci]
top_n = 60
threshold = 0.3
on_jev_error = "full"
```

### Global flags

`--base REV`, `--head REV`, `--staged`, `--profile NAME`, `--config PATH`,
`--format human|json|command|filter|list`, `--json PATH`, `--no-jev`, `--without-jev reach|evidence`,
`--offline`, `--top-n N`, `--threshold F`, `--group-threshold F`, `--max-tests N`,
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
profile, config path, changed files, escalations, rules fired, changed and reached packages, changed
items, per-stage Jev usage (asked, requests, splits, cache hits, input/output tokens, ms,
`est_cost_usd`), the filter and the command, and one entry per candidate:

| Field | Meaning |
|---|---|
| `package`, `file`, `line`, `module`, `name` | Test identity |
| `reach_depth` | Reverse-dependency hops from a changed package |
| `evidence` `{kind, symbols}` | Static evidence and the changed names it matched |
| `group_noul` | Stage-1 score of the test's group |
| `nouls` `{names, body}` | Stage-2 score per view |
| `score` | Max over views + boost |
| `ranks` `{names, body}` | Rank per view |
| `reasons` | Why it was selected or dropped |
| `selected` | Final verdict |

Exit codes:

| Code | Meaning |
|---|---|
| `0` | OK, including "nothing worth running" |
| `1` | Usage, config or git error |
| runner's code | `cargo jevtest run` returns what nextest or cargo test returned |

## CI usage

Use jevtest as a pre-merge fast lane; keep the full suite on main and for releases.

```sh
git fetch --no-tags origin main            # merge-base needs the base branch
cargo jevtest run --profile ci --base origin/main --json jevtest-report.json
```

- Set `TYPESAFE_API_KEY` as a CI secret. Without it the lane still runs, at reach level.
- Persist `~/.cache/jevtest` (or `jev.cache_dir`) between runs so reruns of the same diff are free.
- Release and main-branch builds: run the full suite (`cargo nextest run --workspace`), not jevtest.

## Cost and latency

- Price: $0.042 per million input tokens; output tokens are free.
- Measured on a 25-crate production Rust workspace (1,717 tests), three changes: 38k / 168k / 282k Jev tokens ≈ $0.002 / $0.007 / $0.012 per run; Jev wall time under 2.5 s.
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
| Base cannot be resolved | No `origin/HEAD`, `origin/main` or `origin/master` | `--base REV` or `select.base` |

## Limits

- Static evidence is name-based (syn, no type resolution). Two functions with the same name in
  different modules look the same; a call through a trait object or a closure may not be seen. Jev
  and the top-N policy cover what static evidence misses.
- Tests generated by macros (`rstest` cases, `test-case`, custom harnesses) are invisible to the
  syn scan. Cover them with a `[[rule]]` that runs their package whole (`packages = [...]`).
- Doc tests are not run by nextest. Run `cargo test --doc` separately when you change public docs.
- The figures in the [README](../README.md) are early, small samples from two repositories, not a guarantee of recall.
