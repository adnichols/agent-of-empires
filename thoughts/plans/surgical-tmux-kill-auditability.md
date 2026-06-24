# Surgical tmux kill and auditability plan

## Status

execution-ready

UI impact: text-only, CLI help and log/audit output change. No dashboard UI flow changes are required.

## Goal

Remove AoE's destructive global tmux session sweep behavior. The legacy global sweep entry point should remain only as a non-destructive audited no-op, so any attempted broad kill is visible after the fact and in the TUI. Every real AoE tmux kill must target one explicit tmux session at a time, prove that session belongs to AoE, and write durable audit evidence before and after the kill attempt. If a product flow needs to stop multiple tmux sessions, it must loop over explicit session records and call the same single-session audited helper for each target.

## Decision Attention / Low-confidence Areas

None blocking. The product decision is explicit: remove broad all-AoE tmux killing entirely, require surgical per-session targeting, require clear AoE ownership, and make every tmux session kill auditable after the fact.

Repo planning gap: `thoughts/specs/product_intent.md` is missing. This plan is anchored to the direct product decision above and should not be blocked by that repo-bootstrap gap, but a separate bootstrap follow-up should add the product intent file.

## Why this plan exists

Current `aoe killall` treats AoE tmux sessions as disposable runtime cleanup. That is wrong for the product model. Tmux sessions are user-visible workspaces. Killing them globally destroys active workspace state and makes post-incident reconstruction hard.

The desired model is:

- infrastructure cleanup can stop daemon and worker processes;
- tmux workspace termination is destructive and must be scoped to one known session;
- multi-session operations are explicit loops over known sessions, not prefix sweeps;
- every tmux kill creates durable audit records that survive normal CLI process exit;
- every attempted global sweep creates a durable no-op audit record and a TUI-visible notice.

## Authority and inputs

- User directive in this session: remove global tmux kill functionality entirely.
- Existing investigation: `thoughts/debug/2026-06-22-tmux-kill-inventory.md`.
- Current global sweep: `src/cli/killall.rs:55` calls `crate::tmux::stop_all_sessions()`.
- Current broad matcher: `src/tmux/mod.rs:141` uses `name.starts_with(SESSION_PREFIX)`.
- Current broad sweep implementation: `src/tmux/mod.rs:159` lists all tmux sessions and kills every prefixed session.
- Current single-session helper: `src/tmux/utils.rs:319` `kill_session_if_present()`.
- Current per-instance fan-out: `src/session/instance.rs:3195` `kill_all_tmux_sessions()` and `src/session/instance.rs:3213` `kill_ancillary_tmux_sessions()`.
- Current tool fan-out by suffix: `src/tmux/tool_session.rs:157` `kill_all_tool_sessions_for_id()`.
- Existing session detection surfaces that need a scope audit: tmux name construction, tmux listing, stored instance lookup, terminal and tool session naming, idle reap candidate selection, group selection, and server API delete/archive paths.
- Existing ownership marker support: `src/tmux/env.rs` hidden env helpers and `AOE_INSTANCE_ID`.
- Logging reality: `src/main.rs:95` skips tracing subscriber initialization for normal one-shot CLI unless env logging is set, so auditability cannot rely on tracing alone.

## Current implementation reality

`aoe killall` is the only production path that kills all AoE-prefixed tmux sessions in one sweep. It is documented as destructive and unprompted in `src/cli/definition.rs:104` and implemented in `src/cli/killall.rs`.

`tmux::stop_all_sessions()` is inherently broad because it enumerates the AoE tmux server, accepts name-prefix ownership, kills pane process trees, and then runs `tmux kill-session` for each match. To minimize the compatibility surface while removing the dangerous behavior, this helper should become a non-destructive no-op that records an audit event and returns a result the caller can surface. No product path should be able to use it to kill tmux sessions.

Per-session operations already exist but are not fully centralized for auditability. Agent, terminal, container terminal, and tool sessions each call process-tree kill plus `tmux kill-session`. The implementation should converge these through a single audited helper.

