# TrustCore — notes for Claude Code

## Two sessions share this repo
- **Before editing ANY file, read `~/projects/trustcore-session-locks.md`.** Never touch a path
  another session has claimed. Claim your own exact paths under your session's heading before
  editing, and remove them when you're done.
- Shared work queue: `~/projects/trustcore-backlog.md`. Record what you finished and what you
  reviewed there, not only in chat.
- Coordinate with the other session directly (SendMessage) and cross-review each other's work.
  Escalate to Tanish only what is his: product/UX calls, hard-to-undo actions, commit/push
  approval. Why: he was being used as a relay for decisions the two sessions could settle.

## Hard rules
- Work on `feature/mesh-cluster`. `feature/sovereign-workbench` and `feature/ui-refresh` were
  merged and deleted on 2026-11-01. Don't recreate them. Touch `main` only when Tanish asks for a
  merge, every time. Never commit, push, rebase, or create or switch branches without explicit
  permission, every time.
- Enhance the working codebase; don't degrade it. The build has known functional defects, so a
  green suite is not evidence the behaviour is right.
- Ship documentation in the same change: append to `docs/02-execution-log.md` in its existing
  voice (what was broken, the evidence, what changed, what was deliberately not done and why,
  the takeaway). The docs are a learning artifact, not overhead.
- Never share `CARGO_TARGET_DIR` between two checkouts of the same workspace (e.g. a demo worktree); give each its own. Why: cargo names workspace-crate artifacts the same regardless of folder, so the demo build overwrote FEATURE's and its tests linked stale code (E0407) until `cargo clean`.
