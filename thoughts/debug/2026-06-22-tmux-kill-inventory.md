---
date: 2026-06-22
issue: aoe may have killed tmux server or sessions
status: broad sweep complete
---

# Debug: tmux and process kill inventory

## Symptom

The reported symptom is that AoE may have killed the tmux server or all tmux sessions on the host.

## Scope inspected

Searched production Rust code, tests, web live harness, scripts, docs, repo artifacts, and development helpers for tmux teardown, process tree teardown, process group signals, daemon stop, ACP worker cleanup, hook timeout cleanup, and direct `Command::new("tmux")` use.

Search terms included `kill-server`, `kill-session`, `kill-window`, `kill-pane`, `killall`, `pkill`, `SIGTERM`, `SIGKILL`, `killpg`, `process.kill(-pid)`, `kill_process_tree`, `tmux_command()`, `Command::new("tmux")`, `TMUX`, `TMUX_PANE`, and `TMUX_TMPDIR`.

## Inventory

### Deliberate broad AoE panic path

- `src/cli/killall.rs:50` calls `crate::cli::acp::stop_all_workers(args.timeout_secs).await`.
- `src/cli/killall.rs:55` calls `crate::tmux::stop_all_sessions()`.
- `src/tmux/mod.rs:159` lists sessions on AoE's named tmux server and kills every session where `is_aoe_session(name)` is true.
- `src/tmux/mod.rs:141` defines ownership as `name.starts_with(SESSION_PREFIX)`.
- `src/tmux/mod.rs:38`, `:43`, `:48`, and `:53` define the agent, terminal, container terminal, and tool prefixes.

Impact: `aoe killall` intentionally stops the serve daemon, every structured-view worker in the worker registry, and every AoE-prefixed tmux session in the AoE tmux server.

Risk: this should not kill the user's default tmux server because `tmux_command()` uses `-L aoe` or `-L aoe_dev`, removes inherited `TMUX` and `TMUX_PANE`, and usually removes `TMUX_TMPDIR`. The remaining risk is ownership by prefix only: any foreign session inside the AoE tmux server whose name begins with `aoe_` in release or `aoe_dev_` in debug is treated as AoE-owned.

### Per-instance tmux fan-out cleanup

- `src/session/instance.rs:3195` `kill_all_tmux_sessions()` kills one instance's agent tmux session and ancillary sessions.
- `src/session/instance.rs:3213` `kill_ancillary_tmux_sessions()` kills host terminal, container terminal, and every tool session for that instance ID.
- `src/tmux/tool_session.rs:157` `kill_all_tool_sessions_for_id()` lists all tmux sessions and kills any tool session whose name starts with `TOOL_PREFIX` and ends with the truncated instance ID.

Callers:

- `src/session/deletion.rs:69`, delete one session.
- `src/cli/session.rs:303` and `:305`, archive one session unless `no_kill` is set.
- `src/server/api/sessions.rs:1968`, archive one plain session from web, with `kill_pane` defaulting true.
- `src/server/api/sessions.rs:1956`, archive one structured session's ancillary tmux sessions.
- `src/tui/home/operations.rs:696`, force remove one session.
- `src/tui/home/operations.rs:1320`, archive one session.
- `src/tui/home/operations.rs:1438`, archive selected group, loops over all active sessions in the group.
- `src/server/api/acp.rs:1516`, switching from tmux to structured view kills the agent and ancillary tmux sessions.

Impact: one session operation can kill up to four tmux session kinds for that instance, and group archive can fan out across all sessions in a group.

Risk: tool cleanup uses prefix plus truncated ID suffix. UUID collision is unlikely, but this is still name-derived ownership rather than a registry or tmux hidden option check.

### Per-session tmux kill helper

- `src/tmux/session.rs:273` `Session::kill()` kills pane process tree, then `kill-session -t <name>`.
- `src/tmux/terminal_session.rs:128` does the same for paired terminals.
- `src/tmux/tool_session.rs:114` does the same for tool sessions.
- `src/tmux/utils.rs:319` centralizes `tmux kill-session -t <name>` and treats missing sessions as success.

Impact: these target exact session names, not wildcard patterns.

Risk: if a user-created session with the exact generated AoE name exists in the AoE tmux server, AoE treats it as the intended target.

### Restart paths that kill before recreating