Session detection also needs its own audit pass. The dangerous failure mode is not only killing too many sessions, but identifying too many sessions as eligible before the kill helper is called. All discovery code must be reviewed for prefix-only, suffix-only, substring, stale storage, group expansion, and tmux-server-wide listing behavior. Detection should prefer exact generated names from stored instance ids and typed ownership markers. Any broader discovery must be non-destructive and must filter by marker before it can produce kill candidates.

Existing hidden tmux env support can prove ownership, but coverage is incomplete. Primary sessions publish `AOE_INSTANCE_ID`; terminal, container terminal, and tool sessions need the same marker, plus a kind marker, if multi-session cleanup must be trustworthy.

One-shot CLI commands do not always initialize tracing, so durable auditability needs a direct append-only audit writer in addition to tracing events.

## Progress

- [x] P1: Make global tmux sweep a no-op
- [x] P2: Add durable audited single-session kill helper
- [x] P3: Mark and verify tmux ownership for every session kind
- [x] P4: Refactor multi-session flows into explicit verified loops
- [x] P5: Update command docs and regression coverage

## Resume instructions (agent)

Read this plan fully, then start with the first unchecked item in `## Progress`. Preserve the product invariant that no production code may enumerate tmux sessions and kill all AoE-prefixed names. Do not reintroduce prefix-only ownership. Each phase should be implemented tests-first where practical, then reviewed against the acceptance criteria and BDD scenarios before marking progress.

## Product intent alignment

The product should preserve user workspaces by default. Stopping infrastructure is not the same as destroying tmux sessions. A safe product should require explicit, scoped intent before killing any tmux workspace, and should leave an audit trail that makes later incident analysis possible without guessing.

Self-healing expectation: cleanup flows may repair stale records and remove dead metadata, but they must not kill a live tmux session unless the target is explicit and ownership is proven.

Fail-closed boundary: if AoE cannot prove that a tmux session belongs to the intended AoE instance and kind, it must skip the kill, log the skipped attempt, and surface a useful warning where the caller has a user-facing output channel.

## Locked decisions

- `aoe killall` must no longer kill tmux sessions.
- `tmux::stop_all_sessions()` may remain callable only as a non-destructive audited no-op.
- Prefix-only checks such as `name.starts_with(SESSION_PREFIX)` are insufficient for destructive tmux kills.
- Suffix-only, substring, or tmux-server-wide discovery is insufficient for destructive tmux kill candidate selection.
- Multi-session tmux teardown is allowed only as an explicit loop over known session records or marker-verified exact generated names.
- Every tmux kill attempt must write durable audit evidence independent of tracing subscriber initialization.
- Missing ownership proof means skip, not best-effort kill.
- Session detection audits must prove the candidate set cannot accidentally include unrelated tmux sessions before a destructive helper is called.

## Acceptance criteria

AC1. Running `aoe killall` stops the daemon and ACP workers as before, preserves all AoE tmux sessions, and writes an audit event that the global tmux sweep was intentionally skipped.

AC2. No production path can kill every AoE-prefixed tmux session by listing tmux sessions and applying only a name-prefix predicate.

AC3. Every tmux session kill goes through a single audited helper that records attempt, target identity, ownership proof, reason, caller, pane PID if known, tmux server name, result, and error text when present.

AC4. The audit trail is durable for one-shot CLI, TUI, and serve contexts, even when `AOE_LOG_LEVEL` and `AGENT_OF_EMPIRES_DEBUG` are unset.

AC5. Multi-session operations such as archive group, restart all, idle auto-stop, and delete perform explicit per-session calls and skip any tmux session whose AoE ownership cannot be proven.

AC6. Tool, terminal, container terminal, and primary agent tmux sessions all receive ownership markers at creation time.

AC7. Documentation and generated CLI reference no longer claim that `killall` kills tmux sessions.

AC8. When a global tmux sweep entry point is invoked from a TUI reachable path or while the TUI is running, the TUI surfaces a clear notice that the broad kill was blocked and tmux sessions were preserved.

