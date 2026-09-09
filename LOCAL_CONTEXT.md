# Local context backend

This fork is based on `rust-v0.153.2` (`657a993cbee87acf52d14b758ce49dbd46d1b8eb`).
It adds a local backend to the existing experimental context-management tools.
Synthetic native acceptance tests cover manual and automatic rollover, omitted
original user/tool text recovery, and restarting the same thread. A local build
is not a published release or a production installation.

```toml
model = "gpt-6-astra"
model_context_window = 872000
model_auto_compact_token_limit = 512000

[features.context_management]
experimental_mode = true
backend = "local"
```

`backend` is a fork-only setting. It accepts `codex` (the default) and `local`.
The default preserves the upstream backend's provider and account eligibility
checks. Selecting `local` does not grant access to any official backend service
or change the inference provider.

When a launcher creates temporary Codex homes, set the optional fork-only
`local_store_dir` in the same `[features.context_management]` table to an
absolute, normalized directory owned by the user. It replaces the default
`CODEX_HOME/context-management-local` root for both notes and backend markers.
All path components must be real directories, without symlinks or `.` / `..`
components. New local threads create missing directories privately; resumed
threads require an existing compatible record. Reuse the same explicit root
with the native saved rollout when resuming in a different Codex home. This
setting does not select or broaden the history source, and does not migrate
existing state automatically.

Local mode enables the native token-budget and history-notes integration for
the root thread. Explicitly disabling token-budget or its history-notes
integration while requesting local context is an error. The existing
token-budget reminder, guidance and fallback controls remain available. The
automatic-management threshold is not a strict per-request context cutoff:
native rollover and its configured fallback buffer retain their upstream
semantics.

The local tools keep notes under
`CODEX_HOME/context-management-local/<thread-id>/notes/`. `INDEX.md` supplies a
bounded recovery hint. History queries flush the active thread's existing
store and read original persisted text with window and record references.
They do not discover or search other sessions. Non-text, encrypted, missing,
truncated or incompletely scanned data is reported explicitly. Each scan is
limited to 64 MiB and 10,000 records, with at most 4 MiB per record; tool results
are limited to 8,000 UTF-8 bytes and the recovery hint to 4,000 bytes. Accepted
local call JSON is limited to 4,000 bytes; note write/append text is limited to
3,000 UTF-8 bytes per call, while repeated appends can grow a file to 1 MB.
These are validated-call limits; malformed model output is rejected without
rewriting the native raw transcript. Long
readable items within the scan limit can be read in character ranges. These
limits describe locally persisted data, not a promise to restore content that
upstream never wrote to the rollout.

Local context PreToolUse/PostToolUse hooks remain enabled and may block calls,
but their input/output and ordinary tool diagnostic logs contain only
`local_context_private` metadata. Hooks cannot rewrite hidden private arguments;
returning the same redacted input is a no-op. Full text remains available to the
model and its native thread history/trace. This does not isolate data from the
owner of the Codex home or trusted in-process extensions, which already own the
conversation. The official backend's hook and logging behavior is unchanged.

Start a new persistent thread when enabling this backend. The host creates a
versioned `backend.json` beside its notes. Resuming the same thread requires the
same compatible backend record; an older unmarked thread cannot silently become
a local-context thread. Preserve both the normal rollout and this local store
when moving a Codex home. Ordinary native children, ephemeral threads, and
inherited fork history are outside this milestone's local activation support.
Model changes within an activated thread retain the selected recovery backend.

Production launchers must select a durable local store and pin a tested fork
package before enabling the example in ordinary sessions. The synthetic tests
also cover recovering original user/tool text and notes with different Codex
homes sharing one explicitly configured store.

## Nix package

The `packages.x86_64-linux.codex-rs` and `default` outputs use pinned Rust 1.97.1
and the upstream canonical package assembler. CLI, code-mode host and bwrap are
built together; V8 inputs and upstream zsh/rg resources have fixed hashes.
The packaged zsh is adapted with Nix's ELF interpreter/RPATH fixup. All required
components live under the canonical package root, including `codex-package.json`.
Other platforms retain the upstream source recipe and are not covered by this
Linux package's acceptance. Nix evaluation alone does not establish a working
package: require a completed build and actual CLI/app-server/code-mode smoke
checks before publishing or consuming it.

## Maintaining the fork

Keep upstream releases separate from the local patch branch. For each candidate
upgrade, merge the explicit upstream release into a new candidate worktree,
review changes to feature parsing, token-budget activation, thread persistence,
rollout decoding and extension tool schemas, then run the affected tests and
package smoke checks. Publish or consume a candidate only after its recovery
tests pass. Keep the previous immutable package available for rollback.

The original local candidate reused upstream's `just assemble-codex-package`
builder with Rust 1.97.1. The Nix package now pins that same compiler, retains
the explicit `microsoft/mxc` Git dependency hash and supplies the complete
resource set. Keep packaging changes separate from context backend patches.

There is no need to decide on a second packaging-repository fork to test this
source candidate. Distribution, repository visibility and production consumer
wiring are a separate milestone after a runnable local result is accepted.
