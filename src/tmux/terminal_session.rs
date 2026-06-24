//! Paired terminal sessions — host (`TerminalSession`) and sandbox (`ContainerTerminalSession`).
//!
//! The two session types have nearly identical lifecycles, so the
//! implementation lives in [`PairedTerminal`] and the public types are thin
//! wrappers that fix the tmux name prefix and the log-message label.

use super::utils::{
    append_clipboard_passthrough_args, append_mouse_on_args, append_pane_base_index_args,
    append_remain_on_exit_args, append_window_size_args, ensure_aoe_server_stays_alive,
    is_pane_dead, sanitize_session_name, tmux_command,
};
use super::{
    kill_tmux_session, refresh_session_cache, session_exists_from_cache, TmuxKillRequest,
    TmuxSessionKind, CONTAINER_TERMINAL_PREFIX, TERMINAL_PREFIX,
};
use crate::cli::truncate_id;
use crate::process;
use crate::session::config::should_apply_tmux_clipboard;
use anyhow::{bail, Result};

/// Classifies a paired terminal: adjusts the tmux session prefix and the
/// human-readable label used in error messages.
#[derive(Debug, Clone, Copy)]
enum TerminalKind {
    Host,
    Container,
}

impl TerminalKind {
    fn prefix(self) -> &'static str {
        match self {
            TerminalKind::Host => TERMINAL_PREFIX,
            TerminalKind::Container => CONTAINER_TERMINAL_PREFIX,
        }
    }

    fn label(self) -> &'static str {
        match self {
            TerminalKind::Host => "terminal session",
            TerminalKind::Container => "container terminal session",
        }
    }

    fn kill_kind(self) -> TmuxSessionKind {
        match self {
            TerminalKind::Host => TmuxSessionKind::Terminal,
            TerminalKind::Container => TmuxSessionKind::ContainerTerminal,
        }
    }
}

/// Shared implementation of the paired-terminal lifecycle. Not exposed; the
/// public [`TerminalSession`] and [`ContainerTerminalSession`] wrap one of
/// these with a fixed [`TerminalKind`].
struct PairedTerminal {
    name: String,
    instance_id: String,
    kind: TerminalKind,
}

impl PairedTerminal {
    fn generate_name(kind: TerminalKind, id: &str, title: &str) -> String {
        let safe_title = sanitize_session_name(title);
        format!("{}{}_{}", kind.prefix(), safe_title, truncate_id(id, 8))
    }

    fn new(kind: TerminalKind, id: &str, title: &str) -> Self {
        Self {
            name: Self::generate_name(kind, id, title),
            instance_id: id.to_string(),
            kind,
        }
    }

    fn exists(&self) -> bool {
        if let Some(exists) = session_exists_from_cache(&self.name) {
            return exists;
        }

        tmux_command()
            .args(["has-session", "-t", &self.name])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn is_pane_dead(&self) -> bool {
        is_pane_dead(&self.name)
    }

    fn create_with_size(
        &self,
        working_dir: &str,
        command: Option<&str>,
        size: Option<(u16, u16)>,
    ) -> Result<()> {
        ensure_aoe_server_stays_alive()?;

        if self.exists() {
            return Ok(());
        }

        let mut args = super::session::build_create_args(&self.name, working_dir, command, size);
        append_remain_on_exit_args(&mut args, &self.name);
        append_pane_base_index_args(&mut args, &self.name);
        append_mouse_on_args(&mut args, &self.name);
        append_window_size_args(&mut args, &self.name);
        if should_apply_tmux_clipboard() {
            append_clipboard_passthrough_args(&mut args, &self.name);
        }

        if super::utils::legacy_default_session_exists(&self.name) {
            bail!(
                "{} '{}' exists in the legacy default tmux server; restart or stop that pre-upgrade session before creating it on the AOE tmux server",
                self.kind.label(),
                self.name
            );
        }

        let output = tmux_command().args(&args).output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // "duplicate session" means a concurrent caller won the race;
            // the session exists now, which is what we wanted.
            if stderr.contains("duplicate session") {
                refresh_session_cache();
                return Ok(());
            }
            bail!("Failed to create {}: {}", self.kind.label(), stderr);
        }

        self.mark_ownership()?;
        refresh_session_cache();

        Ok(())
    }

    fn mark_ownership(&self) -> Result<()> {
        let kind = self.kind.kill_kind().as_str();
        let entries = [
            (
                self.name.as_str(),
                super::env::AOE_INSTANCE_ID_KEY,
                self.instance_id.as_str(),
            ),
            (self.name.as_str(), super::env::AOE_SESSION_KIND_KEY, kind),
        ];
        super::env::set_hidden_env_batch(&entries).map_err(|e| {
            tracing::warn!(target: "tmux.ownership", session = %self.name, error = %e, "failed to mark terminal tmux ownership");
            e
        })
    }

    fn kill(&self) -> Result<()> {
        let report = kill_tmux_session(TmuxKillRequest {
            session_name: &self.name,
            expected_instance_id: &self.instance_id,
            expected_kind: self.kind.kill_kind(),
            reason: "paired_terminal_stop",
            caller: "tmux::PairedTerminal::kill",
            allow_legacy_agent_kind: false,
        })?;
        if let Some(error) = report.audit_error {
            tracing::warn!(target: "tmux.audit", error = %error, session = %self.name, "tmux kill audit write failed");
        }
        Ok(())
    }