AC9. Every code path that identifies tmux sessions for possible termination has an audited scope rule, exact stored session, exact generated ancillary name, or marker-verified discovery, with tests proving prefix-only, suffix-only, substring, stale-storage, and cross-server false positives are excluded.

## BDD scenarios

### Scenario S1, killall preserves tmux sessions

Given an AoE tmux server contains two verified AoE tmux sessions and one ACP worker is registered, when the user runs `aoe killall`, then the daemon and worker cleanup paths run, the two tmux sessions still exist afterward, and the audit log records a skipped global tmux sweep.

### Scenario S2, exact session stop is audited

Given a stored AoE session has a verified primary tmux session, when the user runs `aoe session stop <id>`, then AoE writes a durable audit entry before attempting the kill and another entry with the result.

### Scenario S3, missing ownership proof fails closed

Given a tmux session name matches an AoE naming pattern but lacks the expected hidden ownership marker, when a cleanup loop considers that session, then AoE skips the kill, writes a skipped audit entry, and continues with other verified targets.

### Scenario S4, group archive loops explicitly

Given a group contains three sessions, when the user archives the group, then AoE evaluates each stored session independently and never calls a global tmux sweep helper.

### Scenario S5, stale unconfigured tool session is not prefix-killed

Given a tool tmux session has the old suffix-based name but no ownership marker, when per-instance ancillary cleanup runs, then AoE does not kill it by suffix alone and records that ownership proof was missing.

### Scenario S6, one-shot CLI audit works without tracing env

Given no logging environment variables are set, when a one-shot CLI command kills one verified tmux session, then the audit file in the AoE app directory contains the kill attempt and result.

### Scenario S7, unsafe PID is not signaled

Given tmux reports pane PID `0` or `1`, when the audited kill helper resolves the pane PID, then AoE records a skipped unsafe PID audit entry and does not send a signal to that PID.

### Scenario S8, global sweep no-op is visible in the TUI

Given a TUI user invokes a flow that reaches the legacy global tmux sweep entry point, when `tmux::stop_all_sessions()` runs, then AoE kills no tmux sessions, writes a durable skipped-global-sweep audit entry, and shows a TUI notice that broad tmux killing is disabled.

### Scenario S9, session detection cannot over-capture

Given tmux contains sessions with similar names, stale AoE-looking prefixes, suffix collisions, and sessions on a different tmux server, when AoE builds a candidate list for cleanup, then only exact stored sessions or marker-verified generated names are eligible for destructive handling and every excluded candidate is test-covered or audit-visible.

## Phase-by-phase execution plan

### P1: Make global tmux sweep a no-op

#### End State

`aoe killall` no longer kills tmux sessions. The legacy global sweep helper remains only as an audited no-op. The command remains available for daemon and ACP worker panic cleanup, but it preserves tmux workspaces and reports that the tmux sweep was skipped.

#### Tests first

Add a regression test that creates an isolated AoE tmux session, runs the killall command path, and asserts the tmux session still exists. Add a unit test for `stop_all_sessions()` that proves it kills nothing, writes the skipped-global-sweep audit event, and returns status text suitable for CLI or TUI display.

BDD coverage: S1, S8.

#### Work

- Keep the call to `crate::tmux::stop_all_sessions()` from `src/cli/killall.rs` only if needed for minimal compatibility, but change the helper so it never enumerates, signals, or kills tmux sessions.
- Make `tmux::stop_all_sessions()` write a durable audit event with reason `global_sweep_disabled`, caller, process id, tmux server name if known, and result `skipped`.
- Change killall output so it reports that tmux sessions are preserved and the skipped sweep was audited.
- Add a TUI-facing notice path for the skipped global sweep result, using the existing TUI status or toast pattern rather than adding a new dashboard surface.
- Update command wording in `src/cli/definition.rs` so `killall` no longer claims to stop tmux sessions.
- Keep `aoe stop` trap guidance aligned with the new behavior.

#### Expected files

- `src/cli/killall.rs`
- `src/cli/definition.rs`
- `src/main.rs` only if command routing text or behavior needs adjustment
- `src/tmux/mod.rs`
- `src/tui/home/operations.rs` or the existing TUI notice module if the call is surfaced elsewhere
- `tests/e2e/cli.rs` or an existing tmux-aware integration test file

