# Pre-PR implementation review, performance-improvements

Date: 2026-06-22
Branch: `performance-improvements`
Base or range: `origin/main...HEAD` for committed diff, plus unstaged working-tree changes
Plan or scope: standalone performance optimization after profiling AOE CPU usage. Scope is reducing wasteful TUI and tmux status polling CPU while preserving session status correctness and live-send drift behavior.

## Changed files

Committed diff against `origin/main...HEAD`: empty.

Unstaged working-tree changes:

- `src/session/instance.rs`
- `src/tmux/mod.rs`
- `src/tui/app.rs`
- `src/tui/home/input.rs`
- `src/tui/home/mod.rs`
- `src/tui/status_poller.rs`

Final unstaged stat: 6 files changed, 77 insertions, 18 deletions.

## Review cycle 1

GPT verdict: `P1_P2_FOUND`
GLM verdict: `CLEAN_FOR_PR`

| Finding | Reviewer | Severity | Scope | Decision | Evidence |
| --- | --- | --- | --- | --- | --- |
| Warm sessions could leave `SESSION_CACHE` stale after tying cache refresh to adaptive status tiers | GPT | P2 | REGRESSION_FROM_THIS_DIFF | Fixed | `STATUS_REFRESH_INTERVAL` became 2s, warm sessions poll every 5 cycles, cache TTL is 2s, and live-send drift only exits on `Some(false)`. Cache refresh could be delayed to about 10s for Idle sessions. |
| Stale comment in `batch_pane_metadata` | GLM | P3 | IN_PLAN | Fixed while addressing review cleanup | Comment still referenced paired `refresh_session_cache` call after that call was removed from the TUI status poller. |
| Redundant server `refresh_session_cache()` before `batch_pane_metadata()` | GLM | P3 | OUT_OF_SCOPE_FOLLOW_UP | Deferred | `src/server/mod.rs` is outside this TUI-focused diff. Behavior is safe because the second cache write is consistent. Tracking destination: future server poller performance cleanup. |
| New cache test lacked serial isolation | GLM | P3 | IN_PLAN | Fixed | Test writes the global `SESSION_CACHE`; added `#[serial_test::serial]`. |
| `parse_pane_metadata` tests do not cover production 6-field format | GLM | P3 | IN_PLAN | Deferred | Parser already accepts 4 or more fields and ignores extras. Existing parse tests plus the new cache-refresh test cover the changed producer and cache consumer. |
| Error-tier recheck interval increased from about 30s to 120s | GLM | P3 | IN_PLAN | Deferred | Slower auto-recovery for `Status::Error` is a non-blocking tradeoff from the CPU reduction. User actions still bypass the poller. |

## Fixes applied after cycle 1

- Added `should_refresh_tmux_metadata(&instances)` in `src/tui/status_poller.rs`.
- Kept tmux metadata and `SESSION_CACHE` refresh on every 2s status tick when any non-frozen session exists.
- Kept expensive per-session status detection on the existing hot, warm, and cold adaptive tiers.
- Added unit coverage for warm sessions requiring metadata refresh and frozen sessions skipping it.
- Updated the stale `batch_pane_metadata` trace comment.
- Marked `test_pane_metadata_refreshes_session_cache` serial to avoid global cache test races.

## Verification after fixes

Commands run successfully:

```bash
cargo fmt
cargo test --lib status_poller
cargo test --lib pane_metadata_refreshes_session_cache
cargo test --lib parse_pane_metadata
cargo clippy --lib -- -D warnings
cargo test --lib
```

Full `cargo test --lib` result: 3187 passed, 0 failed, 2 ignored.

## Review cycle 2

GPT verdict: `CLEAN_FOR_PR`
GLM verdict: `CLEAN_FOR_PR`