- `src/session/instance.rs:2779` `kill_clean()` uses `Session::kill()` and sleeps 100 ms before restart.
- `src/cli/session.rs:516` restarts all eligible sessions in a profile when `aoe session restart --all` is used.
- `src/server/api/sessions.rs:3902` web `ensure_session` can restart a missing, dead, or shell-fallen plain tmux session.
- `src/server/api/sessions.rs:2283` web start of a stopped plain session also kills any corpse pane first.

Impact: `restart --all` can kill and recreate every eligible tmux-backed session in a profile.

Risk: if restart selection is broader than intended, many AoE sessions can be killed in rapid succession. This is restart behavior, not tmux server destruction.

### Idle auto-stop

- `src/tui/app.rs:2338` TUI auto-stops idle plain tmux sessions every 60 seconds when configured.
- `src/server/mod.rs:2860` serve daemon does the same for plain tmux sessions.
- `src/session/idle_reap.rs:39` selects candidates.
- `src/session/config.rs:887` documents `session.auto_stop_idle_secs`, default `0` at `src/session/config.rs:470`.

Impact: if configured nonzero, all idle unattached plain tmux sessions past the threshold can be stopped over one or more reap passes.

Risk: an aggressive setting can look like AoE killed many sessions. Attached sessions are spared, non-idle sessions are spared, and default is disabled.

### tmux server kill sites

No production Rust path found that calls `tmux kill-server`.

Non-production and artifact sites:

- `web/tests/helpers/aoeServe.ts:742` kills `tmux -L <aoe|aoe_dev> kill-server` for isolated live test cleanup.
- `web/tests/live/tmux-env-isolation.spec.ts:61` kills a test-created sentinel server with `tmux -S <socket> kill-server`.
- `src/tmux/utils.rs:642`, `:710`, `:721`, `:757`, and `:760` are tests around isolated tmux servers.
- `assets/demo.tape:22` runs unscoped `tmux kill-server 2>/dev/null; clear`.
- `docs/development.md:172` mentions `HOME=$SANDBOX/home tmux kill-server` in a demo reset recipe.
- `docs/bug-reports/2026-06-19-daemon-inherits-tmux-server.md:163` includes `tmux kill-server` in a reproduction sketch.

Impact: `assets/demo.tape` is the only repo artifact found that would kill the caller's default tmux server if run as-is.

### tmux environment isolation

- `src/tmux/utils.rs:13` centralizes AoE-owned tmux commands.
- `src/tmux/utils.rs:15` uses `tmux -L aoe` or `tmux -L aoe_dev`.
- `src/tmux/utils.rs:16` and `:17` remove inherited `TMUX` and `TMUX_PANE`.
- `src/tmux/utils.rs:18` only forwards `TMUX_TMPDIR` through explicit `AOE_TMUX_TMPDIR`, otherwise it removes `TMUX_TMPDIR`.
- `src/cli/serve.rs:816` to `:818` removes `TMUX`, `TMUX_PANE`, and `TMUX_TMPDIR` before spawning the daemon child.
- `src/tmux/utils.rs:29` `current_tmux_client_command()` intentionally uses the caller's tmux context for current-session status reads only.
- `src/tmux/utils.rs:46` `default_tmux_command()` intentionally probes the legacy default server to refuse duplicate pre-upgrade session names.

Impact: production lifecycle operations now target the AoE named tmux server, not the caller's tmux server.

Risk: the isolation is recent enough that old installed binaries or stale daemons from before the fix remain plausible explanations. The historical failure mode is documented in `docs/bug-reports/2026-06-19-daemon-inherits-tmux-server.md`.

### Pane process tree kill hazard

- `src/process/mod.rs:20` gets tmux `#{pane_pid}` and parses it to `u32`.
- `src/process/mod.rs:108` exposes `kill_process_tree(pid)`.
- `src/process/mod.rs:122` `kill_with_fallback()` sends SIGTERM, waits 100 ms, then SIGKILLs survivors.
- `src/tmux/session.rs:282`, `src/tmux/terminal_session.rs:135`, `src/tmux/tool_session.rs:120`, `src/tmux/tool_session.rs:170`, and `src/tmux/mod.rs:171` pass pane PIDs into `kill_process_tree`.

Impact: normally kills only the pane shell process and descendants.