#### Decision dependencies

None.

#### Verify

```bash
cargo test --test e2e killall_preserves_aoe_tmux_sessions -- --nocapture
cargo test tmux:: -- --test-threads=1
```

### P2: Add durable audited single-session kill helper

#### End State

All tmux session destruction flows call one helper that performs ownership validation, process-tree cleanup, `tmux kill-session`, durable audit writes, and tracing events.

#### Tests first

Add unit tests around the audit writer and helper decision logic before changing callers. Tests should cover successful kill audit fields, tmux kill failure audit fields, unsafe PID skip, and one-shot CLI durability without tracing initialization.

BDD coverage: S2, S6, S7.

#### Work

- Introduce a single kill request type, for example `TmuxKillRequest`, with session name, expected instance id, expected kind, reason, and caller.
- Add a durable audit writer under the app directory, for example `tmux-kill-audit.log`, with one JSON line per event.
- Emit at least two events per attempted kill: `attempt` and `result`. Emit `skipped` when ownership or PID safety fails before signaling.
- Include timestamp, process id, binary build version if available, tmux server name, session name, expected instance id, expected kind, reason, caller, ownership proof source, pane PID, command result, and error text.
- Make audit writes best-effort but never silent: audit write failure should emit tracing and, for user-facing CLI paths, warning text if available.
- Add unsafe PID guards for `pid <= 1` before process signaling.
- Route `Session::kill`, terminal kill, container terminal kill, tool kill, and `kill_session_if_present` through the new helper or delete direct destructive variants.

#### Expected files

- `src/tmux/utils.rs`
- `src/tmux/session.rs`
- `src/tmux/terminal_session.rs`
- `src/tmux/tool_session.rs`
- `src/process/mod.rs`
- `src/session/mod.rs` or a new small audit module if app-dir utilities are needed
- Unit tests in the touched Rust modules

#### Decision dependencies

None.

#### Verify

```bash
cargo test tmux_kill_audit -- --test-threads=1
cargo test process:: -- --test-threads=1
```

### P3: Mark and verify tmux ownership for every session kind

#### End State

Every newly created AoE tmux session carries hidden ownership markers, and destructive helpers require those markers unless the caller is stopping one exact primary session that can be tied directly to a stored `Instance` and generated session name.

#### Tests first

Add tests that create or simulate primary, host terminal, container terminal, and tool tmux sessions and verify hidden `AOE_INSTANCE_ID` plus session kind markers are written. Add negative tests for missing marker, wrong instance id, wrong kind, similar name, suffix collision, and wrong tmux server.

BDD coverage: S3, S5, S9.

#### Work

- Add a hidden env constant for session kind, for example `AOE_SESSION_KIND`.
- On primary session creation, write `AOE_INSTANCE_ID` and `AOE_SESSION_KIND=agent` as part of launch finalization.
- On terminal and container terminal creation, write `AOE_INSTANCE_ID` and the correct kind marker after tmux creation.
- On tool session creation, write `AOE_INSTANCE_ID` and `AOE_SESSION_KIND=tool`.
- Add an ownership verification helper that reads hidden env and compares expected instance id and kind.
- Add a session-candidate helper that converts stored instance data and known session kinds into exact expected names before any destructive handling.
- For exact generated primary session kills, keep the same marker requirement unless implementation evidence proves a narrow stored-instance fallback is needed. If fallback is needed, it must be audited as `verified_by=stored_instance_exact_name`.
- Treat marker mismatch or absence as skip for destructive multi-session cleanup.
- Make broad tmux listing a discovery-only operation, never a kill-candidate source unless every result is marker-verified and server-scoped.

#### Expected files

- `src/tmux/env.rs`
- `src/tmux/session.rs`
- `src/tmux/terminal_session.rs`
- `src/tmux/tool_session.rs`
- `src/session/instance.rs`
- Relevant tests in those modules

#### Decision dependencies

None.

#### Verify