| Finding | Reviewer | Severity | Scope | Decision | Evidence |
| --- | --- | --- | --- | --- | --- |
| Prior warm-session cache staleness P2 | GPT, GLM | P2 | REGRESSION_FROM_THIS_DIFF | Resolved | Rereview confirmed metadata/cache refresh is now independent from adaptive status polling tiers. |
| Cache not explicitly cleared on `batch_pane_metadata()` failure | GLM | P3 | IN_PLAN | Deferred | On failure, stale cache can remain for the 2s TTL, then callers fall back to `has-session` or safe live-send `None` behavior. No data loss or false drift. Tracking destination: optional follow-up if stricter immediate error detection is desired. |
| Cache TTL equals refresh interval | GLM | P3 | IN_PLAN | Deferred | Jitter can cause brief `None` cache reads and occasional fallback subprocesses, but live-send treats `None` safely and status checks fall back to `has-session`. Tracking destination: optional follow-up if runtime profiling shows jitter-driven subprocesses are material. |
| Server path redundant cache refresh | GLM | P3 | OUT_OF_SCOPE_FOLLOW_UP | Deferred | `src/server/mod.rs` still calls `refresh_session_cache()` before `batch_pane_metadata()`. This is outside the TUI poller optimization scope and is safe. Tracking destination: future server poller performance cleanup. |

## P3 follow-up pass

User requested resolving all P3 findings as well.

Fixes applied:

- `src/tmux/mod.rs`: `batch_pane_metadata()` now clears `SESSION_CACHE` when `tmux list-panes` exits non-zero or fails to spawn, matching the old `refresh_session_cache()` failure behavior.
- `src/tmux/mod.rs`: cache TTL now uses a 3 second constant, giving margin over the TUI's 2 second metadata refresh cadence.
- `src/server/mod.rs`: removed the redundant `refresh_session_cache()` call before `batch_pane_metadata()` in the server status loop.
- `src/tmux/mod.rs`: added production 6-field parser coverage for `#{session_name}|#{pane_index}|#{pane_dead}|#{pane_current_command}|#{pane_pid}|#{session_activity}`.
- `src/tmux/mod.rs`: added cache clearing coverage.
- `src/tui/status_poller.rs`: changed cold Error tier from 60 cycles to 15 cycles, preserving roughly the old 30 second Error recheck cadence after the status tick moved to 2 seconds.
- `src/server/mod.rs`: removed the startup-recovery `refresh_session_cache()` immediately before `batch_pane_metadata()`.
- `src/session/instance.rs`: replaced integer-second truncation in Error recheck throttling with a precise `Duration::from_secs(30)` comparison.
- `src/tui/status_poller.rs` and `src/tui/attached_status_hooks.rs`: split the main TUI and attached-status-hook polling policies so the 2s main TUI keeps a 15-cycle cold tier and cache refresh every tick, while the 500ms attached hook watcher keeps the old 60-cycle cold tier and only fetches tmux metadata on actual polling cycles.

Verification after P3 fixes:

```bash
cargo fmt
cargo test --lib status_poller
cargo test --lib parse_pane_metadata
cargo test --lib clear_session_cache_removes_cached_presence
cargo clippy --lib -- -D warnings
cargo test --lib
cargo clippy --features serve -- -D warnings
```

After the startup-recovery and Error timing fixes, these commands were rerun successfully:

```bash
cargo fmt
cargo test --lib status_poller
cargo test --lib parse_pane_metadata
cargo test --lib clear_session_cache_removes_cached_presence
cargo clippy --lib -- -D warnings
cargo clippy --features serve -- -D warnings
cargo test --lib
```

A final confirmation review found one more P3 in the attached-status-hook path: it still ticks every 500ms, so the new main-TUI cache-refresh-every-cycle behavior would have made attached status hooks run `tmux list-panes -a` every 500ms. The polling policy split above fixed that path.

After the attached-hook fix, this combined command was rerun:

```bash
cargo fmt
cargo test --lib status_poller
cargo test --lib parse_pane_metadata
cargo test --lib clear_session_cache_removes_cached_presence
cargo clippy --lib -- -D warnings
cargo clippy --features serve -- -D warnings
cargo test --lib
```

The parallel full-suite run hit unrelated file-watch test races on two different tests; both passed on direct rerun. The final deterministic full-suite run succeeded:

```bash
cargo test --lib -- --test-threads=1
```

Full serial `cargo test --lib` result after P3 fixes: 3190 passed, 0 failed, 2 ignored.

## Final no-P3 confirmation

After the attached-status-hook policy split, both final read-only reviewers checked the current diff again.

GPT verdict: `CLEAN_NO_P1_P2_P3`
GLM verdict: `CLEAN_NO_P1_P2_P3`

## Final gate result

No unresolved P1, P2, or P3 findings remain. P3 follow-ups from the implementation review, including the attached-status-hook polling regression found during final confirmation, have been addressed.