    fn get_pane_pid(&self) -> Option<u32> {
        process::get_pane_pid(&self.name)
    }

    fn attach(&self) -> Result<()> {
        if !self.exists() {
            bail!("{} does not exist: {}", self.kind.label(), self.name);
        }

        let status = tmux_command()
            .args(["attach-session", "-t", &self.name])
            .status()?;

        if !status.success() {
            bail!("Failed to attach to {}", self.kind.label());
        }

        Ok(())
    }

    fn capture_pane(&self, lines: usize) -> Result<String> {
        // Shared with the agent session / web live view paths: same
        // `^.0` targeting and trailing-blank preservation semantics.
        super::Session::from_name(&self.name).capture_pane(lines)
    }
}

pub struct TerminalSession {
    inner: PairedTerminal,
}

impl TerminalSession {
    pub fn new(id: &str, title: &str) -> Result<Self> {
        Ok(Self {
            inner: PairedTerminal::new(TerminalKind::Host, id, title),
        })
    }

    pub fn generate_name(id: &str, title: &str) -> String {
        PairedTerminal::generate_name(TerminalKind::Host, id, title)
    }

    pub fn exists(&self) -> bool {
        self.inner.exists()
    }

    pub fn is_pane_dead(&self) -> bool {
        self.inner.is_pane_dead()
    }

    pub fn create(&self, working_dir: &str) -> Result<()> {
        self.inner.create_with_size(working_dir, None, None)
    }

    pub fn create_with_size(
        &self,
        working_dir: &str,
        command: Option<&str>,
        size: Option<(u16, u16)>,
    ) -> Result<()> {
        self.inner.create_with_size(working_dir, command, size)
    }

    pub fn kill(&self) -> Result<()> {
        self.inner.kill()
    }

    pub fn get_pane_pid(&self) -> Option<u32> {
        self.inner.get_pane_pid()
    }

    pub fn attach(&self) -> Result<()> {
        self.inner.attach()
    }

    pub fn capture_pane(&self, lines: usize) -> Result<String> {
        self.inner.capture_pane(lines)
    }
}

/// Container terminal session for sandboxed sessions.
/// Uses a separate prefix (aoe_cterm_) to allow both container and host terminals to coexist.
pub struct ContainerTerminalSession {
    inner: PairedTerminal,
}

impl ContainerTerminalSession {
    pub fn new(id: &str, title: &str) -> Result<Self> {
        Ok(Self {
            inner: PairedTerminal::new(TerminalKind::Container, id, title),
        })
    }

    pub fn generate_name(id: &str, title: &str) -> String {
        PairedTerminal::generate_name(TerminalKind::Container, id, title)
    }

    pub fn exists(&self) -> bool {
        self.inner.exists()
    }

    pub fn is_pane_dead(&self) -> bool {
        self.inner.is_pane_dead()
    }

    pub fn create_with_size(
        &self,
        working_dir: &str,
        command: Option<&str>,
        size: Option<(u16, u16)>,
    ) -> Result<()> {
        self.inner.create_with_size(working_dir, command, size)
    }

    pub fn kill(&self) -> Result<()> {
        self.inner.kill()
    }

    pub fn get_pane_pid(&self) -> Option<u32> {
        self.inner.get_pane_pid()
    }

    pub fn attach(&self) -> Result<()> {
        self.inner.attach()
    }