```bash
cargo test tmux::env:: -- --test-threads=1
cargo test tmux::session:: -- --test-threads=1
cargo test tmux::terminal_session:: -- --test-threads=1
cargo test tmux::tool_session:: -- --test-threads=1
```

### P4: Refactor multi-session flows into explicit verified loops

#### End State

Any operation that affects more than one tmux session does so by looping through explicit session records or explicit expected ancillary session names, then calling the audited single-session helper once per target. Prefix-only or suffix-only tmux enumeration is gone from destructive production paths.

#### Tests first

Add tests for group archive, per-instance ancillary cleanup, tool cleanup, restart all, and idle auto-stop that prove each target is handled independently and unverified sessions are skipped. Add candidate-list tests that inject similar names and stale records to prove scope cannot widen before the audited kill helper runs.

BDD coverage: S3, S4, S5, S9.

#### Work

- Replace `kill_all_tmux_sessions()` internals with a loop over exact expected session kinds for that instance.
- Replace `kill_ancillary_tmux_sessions()` internals with exact expected terminal/container/tool targets.
- Replace `kill_all_tool_sessions_for_id()` suffix-based destructive behavior. Prefer known configured tool sessions plus marker-verified discovery only if non-destructive enumeration is still needed.
- Audit all session detection call sites: tmux list parsing, generated name helpers, storage lookups, group expansion, idle reap candidate selection, server API delete/archive, TUI archive/delete, and CLI restart-all.
- For each call site, document the scope rule in code or tests: exact stored instance, exact generated ancillary name, or marker-verified discovery.
- Ensure archive group loops over stored `Instance` values and never asks tmux for all prefixed names.
- Ensure `restart --all` remains a storage-driven loop and uses audited per-session kill through `kill_clean()`.
- Ensure idle auto-stop remains candidate-driven from stored sessions and audited through `Instance::stop()`.
- Make skipped unverified sessions visible through audit records and relevant warnings.

#### Expected files

- `src/session/instance.rs`
- `src/tmux/tool_session.rs`
- `src/session/deletion.rs`
- `src/tui/home/operations.rs`
- `src/server/api/sessions.rs`
- `src/cli/session.rs`
- `src/server/mod.rs`
- Tests covering each touched flow

#### Decision dependencies

None.

#### Verify

```bash
cargo test session::instance:: -- --test-threads=1
cargo test session::idle_reap:: -- --test-threads=1
cargo test cli::session:: -- --test-threads=1
cargo test --test e2e archive -- --nocapture
```

### P5: Update command docs and regression coverage

#### End State

User-facing docs, generated CLI reference, and regression tests all encode the new product rule: AoE does not provide a global tmux-session nuke, and all tmux kills are single-session, verified, and audited.

#### Tests first

Add or update CLI help snapshot tests and docs consistency checks before regenerating docs. Add a repository search test or lightweight static check that fails on production `tmux kill-session` calls outside the audited helper, on production calls to `tmux kill-server`, and on destructive candidate builders that rely on prefix-only or suffix-only matching.

BDD coverage: S1 through S9.

#### Work

- Regenerate `docs/cli/reference.md` after clap help changes.
- Update any prose that says `killall` kills tmux sessions.
- Add static regression coverage preventing `stop_all_sessions()` from containing tmux enumeration, process signaling, or `tmux kill-session`.
- Add static regression coverage preventing direct `tmux kill-session` outside the audited helper.
- Add static regression coverage for dangerous session detection patterns, including prefix-only, suffix-only, substring, and unscoped tmux-server-wide matching in destructive paths.
- Add final tests for audit file contents across one-shot CLI and long-lived contexts.

#### Expected files

- `docs/cli/reference.md`
- `docs/development/logging.md` if audit file discoverability should be documented there
- `tests/e2e/cli.rs`
- `tests/integration/*` as appropriate for static checks
- Any touched Rust test modules from prior phases

#### Decision dependencies

None.

#### Verify

```bash
cargo xtask gen-docs
cargo test --test e2e killall_preserves_aoe_tmux_sessions -- --nocapture
cargo test tmux_kill_audit -- --test-threads=1
cargo test
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```

