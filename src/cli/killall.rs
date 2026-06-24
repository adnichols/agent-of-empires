//! `aoe killall`: a panic button that stops AoE infrastructure in one command.
//! It tears down the serve daemon and every ACP cockpit worker. Legacy broad
//! tmux teardown is disabled and audited so user workspaces are preserved. Each
//! surface is attempted independently; one failing surface never aborts the
//! others, and the exit code is non-zero only if something failed.

use anyhow::Result;
use clap::Args;

#[derive(Args, Debug)]
pub struct KillallArgs {
    /// Grace period in seconds before force-killing agent workers. The daemon
    /// uses its own built-in grace, and tmux sessions are preserved.
    #[cfg(feature = "serve")]
    #[arg(long, default_value_t = 5)]
    pub timeout_secs: u64,

    /// Leave the `aoe serve` daemon running; stop only workers.
    #[cfg(feature = "serve")]
    #[arg(long)]
    pub keep_daemon: bool,
}

pub async fn run(args: KillallArgs) -> Result<()> {
    // Every surface is best-effort: each is attempted independently and its
    // failure is collected here rather than aborting the rest. In a TUI-only
    // build only the tmux sweep runs, so `args` carries no fields.
    #[cfg(not(feature = "serve"))]
    let _ = args;

    let mut errors: Vec<String> = Vec::new();

    // Daemon first. Removing the orchestrator means the worker sweep below
    // cannot race a daemon-driven respawn; any orphaned workers still die via
    // their recorded process group in that sweep.
    #[cfg(feature = "serve")]
    if !args.keep_daemon {
        if crate::cli::serve::daemon_pid().is_some() {
            match crate::cli::serve::stop_daemon().await {
                Ok(()) => println!("Stopped aoe serve daemon."),
                Err(e) => errors.push(format!("daemon: {e}")),
            }
        } else {
            println!("No aoe serve daemon running.");
        }
    }

    #[cfg(feature = "serve")]
    match crate::cli::acp::stop_all_workers(args.timeout_secs).await {
        Ok(n) => println!("Stopped {n} agent worker(s)."),
        Err(e) => errors.push(format!("workers: {e}")),
    }

    match crate::tmux::stop_all_sessions() {
        Ok(report) => {
            println!("Tmux sessions preserved; broad tmux teardown is disabled.");
            if let Some(path) = report.audit_path {
                println!("Recorded skipped tmux sweep in {}.", path.display());
            }
            if let Some(error) = report.audit_error {
                eprintln!("warning: failed to write skipped tmux sweep audit: {error}");
            }
        }
        Err(e) => errors.push(format!("tmux: {e}")),
    }

    if !errors.is_empty() {
        for e in &errors {
            eprintln!("killall error: {e}");
        }
        anyhow::bail!("killall completed with {} error(s)", errors.len());
    }

    Ok(())
}

/// Hidden trap for `aoe stop [...]`. Users conditioned by `docker stop` /
/// `systemctl stop` reach for `aoe stop`, but stopping in aoe is always scoped
/// to a noun. Rather than clap's bare "unrecognized subcommand" error, point
/// them at the right verb and exit non-zero. Never triggers a teardown itself.
pub fn stop_trap() -> Result<()> {
    anyhow::bail!(
        "`aoe stop` is not a command. Did you mean:\n  \
         aoe session stop <id>   stop one session\n  \
         aoe acp stop [--all]    stop agent workers\n  \
         aoe serve --stop        stop the web daemon\n  \
         aoe killall             force-stop AoE daemon and workers"
    )
}
