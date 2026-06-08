//! Process utilities for tmux session management

use std::process::Command;
use std::time::Duration;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use nix::errno::Errno;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use nix::sys::signal::{kill, Signal};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use nix::unistd::Pid;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "macos")]
mod macos;

/// Get the PID of the shell process running in a tmux pane
pub fn get_pane_pid(session_name: &str) -> Option<u32> {
    // Use `^.0` to target the first window's first pane regardless of
    // base-index or which pane is active, so we always query the agent's
    // pane even when the user has created additional tmux windows or split
    // panes.  See #435, #488.
    let target = format!("{session_name}:^.0");
    let output = Command::new("tmux")
        .args(["display-message", "-t", &target, "-p", "#{pane_pid}"])
        .output()
        .ok()?;

    if !output.status.success() {
        // Guarded: hot poll path. Only formats arguments when the user has
        // enabled `process.ppid=trace` (or finer) on their filter.
        if tracing::enabled!(target: "process.ppid", tracing::Level::TRACE) {
            tracing::trace!(
                target: "process.ppid",
                session = %session_name,
                status = ?output.status,
                "display-message failed; no pane pid",
            );
        }
        return None;
    }

    let pid = String::from_utf8_lossy(&output.stdout).trim().parse().ok();
    if tracing::enabled!(target: "process.ppid", tracing::Level::TRACE) {
        tracing::trace!(
            target: "process.ppid",
            session = %session_name,
            pid = ?pid,
            "resolved pane pid",
        );
    }
    pid
}

/// Get the foreground process group leader PID for a given shell PID
/// This finds the actual process that has the terminal foreground
pub fn get_foreground_pid(shell_pid: u32) -> Option<u32> {
    let pid = {
        #[cfg(target_os = "linux")]
        {
            linux::get_foreground_pid(shell_pid)
        }

        #[cfg(target_os = "macos")]
        {
            macos::get_foreground_pid(shell_pid)
        }

        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = shell_pid;
            None
        }
    };
    if tracing::enabled!(target: "process.ppid", tracing::Level::TRACE) {
        tracing::trace!(
            target: "process.ppid",
            shell_pid,
            foreground_pid = ?pid,
            "resolved foreground pid",
        );
    }
    pid
}

/// Return a PID and every descendant process that can be discovered locally.
pub fn collect_pid_tree(pid: u32) -> Vec<u32> {
    #[cfg(target_os = "linux")]
    {
        linux::collect_pid_tree(pid)
    }

    #[cfg(target_os = "macos")]
    {
        macos::collect_pid_tree(pid)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        vec![pid]
    }
}

/// Kill a process and all its descendants
/// Sends SIGTERM first, then SIGKILL to any survivors
pub fn kill_process_tree(pid: u32) {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    kill_with_fallback(&collect_pid_tree(pid));

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        // No-op on unsupported platforms, fall back to tmux kill-session only
    }
}

/// SIGTERM every pid in reverse order (children first), wait briefly for
/// graceful shutdown, then SIGKILL anything still alive.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn kill_with_fallback(pids: &[u32]) {
    tracing::debug!(
        target: "process.tree",
        descendants = ?pids,
        "killing process tree"
    );

    for &p in pids.iter().rev() {
        tracing::debug!(target: "process.signal", pid = p, signal = "SIGTERM", "sending signal");
        let _ = kill(Pid::from_raw(p as i32), Signal::SIGTERM);
    }

    std::thread::sleep(Duration::from_millis(100));

    for &p in pids.iter().rev() {
        if process_exists(p) {
            tracing::warn!(
                target: "process.reap",
                pid = p,
                "pid survived SIGTERM after 100ms; sending SIGKILL"
            );
            tracing::info!(target: "process.signal", pid = p, signal = "SIGKILL", "sending signal");
            let _ = kill(Pid::from_raw(p as i32), Signal::SIGKILL);
        }
    }
}

/// Portable "is this pid still around?" check via kill(pid, 0).
/// EPERM means the process exists but we lack permission (still exists).
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn process_exists(pid: u32) -> bool {
    match kill(Pid::from_raw(pid as i32), None) {
        Ok(()) => true,
        Err(Errno::EPERM) => true,
        Err(_) => false,
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn process_exists(_pid: u32) -> bool {
    false
}

#[cfg(unix)]
pub fn process_owner_is_current_user(pid: u32) -> bool {
    let output = Command::new("ps")
        .args(["-o", "uid=", "-p", &pid.to_string()])
        .output();
    let Ok(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let Ok(uid) = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u32>()
    else {
        return false;
    };
    let current_uid = Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|output| {
            output
                .status
                .success()
                .then(|| {
                    String::from_utf8_lossy(&output.stdout)
                        .trim()
                        .parse::<u32>()
                        .ok()
                })
                .flatten()
        });
    Some(uid) == current_uid
}

#[cfg(not(unix))]
pub fn process_owner_is_current_user(_pid: u32) -> bool {
    true
}

/// Send SIGSTOP to a process and all its descendants. Used to pause
/// the agent (claude) while a mobile client is reading tmux scrollback
/// — without this, claude's continued output keeps pushing lines into
/// scrollback under the reader and shifts what they're trying to read.
///
/// Paired with [`continue_process_tree`] which sends SIGCONT. The web
/// server guarantees a SIGCONT on client disconnect so a dropped
/// connection cannot leave the pane's process permanently suspended.
pub fn stop_process_tree(pid: u32) {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    signal_process_tree(pid, Signal::SIGSTOP);

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
    }
}

/// Send SIGCONT to a process and all its descendants. Inverse of
/// [`stop_process_tree`]; SIGCONT to a non-stopped process is a no-op,
/// so this is safe to invoke unconditionally as cleanup.
pub fn continue_process_tree(pid: u32) {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    signal_process_tree(pid, Signal::SIGCONT);

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn signal_process_tree(pid: u32, signal: Signal) {
    #[cfg(target_os = "linux")]
    let pids = linux::collect_pid_tree(pid);
    #[cfg(target_os = "macos")]
    let pids = macos::collect_pid_tree(pid);

    tracing::debug!(
        target: "process.tree",
        descendants = ?pids,
        ?signal,
        "signaling process tree"
    );
    for &p in pids.iter().rev() {
        if let Err(e) = kill(Pid::from_raw(p as i32), signal) {
            if e != Errno::ESRCH {
                tracing::debug!(
                    target: "process.signal",
                    pid = p,
                    ?signal,
                    error = %e,
                    "kill failed"
                );
            }
        }
    }
}
