# cargo-jevtest

Picks the Rust tests worth running for a git diff: changed packages plus their reverse
dependencies, static test enumeration with syn, then TypeSafe Jev in two stages: one Noul per
(package, file, module) group of tests, then one per test inside the groups that stay in
(`--group-threshold`, `--threshold`). Prints (or runs) a `cargo nextest run` command limited to
the selected tests.

## Usage

```
cargo install --path .
cargo jevtest                                  # merge-base with origin/HEAD vs working tree
cargo jevtest --base main~3 --head main --json report.json
cargo jevtest --no-jev                         # deterministic arm only (every candidate)
cargo jevtest --run -- --no-fail-fast          # run nextest, extra args after --
```

The API key comes from `TYPESAFE_API_KEY` or `~/.config/jevtest/typesafe.key`; optional
`TYPESAFE_BASE_URL` and `TYPESAFE_MODEL`. Responses are cached in `~/.cache/jevtest/`, keyed by the
sha256 of the request body, so reruns on the same diff cost nothing. The summary goes to stderr;
`cargo jevtest --help` lists every option.