    pub fn capture_pane(&self, lines: usize) -> Result<String> {
        self.inner.capture_pane(lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::test_helpers::TmuxTestSession;
    use crate::tmux::{Session, SESSION_PREFIX};

    #[test]
    fn test_terminal_session_generate_name() {
        let name = TerminalSession::generate_name("abc123def456", "My Project");
        assert!(name.starts_with(TERMINAL_PREFIX));
        assert!(name.contains("My_Project"));
        assert!(name.contains("abc123de"));
    }

    #[test]
    fn test_container_terminal_session_generate_name() {
        let name = ContainerTerminalSession::generate_name("abc123def456", "My Project");
        assert!(name.starts_with(CONTAINER_TERMINAL_PREFIX));
        assert!(name.contains("My_Project"));
        assert!(name.contains("abc123de"));
    }

    #[test]
    fn test_terminal_session_name_differs_from_agent_session() {
        let agent_name = Session::generate_name("abc123def456", "My Project");
        let terminal_name = TerminalSession::generate_name("abc123def456", "My Project");
        assert_ne!(agent_name, terminal_name);
        assert!(agent_name.starts_with(SESSION_PREFIX));
        assert!(terminal_name.starts_with(TERMINAL_PREFIX));
    }

    #[test]
    fn test_container_terminal_name_differs_from_host_terminal() {
        let host_name = TerminalSession::generate_name("abc123def456", "My Project");
        let container_name = ContainerTerminalSession::generate_name("abc123def456", "My Project");
        assert_ne!(host_name, container_name);
        assert!(host_name.starts_with(TERMINAL_PREFIX));
        assert!(container_name.starts_with(CONTAINER_TERMINAL_PREFIX));
    }

    fn tmux_available() -> bool {
        tmux_command()
            .arg("-V")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    struct KillTmuxOnDrop(String);

    impl Drop for KillTmuxOnDrop {
        fn drop(&mut self) {
            let _ = tmux_command()
                .args(["kill-session", "-t", &self.0])
                .output();
        }
    }

    #[test]
    #[serial_test::serial]
    fn terminal_session_create_marks_ownership() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let id = "termowner123456";
        let title = format!("owner-{}", std::process::id());
        let name = TerminalSession::generate_name(id, &title);
        let _guard = KillTmuxOnDrop(name.clone());
        let session = TerminalSession::new(id, &title).expect("terminal session");

        session
            .create_with_size("/tmp", Some("sleep 30"), Some((80, 24)))
            .expect("create terminal session");

        assert_eq!(
            crate::tmux::env::get_hidden_env(&name, crate::tmux::env::AOE_INSTANCE_ID_KEY)
                .as_deref(),
            Some(id)
        );
        assert_eq!(
            crate::tmux::env::get_hidden_env(&name, crate::tmux::env::AOE_SESSION_KIND_KEY)
                .as_deref(),
            Some(TmuxSessionKind::Terminal.as_str())
        );
    }

    #[test]
    #[serial_test::serial]
    fn terminal_session_create_does_not_certify_existing_unmarked_session() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let id = "termstale123456";
        let title = format!("stale-{}", std::process::id());
        let name = TerminalSession::generate_name(id, &title);
        let _guard = KillTmuxOnDrop(name.clone());
        let _ = tmux_command().args(["kill-session", "-t", &name]).output();
        let created = tmux_command()
            .args(["new-session", "-d", "-s", &name, "sleep", "30"])
            .status()
            .expect("tmux new-session");
        assert!(created.success());
        refresh_session_cache();
        let session = TerminalSession::new(id, &title).expect("terminal session");

        session
            .create_with_size("/tmp", Some("sleep 30"), Some((80, 24)))
            .expect("existing terminal session is left alone");

        assert_eq!(
            crate::tmux::env::get_hidden_env(&name, crate::tmux::env::AOE_INSTANCE_ID_KEY),
            None
        );
        assert_eq!(
            crate::tmux::env::get_hidden_env(&name, crate::tmux::env::AOE_SESSION_KIND_KEY),
            None
        );
    }

    #[test]
    #[serial_test::serial]
    fn container_terminal_session_create_marks_ownership() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let id = "ctermowner123456";
        let title = format!("owner-{}", std::process::id());
        let name = ContainerTerminalSession::generate_name(id, &title);
        let _guard = KillTmuxOnDrop(name.clone());
        let session =
            ContainerTerminalSession::new(id, &title).expect("container terminal session");

        session
            .create_with_size("/tmp", Some("sleep 30"), Some((80, 24)))
            .expect("create container terminal session");

        assert_eq!(
            crate::tmux::env::get_hidden_env(&name, crate::tmux::env::AOE_INSTANCE_ID_KEY)
                .as_deref(),
            Some(id)
        );
        assert_eq!(
            crate::tmux::env::get_hidden_env(&name, crate::tmux::env::AOE_SESSION_KIND_KEY)
                .as_deref(),
            Some(TmuxSessionKind::ContainerTerminal.as_str())
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_terminal_session_is_pane_dead_after_command_exits() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let guard = TmuxTestSession::new("aoe_test_terminal_dead");
        let session_name = guard.name().to_string();
        let session = TerminalSession {
            inner: PairedTerminal {
                name: session_name.clone(),
                instance_id: "test-instance".to_string(),
                kind: TerminalKind::Host,
            },
        };

        let output = tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                &session_name,
                "-x",
                "80",
                "-y",
                "24",
                "sleep 1",
                ";",
                "set-option",
                "-p",
                "-t",
                &session_name,
                "remain-on-exit",
                "on",
            ])
            .output()
            .expect("tmux new-session");
        assert!(output.status.success());

        std::thread::sleep(std::time::Duration::from_millis(1500));

        assert!(
            session.is_pane_dead(),
            "Terminal session pane should be dead after command exits"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_terminal_session_is_pane_dead_on_running_session() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let guard = TmuxTestSession::new("aoe_test_terminal_alive");
        let session_name = guard.name().to_string();
        let session = TerminalSession {
            inner: PairedTerminal {
                name: session_name.clone(),
                instance_id: "test-instance".to_string(),
                kind: TerminalKind::Host,
            },
        };

        let output = tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                &session_name,
                "-x",
                "80",
                "-y",
                "24",
                "sleep 30",
                ";",
                "set-option",
                "-p",
                "-t",
                &session_name,
                "remain-on-exit",
                "on",
            ])
            .output()
            .expect("tmux new-session");
        assert!(output.status.success());

        std::thread::sleep(std::time::Duration::from_millis(200));

        assert!(
            !session.is_pane_dead(),
            "Terminal session pane should be alive while command running"
        );
    }
}
