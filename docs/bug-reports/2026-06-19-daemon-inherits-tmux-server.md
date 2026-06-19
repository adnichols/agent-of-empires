# Bug report: `aoe serve --daemon` inherits caller `TMUX` and loses/targets the wrong tmux server

Date: 2026-06-19

## Summary

`aoe serve --daemon` can inherit the caller's `TMUX` environment variable when launched from inside a tmux client. The daemon then runs plain `tmux ...` commands with that inherited `TMUX`, so all daemon tmux probes, lifecycle operations, status checks, and recovery logic are coupled to the caller's tmux server/socket instead of an AOE-owned or environment-independent tmux server.

When the caller/default tmux server is killed, exits, or is otherwise unavailable, AOE starts reporting all sessions as missing/error even if the user expected those AOE sessions to be independent of the original terminal/tmux context. This can make all AOE sessions appear killed at once.

## User-visible impact

- AOE sessions unexpectedly move to `error` / `tmux session is gone`-style states.
- `aoe serve` continuously logs `tmux.cache` and `tmux.pane` warnings.
- AOE daemon behavior depends on the tmux server from which it was launched.
- Killing or losing an unrelated/default tmux server can take out AOE-managed tmux state.
- Restart/recovery logic may skip or misclassify sessions because tmux probes fail globally.

## Observed evidence on affected machine

### Running AOE daemon inherited `TMUX`

A production daemon process was running as:

```text
PID 34032
/opt/homebrew/bin/aoe serve --daemon-child --port 63827 --host 0.0.0.0 --auth token
```

Its environment contained:

```text
TMUX=/private/tmp/tmux-501/default,49586,91
HOME=/Users/anichols
```

This means the daemon was still bound to the tmux client/server context of the shell that launched `aoe serve --daemon`.

A local debug/web test daemon also showed the same class of problem, with both inherited `TMUX` and an isolated `TMUX_TMPDIR`:

```text
PID 35864
/Users/anichols/code/agent-of-empires-worktrees/fix-token-refresh/target/debug/aoe serve --host 127.0.0.1 --port 5200 --auth token

TMUX=/private/tmp/tmux-501/default,49586,161
HOME=/private/tmp/aoe-pw-w0-p0-ETU0P2
XDG_CONFIG_HOME=/private/tmp/aoe-pw-w0-p0-ETU0P2/config
TMUX_TMPDIR=/private/tmp/aoe-pw-w0-p0-ETU0P2/tmux
```

### AOE log showed repeated tmux global probe failures

`~/.agent-of-empires/debug.log` repeatedly logged:

```text
WARN tmux.cache: list-sessions returned non-zero; cache cleared status=ExitStatus(unix_wait_status(256)) stderr_bytes=51
WARN tmux.pane: list-panes returned non-zero status=ExitStatus(unix_wait_status(256)) stderr_bytes=51
WARN tmux.cache: list-sessions (attached) returned non-zero status=ExitStatus(unix_wait_status(256))
```

The 51-byte stderr was reproduced exactly by running `tmux list-sessions` with the inherited daemon `TMUX` value:

```bash
TMUX='/private/tmp/tmux-501/default,49586,91' \
  tmux list-sessions -F '#{session_name}'
```

Output:

```text
no server running on /private/tmp/tmux-501/default
```

### Persisted AOE session state after failure

`~/.agent-of-empires/profiles/main/sessions.json` contained many non-archived sessions marked `error`, plus a few `running` / `starting` rows. Example status summary at the time:

