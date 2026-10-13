# Workflow Strategy

The workflows in this directory are split so that pull requests get fast, review-friendly signal while `main` still gets the full cross-platform verification pass.

## Pull Requests

- Required checks run against GitHub's synthetic merge commit, not the pull
  request head alone. This includes changes already on `main` and catches
  conflicts before they reach the branch.
- `bazel.yml` is the main pre-merge verification path for Rust code.
  It runs Bazel `test` and Bazel `clippy` on the supported Bazel targets,
  including the generated Rust test binaries needed to lint inline `#[cfg(test)]`
  code.
- `rust-ci.yml` keeps the Cargo-native PR checks intentionally small:
  - `cargo fmt --check`
  - `cargo shear`
  - the four tmux resize regressions on Linux for Rust changes, using `tui-terminal.yml`
  - `argument-comment-lint` on Linux, macOS, and Windows
  - `tools/argument-comment-lint` package tests when the lint or its workflow wiring changes

## Post-Merge On `main`

- `bazel.yml` also runs on pushes to `main`.
  This re-verifies the merged Bazel path and helps keep the BuildBuddy caches warm.
- `rust-ci-full.yml` is the full Cargo-native verification workflow.
  It keeps the heavier checks off the PR path while still validating them after merge:
  - the full Cargo `clippy` matrix
  - the full Cargo `nextest` matrix via per-platform archive-backed shards
  - Windows ARM64 nextest archives cross-compiled on Windows x64, then replayed on native Windows ARM64 shards
  - release-profile Cargo builds
  - cross-platform `argument-comment-lint`
  - Linux remote-env tests
  - the four tmux resize regressions through `tui-terminal.yml`

## Rule Of Thumb

- If a build/test/clippy check can be expressed in Bazel, prefer putting the PR-time version in `bazel.yml`.
- Keep `rust-ci.yml` fast enough that it usually does not dominate PR latency.
- Reserve `rust-ci-full.yml` for heavyweight Cargo-native coverage that Bazel does not replace yet.

## TUI Terminal Delivery Gate

`tui-terminal.yml` builds the CLI from the checked-out revision before running the
four ignored `suite::resize_reflow::tmux_*` tests, with retries disabled. The PR
workflow includes this job in its required result. It selects all Rust changes
conservatively because shared dependencies can affect TUI startup and rendering;
pure documentation changes do not trigger it. Full CI always runs it, and the
workflow can also be dispatched for a complete TUI acceptance run.

Ordinary `just test -p codex-tui` stays lightweight and leaves these tests ignored.
For local terminal acceptance, use the build and filtered `just test` commands in
`AGENTS.md`. Tests require tmux and local loopback access, use mock model responses,
and isolate their terminal servers from user sessions. CLI construction is the
main extra cost; the dedicated gate makes that cost visible instead of adding it
to every focused test run.