## Verification strategy

Run focused tests after each phase, then run full Rust quality gates before review. Because the work is Rust-only unless docs are regenerated, no web format, lint, or Playwright suite is required unless implementation touches `web/` or dashboard user flows.

Static verification should prove the negative behavior: no production code path can globally enumerate and kill all AoE tmux sessions, no direct `tmux kill-session` call bypasses audit, and no destructive candidate builder depends on prefix-only, suffix-only, substring, or unscoped tmux-server-wide matching.

Runtime verification should prove positive behavior: single-session stop still works, multi-session workflows loop explicitly, candidate detection stays exact and scoped, unverified targets are skipped, and audit records contain enough detail to reconstruct what happened.

## Test coverage matrix

| Acceptance | Scenarios | Test layer | Planned checks |
| --- | --- | --- | --- |
| AC1 | S1 | E2E or integration with isolated tmux | `killall_preserves_aoe_tmux_sessions` |
| AC2 | S1, S4 | Unit plus static regression | no destructive implementation inside `stop_all_sessions`, no prefix-only destructive helper |
| AC3 | S2, S7 | Unit | audit helper emits attempt/result/skipped records |
| AC4 | S6 | Integration | one-shot CLI writes audit file without logging env |
| AC5 | S3, S4, S5 | Unit plus integration | group archive, restart all, idle stop use verified per-session calls |
| AC6 | S3, S5 | Unit with tmux when available | every session kind gets ownership markers |
| AC7 | S1 | Docs and CLI help tests | killall help and generated docs no longer mention tmux session teardown |
| AC8 | S8 | TUI or presenter unit test | skipped global sweep notice is visible in the TUI |
| AC9 | S9 | Unit plus static regression | candidate builders exclude similar names, suffix collisions, stale records, and wrong-server sessions |

## Delivery order

Deliver P1 first to make the most dangerous behavior non-destructive, audited, and visible. P2 and P3 then establish the safety foundation for all remaining session kills. P4 migrates higher-level workflows onto the foundation. P5 locks docs and regression coverage after behavior is stable.

## Non-goals

- Do not add a replacement global tmux nuke command.
- Do not preserve prefix-only kill behavior for legacy sessions.
- Do not change ACP worker process-group cleanup except where `killall` user-facing text changes.
- Do not redesign session storage or add a migration unless implementation proves markers cannot be written lazily on creation.
- Do not add dashboard UI for audit browsing in this plan.

## Decisions / Deviations log

- 2026-06-22: Product decision locked by user, remove global AoE tmux kill functionality entirely.
- 2026-06-22: Plan uses a new `thoughts/plans/` artifact because the repo had no existing active plan path. Root planning guidance and product intent bootstrap remain separate follow-up work.
- 2026-06-22: Default `cargo test` exposed that `tests/acp_runner_orphan.rs` requires the `serve` feature because it invokes hidden `__acp-runner`. Added a Cargo `required-features = ["serve"]` test gate and verified the test separately with `cargo test --features serve --test acp_runner_orphan`.
- 2026-06-22: Full e2e verification exposed stale profile-picker delete navigation and a redraw race in the delete-flow test. Updated only the test navigation and its post-delete wait so the required `cargo test` gate can exercise the intended profile delete/cancel flows instead of asserting during a transient blank redraw.
- 2026-06-22: GLM scoped review found three in-scope fail-open gaps after the first GPT pass: pre-kill audit write failure still allowed destruction, cached tmux ownership markers could certify a newly recreated unmarked session with the same deterministic name, and marker-write failures could leave a successful create without observable ownership markers. Fixed by requiring the attempt audit write before any kill, reading ownership markers fresh for destructive verification, returning marker batch-write failures from fallback, and failing agent/terminal/tool create/finalize paths when ownership stamping fails.
- 2026-06-24: Final scoped reviewer gates passed after the GLM fixes (`quality-reviewer` and GLM model override both returned `VERDICT: PASS_SCOPED`). Final full verification passed: `cargo test && cargo fmt --check && cargo clippy --all-targets --all-features -- -D warnings`.