```text
count 18
5d2bb0de1bbc447e 'nod-679 Observability' error archived False agent_sid True
c647777ec72e4817 'redeploy' error archived False agent_sid True
2e340260977843a0 'deploy' error archived False agent_sid True
71a5e029081a4cea 'review-activity-0608' error archived False agent_sid True
a1d409fc93574b56 'claude-collab-space-attach' error archived False agent_sid True
99b4fe4a588f4a5a 'pi-collab-space-attach' error archived False agent_sid True
d1e1cb88ce4d4201 'pi-space-fix-design' error archived False agent_sid True
fc714d44ebe54c24 'separate collab and plan docs' error archived False agent_sid True
99d461f2080d4386 'nod-1028' starting archived False agent_sid True
09a11118b74d4790 'Huns' error archived False agent_sid True
f3c2b42bb37649b5 'byzantine' error archived False agent_sid True
ab799c93733745b5 'Stubbed incomplete inventory audit' running archived False agent_sid True
ab8cacb99104471b 'develop' error archived False agent_sid True
4e332ac7f1a843db 'v0.2.26' error archived False agent_sid True
253b4b08ae924320 'update-plan-review' error archived False agent_sid True
af9ab7db6570451f 'ccore' error archived False agent_sid True
f8230c2d9ca34bec 'ccore-keychain-access' running archived False agent_sid True
4a23757db20545df 'fix-token-refresh' running archived False agent_sid True
```

### No AOE archive/delete/kill event was found

Searches of the relevant AOE logs did not show matching `archive`, `delete`, `session.stop`, `perform_deletion`, or `tmux_kill` activity for the incident window. The dominant signal was global tmux probe failure.

## Root cause

AOE invoked `tmux` directly in many places with `Command::new("tmux")`, and the daemon launch path did not sanitize the inherited tmux environment. Clearing the daemon's inherited `TMUX` was necessary but not sufficient: AOE-managed sessions were still created in the user's default tmux server because AOE did not use an explicit tmux socket/name for its own sessions.

The dangerous invariant was therefore broader than daemon inheritance. Any AOE operation that created, probed, resized, captured, or killed sessions through plain `tmux` could couple AOE state to the user/default tmux server. If that server exited for any reason, including because AOE emptied its last managed sessions, all AOE sessions in that shared server were lost together.

Notable source locations observed:

- `src/cli/serve.rs`
  - `start_daemon()` spawns `serve --daemon-child` but does not call `cmd.env_remove("TMUX")`.
- `src/tmux/mod.rs`
  - `refresh_session_cache()` runs `tmux list-sessions ...`.
  - `batch_pane_metadata()` runs `tmux list-panes -a ...`.
  - `attached_session_names()` runs `tmux list-sessions ...`.
  - `stop_all_sessions()` runs `tmux list-sessions` and `tmux kill-session`.
- `src/tmux/session.rs`, `src/tmux/terminal_session.rs`, `src/tmux/tool_session.rs`, `src/tmux/env.rs`, `src/server/pane.rs`, and other call sites run `tmux` through `Command::new("tmux")`.

Plain `tmux` client behavior honors `TMUX` when it is set. Therefore a daemon that inherits `TMUX=/private/tmp/tmux-501/default,...` continues targeting that default server, even after daemonization via `setsid()`.

## Expected behavior

A daemonized AOE process should not remain coupled to the tmux client/server that happened to launch it.

Required invariants:

- `aoe serve --daemon` removes `TMUX` and `TMUX_PANE` before spawning `--daemon-child`.
- AOE-owned tmux operations do not inherit stale tmux client state.
- AOE-managed sessions are created, probed, captured, resized, and killed through an explicit AOE-owned tmux server name.
- Killing, losing, or restarting the user/default tmux server must not kill AOE-managed sessions.

## Actual behavior

`aoe serve --daemon-child` inherited `TMUX`, and later `tmux list-sessions` failed against the inherited socket:

```text
no server running on /private/tmp/tmux-501/default
```

AOE then treated the tmux world as unavailable/missing and marked sessions with error states.

## Reproduction sketch

1. Start a tmux server/client.
2. From inside that tmux client, launch AOE daemon:

   ```bash
   aoe serve --daemon
   ```

3. Confirm the daemon child inherited `TMUX`:

   ```bash
   pid=$(cat ~/.agent-of-empires/serve.pid)
   ps eww -p "$pid" | tr ' ' '\n' | grep '^TMUX='
   ```

4. Kill or stop the launching/default tmux server:

   ```bash
   tmux kill-server
   ```

5. Observe AOE log warnings:

   ```bash
   rg -n 'tmux.cache|tmux.pane|no server|list-sessions returned' ~/.agent-of-empires/debug.log | tail -50
   ```