Risk: there is no explicit guard against PID `0` or PID `1`. POSIX `kill(0, SIGTERM)` signals the caller's process group. I did not find evidence tmux emits `pane_pid=0`, but the defensive guard is absent. This is the highest-value small hardening change.

### ACP worker process group kill paths

- `src/acp/worker_registry.rs:457` `signal_runner_group(pid, sig)` sends `killpg(pid, sig)` and then `kill(pid, sig)`.
- `src/acp/worker_registry.rs:466` `terminate_runner_group(pid)` sends SIGTERM.
- `src/acp/worker_registry.rs:475` `kill_runner_group(pid)` sends SIGKILL.
- `src/acp/worker_registry.rs:504` `reap_group_escalating(pid, grace)` sends SIGTERM, waits, then SIGKILL.
- `src/cli/acp.rs:562` `aoe acp kill` SIGKILLs a registered worker group.
- `src/cli/acp.rs:582` `aoe acp stop` and `aoe acp stop --all` SIGTERM registered worker groups and escalate.
- `src/cli/acp.rs:655` `aoe acp restart` SIGTERMs the worker group before respawn.
- `src/server/acp_reconciler.rs:958` respawns idle build-stale workers by terminating the old group.
- `src/server/acp_reconciler.rs:1367` sweeps orphan workers and calls `reap_group_escalating`.
- `src/acp/supervisor.rs:1604` and `:1625` SIGTERM and then SIGKILL an unresponsive runner group.
- `src/acp/supervisor.rs:2215` supervisor shutdown-all SIGTERMs every registry PID it knows.
- `src/acp/supervisor.rs:2601` `terminate_runner_for_session()` terminates one session's registered runner.

Impact: these can kill every structured-view worker process group known in the registry. They do not target tmux, but they can kill many user-visible agent processes.

Risk: the registry stores raw PIDs in JSON. `src/acp/worker_registry.rs:419` `is_record_live()` checks runner schema version, `kill(pid, 0)`, and socket existence, but group-kill paths still generally trust the recorded PID when issuing signals. There is no central `pid > 1` guard in `signal_runner_group()`. There is also no final check that the PID is currently an `aoe __acp-runner` owned by the current user before `killpg` plus fallback `kill(pid)`. If a stale registry PID is reused by an unrelated process, AoE could signal that unrelated PID, and if its PGID equals the stale PID, its process group too. This is unlikely but is the broadest non-tmux defensive gap.

### Test harness process group kill

- `web/tests/helpers/aoeServe.ts:311` uses `process.kill(-pid, "SIGKILL")` for orphan ACP runners in isolated live tests.
- The helper guards `typeof pid !== "number" || pid <= 1` before signaling.
- The harness runs under isolated `HOME`, `XDG_CONFIG_HOME`, `TMPDIR`, `TMUX_TMPDIR`, and `AOE_TMUX_TMPDIR`.

Impact: test-only, isolated.

Risk: the TypeScript harness has the PID lower-bound guard the Rust worker registry lacks. That is a useful implementation precedent.

### Hook timeout process cleanup

- `src/session/repo_config.rs:970` `run_hook_with_timeout()` spawns hook commands and, on timeout or wait error, calls `crate::process::kill_process_tree(pid)`.
- `src/session/repo_config.rs:849` uses `setsid()` for local hooks when `detach_tty` is enabled.

Impact: hook timeout cleanup can kill a hook process and descendants. Hooks are user-configured commands and can themselves run arbitrary destructive commands, including `tmux kill-server`.

Risk: the direct cleanup PID comes from `Child::id()`, so PID zero is not realistic here. The broader risk is user hook content, not AoE intrinsic behavior.

### Daemon stop and xtask dev process group cleanup

- `src/cli/serve.rs:489` `verify_pid_is_aoe(pid)` checks a PID file target contains `aoe` or `agent-of-empires` before daemon stop signals it.
- `src/cli/serve.rs:1005` refuses to stop a daemon if PID verification fails.
- `src/cli/serve.rs:1016` sends SIGTERM to the daemon PID and escalates to SIGKILL after 2 seconds.
- `xtask/src/main.rs:91` `terminate_group()` kills child process groups for `cargo xtask dev` children.
- `xtask/src/main.rs:193` and `:239` spawn those dev children with `.process_group(0)`.

