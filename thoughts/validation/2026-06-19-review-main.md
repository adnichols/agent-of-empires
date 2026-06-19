---
date: 2026-06-19T20:46:24Z
branch: main
commit: c7eda057
type: review
status: complete
---

# Code Review: AOE tmux isolation fix

## Summary
- Files reviewed: 25 tracked files plus `docs/bug-reports/2026-06-19-daemon-inherits-tmux-server.md`
- Review posture: normal quality review, repeated until GPT and GLM both agreed merge-ready
- Issues found: 0 critical, 0 important, 0 minor remaining
- Overall: pass, merge-ready

## Reviewers

### GPT quality-reviewer
Final verdict: merge-ready, no actionable findings.

Earlier blockers found and fixed:
- Legacy/default-server same-name sessions could become invisible and be duplicated.
- In-tmux `switch-client` was routed through the AOE-owned helper and lost current-client context.
- `get_session_info_for_current()` mixed AOE-server and current-client metadata reads.
- Legacy duplicate test needed panic-safe env restoration and an explicit no-duplicate assertion.

### GLM quality-reviewer-glm
Final verdict: merge-ready, no actionable findings.

Earlier blockers found and fixed:
- Tests still created/captured/cleaned AOE sessions on the default tmux server.
- In-tmux attach fallback forced a nested attach by clearing `TMUX`.
- E2E/tool-session tests needed to reflect AOE-owned tmux server routing.

## Critical Issues
None remaining.

## Important Issues
None remaining.

## Minor Issues
None remaining.

## Positive Findings
- AOE-owned tmux operations now route through a single `tmux_command()` helper using the explicit AOE tmux server.
- Daemon startup clears inherited `TMUX` and `TMUX_PANE` while preserving `TMUX_TMPDIR`.
- Current-client operations, including `switch-client`, same-client attach fallback, and `aoe tmux-status` metadata reads, preserve caller tmux context.
- Same-name legacy/default-server sessions are refused before creating an AOE-server duplicate.
- Regression coverage now protects owned-server isolation and legacy duplicate refusal.
- Tests and E2E fixtures were migrated to use the same AOE tmux server as production where they operate on AOE-managed sessions.
- The bug report documents the stronger invariant and the pre-upgrade restart/stop caveat.

## Verification
Passed:
- `cargo fmt --check`
- `cargo clippy --features serve -- -D warnings`
- `cargo test --features serve --lib -- --test-threads=1`
- `cargo test --features serve --tests`

Targeted checks also passed during review:
- `cargo test --features serve tmux::utils::tests::create_refuses_duplicate_legacy_default_server_session -- --exact --nocapture`
- `cargo test --features serve --test acp_runner_orphan -- --nocapture`
- `cargo test --features serve --test e2e -- tool_sessions --nocapture`
- `cargo test --features serve --test e2e -- new_session::test_right_click_on_session_row_opens_rename_delete_menu --nocapture`
- reviewer-cited serve E2E tests passed isolated

Note: one full parallel lib run hit an unrelated file-watch test race; the same test passed isolated, and the final lib verification passed with `--test-threads=1`.

## Action Items
- [x] Fix GPT and GLM review findings.
- [x] Rerun reviewers until both agree merge-ready.
- [x] Run final verification.