6. Observe sessions moving to error/missing states.

## Implemented fix

### Immediate daemon fix

In `src/cli/serve.rs`, `build_daemon_command()` removes tmux client state before spawning the daemon child:

```rust
cmd.env_remove("TMUX");
cmd.env_remove("TMUX_PANE");
```

This happens before `cmd.spawn()` and applies to `serve --daemon-child`.

Do **not** remove `TMUX_TMPDIR` blindly without deciding the desired behavior. Test harnesses may intentionally set `TMUX_TMPDIR` to isolate tmux sockets. The important bug is stale `TMUX` pointing to a specific client/server tuple.

### Architectural fix

AOE now routes AOE-owned tmux operations through `tmux_command()`, which centralizes tmux server selection and environment handling:

```rust
pub(crate) fn tmux_command() -> std::process::Command {
    let mut cmd = std::process::Command::new("tmux");
    cmd.args(["-L", SERVER_NAME]);
    cmd.env_remove("TMUX");
    cmd.env_remove("TMUX_PANE");
    cmd
}
```

Production tmux paths that manage AOE state were migrated from `Command::new("tmux")` to this helper, including:

- daemon status/probe paths
- create/kill/rename/respawn/session lifecycle paths
- hidden environment get/set paths
- pane capture/readiness paths
- stop-all / idle reap / recovery paths

Plain `tmux` remains only where AOE must inspect the caller/current tmux context rather than an AOE-owned session, for example:

- user-facing current-session/status helper behavior
- `switch-client` and same-client `attach-session` attempts that need the current tmux client context
- tmux availability probes
- tests that intentionally create an isolated default tmux server as the negative control

To avoid duplicating live pre-upgrade sessions, create paths refuse to create an AOE-server session when a same-name AOE session still exists in the legacy default tmux server. Users should restart or stop those pre-upgrade panes once after upgrading.

## Regression tests

### Daemon launch environment

Coverage around daemon child command construction asserts:

- parent environment contains `TMUX=/tmp/some/stale/socket,...`
- spawned daemon command has no `TMUX`
- spawned daemon command has no `TMUX_PANE`

The implementation factors daemon child command construction into a testable helper.

### Tmux helper and owned-server isolation

`tmux_command()` coverage verifies the helper removes:

- `TMUX`
- `TMUX_PANE`

while preserving `TMUX_TMPDIR` unless the desired behavior changes.

An owned-server regression creates one session in an isolated default tmux server and one session in the AOE server, kills only the isolated default server, and asserts the AOE-managed session still exists.

A legacy-duplicate regression creates a same-name AOE session in an isolated default tmux server and asserts AOE refuses to create a duplicate on the AOE server.

### End-to-end/manual validation

Manual repro after fix:

```bash
# inside tmux
aoe serve --daemon
pid=$(cat ~/.agent-of-empires/serve.pid)
ps eww -p "$pid" | tr ' ' '\n' | grep '^TMUX=' && echo BUG || echo OK
```

Expected: `OK`.

Then restart or lose the launching/default tmux server and verify AOE-managed sessions still exist on the AOE tmux server and the daemon no longer logs repeated `no server running on /private/tmp/tmux-501/default` due to inherited `TMUX`.

## Risk notes

- Clearing `TMUX` in the daemon child is low risk and matches daemonization expectations.
- `switch-client` must preserve the current tmux client context; only AOE-owned lifecycle operations should clear `TMUX` and target the explicit AOE server.
- `TMUX_TMPDIR` may be used by test harnesses and sandboxed runs to isolate tmux sockets; preserve it unless adopting an explicit AOE socket strategy.
- Existing pre-upgrade AOE sessions in the default tmux server cannot be moved across servers. They should be restarted or stopped once after upgrade; AOE refuses same-name duplicate creation to avoid accidentally running two agents for one session.

## Classification

Severity: high for users who run AOE from tmux and expect daemon/session independence.

Likely affected surfaces:

- `aoe serve --daemon`
- daemon startup recovery
- status polling
- idle reaping
- live pane readiness
- session create/kill/rename/restart flows

Primary fix owner area: tmux integration / serve daemon launch.
