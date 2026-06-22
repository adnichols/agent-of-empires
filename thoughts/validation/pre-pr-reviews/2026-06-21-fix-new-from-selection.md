# Pre-PR implementation review: fix-new-from-selection

Date: 2026-06-21
Branch: fix-new-from-selection
Base: origin/main
Plan/scope: standalone user request, make "New from selection" default to the selected session's concrete worktree/project path. If the selected session is a worktree, create the new session in that same existing worktree rather than provisioning another worktree. Keep project/group header creation on repo/project paths. Include Linux build unblocks required to test this branch locally without system linker changes.

## Changed files

Committed diff against origin/main is currently empty; review scope is unstaged working-tree changes:

- Cargo.lock
- Cargo.toml
- src/tui/dialogs/context_menu.rs
- src/tui/dialogs/new_session/mod.rs
- src/tui/home/input.rs
- src/tui/home/tests.rs
- web/src/App.tsx
- web/src/components/WorkspaceSidebar.tsx
- web/src/components/__tests__/SessionRowTriage.test.tsx
- web/src/components/session-wizard/SessionWizard.tsx
- web/src/components/session-wizard/__tests__/structuredViewToggle.test.tsx
- web/tests/coverage-matrix.json

## Review cycle 1

### GPT-5.5 reviewer

Verdict: CLEAN_FOR_PR

Summary: no P1/P2 findings. Reviewer verified TUI selected-session creation uses `project_path`, worktree sessions disable worktree creation, project/group header creation remains repo-path based, web row creation passes `path`, `repoPath`, and `useWorktree: false` correctly, wizard omits `worktree_branch` when `useWorktree` is false, tests cover key regression paths, and Cargo changes are scoped.

### GLM-5.2 reviewer

Verdict: CLEAN_FOR_PR

Summary: no P1/P2 findings. Reviewer verified TUI, web, server request semantics, dependency changes, and runtime linkage. Two non-blocking P3/QUESTION items were noted.

## Triage

| Finding | Reviewer | Severity | Scope | Decision | Evidence |
| --- | --- | --- | --- | --- | --- |
| New session created in an existing worktree has no `worktree_info`, so later grouping/management may not treat it as a managed worktree session | GLM | P3 | QUESTION | Non-blocking follow-up, do not fix in this PR | Current scope requires creating in the same existing worktree and not provisioning another worktree. Both are met. Adopting worktree metadata is a product design extension. |
| Web heuristic for existing worktree differs from TUI in an effectively unreachable unmanaged worktree/main-repo-equal state | GLM | P3 | QUESTION | Non-blocking, no fix | Worktree directories are distinct from their main repo path; current web behavior is acceptable even if the state were reached. |

No blocking P1/P2 findings were reported by either reviewer.

## Fixes applied after review

None required for P1/P2 issues.

## Verification

Completed before review:

- `cargo test test_shift_n_prefills_existing_worktree_path_for_worktree_session`, passed
- `cargo test test_session_context_menu_new_session_prefills_from_session`, passed
- `cargo fmt --check`, passed
- `git diff --check`, passed
- Installed current branch with `cargo install --path . --features serve --profile dev-release --force`, passed
- Runtime linkage check on installed binary, no dynamic `libgit2` dependency

Completed during review gate:

- `cd web && npm run test:unit -- src/components/__tests__/SessionRowTriage.test.tsx src/components/session-wizard/__tests__/structuredViewToggle.test.tsx`, passed
- `cd web && npx tsc -b`, passed
- `cd web && npm run format:check && npm run lint && node tests/validate-coverage-matrix.mjs`, passed

Final verification after review:

- `cargo clippy --features serve -- -D warnings`, passed
- `cargo test --features serve test_shift_n_prefills_existing_worktree_path_for_worktree_session`, passed
- `cargo test --features serve test_session_context_menu_new_session_prefills_from_session`, passed
- `umask 077 && cargo test --features serve server::login::tests -- --test-threads=1`, passed
- `env -u TMUX -u TMUX_PANE -u TMUX_TMPDIR cargo test --features serve tmux::utils::tests::tmux_command_uses_aoe_owned_server_and_clears_tmux_client_env -- --test-threads=1`, passed

Full-suite note: `cargo test --features serve` failed locally with 4107 passed, 6 failed. The failures were pre-existing environment-sensitive tests: five `server::login::tests` fail under this shell's permissive `umask 0002` because their security check rejects group-writable temp dirs, and one tmux env test fails when `TMUX_TMPDIR` is inherited. Rerunning those scopes with `umask 077` and with `TMUX*` unset passed, so no branch code change was required.

## Final gate result

GPT verdict: CLEAN_FOR_PR
GLM verdict: CLEAN_FOR_PR
Blocking P1/P2 findings: none
Remaining non-blocking follow-ups: the two P3/QUESTION items listed above.