Impact: daemon stop targets one AoE-looking daemon PID. xtask dev targets only child process groups it created.

Risk: daemon PID verification is substring-based, so it is safer than a raw PID file but not perfect. It is not a tmux-server kill path. xtask is development-only and uses dedicated child process groups.

## Coverage gaps

- No direct integration test that `aoe killall` preserves a default-server sentinel tmux session.
- No direct test that `stop_all_sessions()` preserves a foreign session inside the AoE server whose name starts with a non-AoE prefix, and no explicit product decision test for foreign names that begin with `aoe_`.
- No defensive test for `kill_process_tree(0)`, `kill_process_tree(1)`, or `get_pane_pid()` rejecting unsafe PIDs.
- No defensive tests for `worker_registry::terminate_runner_group(0)`, `kill_runner_group(0)`, or stale PID reuse protection.
- No check preventing unscoped `tmux kill-server` in executable repo artifacts like `assets/demo.tape`.

## Recommendation

Recommended hardening order:

1. Add central unsafe-PID guards for all signal helpers.
   - `kill_process_tree(pid)` should no-op and warn for `pid <= 1`.
   - `get_pane_pid()` should reject parsed values `<= 1`.
   - `worker_registry::signal_runner_group(pid, sig)` should reject `pid <= 1` before `killpg` or `kill`.

2. Verify worker registry PID ownership before process-group signaling.
   - Add a helper that checks the PID is owned by the current user and its command line looks like AoE's `__acp-runner` for the expected session.
   - Use that helper before `terminate_runner_group`, `kill_runner_group`, and `reap_group_escalating`.
   - If verification fails, delete the stale registry record and skip signaling rather than risking PID reuse damage.

3. Strengthen tmux ownership beyond name prefixes.
   - Set hidden tmux env or user options such as `AOE_INSTANCE_ID` and a session kind on every AoE-created tmux session, including primary, terminal, container terminal, and tool sessions.
   - Make broad sweeps such as `stop_all_sessions()` require this marker where possible.
   - Keep a deliberate fallback for legacy running sessions only if product wants older sessions to remain killable by `killall`.

4. Add default-server preservation tests.
   - Create a sentinel session in an isolated default tmux server and an AoE session in the AoE named server, run the public broad-kill path, and assert the sentinel survives.
   - Add coverage for inherited `TMUX`, `TMUX_PANE`, and `TMUX_TMPDIR` around the public `aoe killall` path, not only the helper.

5. Remove or scope unscoped artifact-level `tmux kill-server`.
   - Change `assets/demo.tape:22` to use an isolated `TMUX_TMPDIR` plus explicit `tmux -L ... kill-server`, or delete the line.
   - Add a lightweight repo check that rejects unscoped `tmux kill-server` outside docs explicitly marked as reproductions.

6. Improve observability for destructive paths.
   - Log target counts and target names for `aoe killall`, group archive, restart-all, idle reap, ACP worker stop-all, and orphan worker sweep.
   - Include the tmux server name, effective `AOE_TMUX_TMPDIR`, and whether ownership was marker-verified.

## Current hypothesis

The current codebase has no production `tmux kill-server` path. The most plausible broad-kill explanations are:

1. `aoe killall`, intentionally broad for AoE-owned tmux sessions and ACP workers.
2. Group archive, restart-all, or idle auto-stop, each capable of stopping many AoE sessions by design.
3. ACP worker registry cleanup, which can stop many structured-view runner groups and has stale-PID defensive gaps.
4. Running `assets/demo.tape`, which contains unscoped `tmux kill-server`.
5. A stale installed binary or daemon from before the tmux server isolation fix documented in `docs/bug-reports/2026-06-19-daemon-inherits-tmux-server.md`.

## Recommended incident checks

- Inspect the installed binary version and whether a daemon from an older build was still running.
- Inspect recent `debug.log` around the incident for `killall`, `session.tmux_cleanup`, `session.stop`, `server.idle_reap`, `tui.idle_reap`, `archive`, `perform_deletion`, `acp.supervisor`, and `serve.shutdown`.
- Check the active config for `session.auto_stop_idle_secs`.
- Check shell history or automation logs for `aoe killall`, `aoe acp stop --all`, `aoe session restart --all`, `vhs assets/demo.tape`, and raw `tmux kill-server`.
