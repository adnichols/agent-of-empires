//! Tool sessions: user-configured dev tools (lazygit, yazi, tig, etc.) that
//! run in persistent tmux sessions tied to an agent session's working directory.

use anyhow::{bail, Result};

use super::utils::{
    append_clipboard_passthrough_args, append_mouse_on_args, append_pane_base_index_args,
    append_remain_on_exit_args, append_window_size_args, ensure_aoe_server_stays_alive,
    is_pane_dead, sanitize_session_name, tmux_command,
};
use super::{
    kill_tmux_session, refresh_session_cache, session_exists_from_cache, TmuxKillRequest,
    TmuxSessionKind, TOOL_PREFIX,
};
use crate::cli::truncate_id;
use crate::session::config::should_apply_tmux_clipboard;

pub struct ToolSession {
    name: String,
    instance_id: String,
}

impl ToolSession {
    pub fn new(session_id: &str, session_title: &str, tool_name: &str) -> Self {
        let safe_title = sanitize_session_name(session_title);
        let safe_tool = sanitize_session_name(tool_name);
        let name = format!(
            "{}{}_{}_{}",
            TOOL_PREFIX,
            safe_tool,
            safe_title,
            truncate_id(session_id, 8)
        );
        Self {
            name,
            instance_id: session_id.to_string(),
        }
    }

    pub fn session_name(&self) -> &str {
        &self.name
    }

    pub fn exists(&self) -> bool {
        if let Some(exists) = session_exists_from_cache(&self.name) {
            return exists;
        }

        tmux_command()
            .args(["has-session", "-t", &self.name])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    pub fn is_pane_dead(&self) -> bool {
        is_pane_dead(&self.name)
    }

    pub fn create_with_size(
        &self,
        working_dir: &str,
        command: &str,
        size: Option<(u16, u16)>,
    ) -> Result<()> {
        ensure_aoe_server_stays_alive()?;

        if self.exists() {
            return Ok(());
        }

        let mut args = vec![
            "new-session".to_string(),
            "-d".to_string(),
            "-s".to_string(),
            self.name.clone(),
            "-c".to_string(),
            working_dir.to_string(),
        ];

        if let Some((width, height)) = size {
            args.push("-x".to_string());
            args.push(width.to_string());
            args.push("-y".to_string());
            args.push(height.to_string());
        }

        args.push(command.to_string());

        append_remain_on_exit_args(&mut args, &self.name);
        append_pane_base_index_args(&mut args, &self.name);
        append_mouse_on_args(&mut args, &self.name);
        append_window_size_args(&mut args, &self.name);
        if should_apply_tmux_clipboard() {
            append_clipboard_passthrough_args(&mut args, &self.name);
        }

        if super::utils::legacy_default_session_exists(&self.name) {
            bail!(
                "tool session '{}' exists in the legacy default tmux server; restart or stop that pre-upgrade session before creating it on the AOE tmux server",
                self.name
            );
        }

        let output = tmux_command().args(&args).output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("duplicate session") {
                refresh_session_cache();
                return Ok(());
            }
            bail!("Failed to create tool session '{}': {}", self.name, stderr);
        }

        self.mark_ownership()?;
        refresh_session_cache();
        Ok(())
    }

    fn mark_ownership(&self) -> Result<()> {
        let entries = [
            (
                self.name.as_str(),
                super::env::AOE_INSTANCE_ID_KEY,
                self.instance_id.as_str(),
            ),
            (
                self.name.as_str(),
                super::env::AOE_SESSION_KIND_KEY,
                TmuxSessionKind::Tool.as_str(),
            ),
        ];
        super::env::set_hidden_env_batch(&entries).map_err(|e| {
            tracing::warn!(target: "tmux.ownership", session = %self.name, error = %e, "failed to mark tool tmux ownership");
            e
        })
    }

    pub fn kill(&self) -> Result<()> {
        let report = kill_tmux_session(TmuxKillRequest {
            session_name: &self.name,
            expected_instance_id: &self.instance_id,
            expected_kind: TmuxSessionKind::Tool,
            reason: "tool_session_stop",
            caller: "tmux::ToolSession::kill",
            allow_legacy_agent_kind: false,
        })?;
        if let Some(error) = report.audit_error {
            tracing::warn!(target: "tmux.audit", error = %error, session = %self.name, "tmux kill audit write failed");
        }
        Ok(())
    }

    pub fn attach(&self) -> Result<()> {
        if !self.exists() {
            bail!("Tool session does not exist: {}", self.name);
        }

        let status = tmux_command()
            .args(["attach-session", "-t", &self.name])
            .status()?;

        if !status.success() {
            bail!("Failed to attach to tool session '{}'", self.name);
        }

        Ok(())
    }

    pub fn capture_pane(&self, lines: usize) -> Result<String> {
        super::Session::from_name(&self.name).capture_pane(lines)
    }
}

/// Kill marker-verified tool sessions associated with an agent session ID.
pub fn kill_all_tool_sessions_for_id(session_id: &str) {
    let id_suffix = format!("_{}", truncate_id(session_id, 8));

    let output = tmux_command()
        .args(["list-sessions", "-F", "#{session_name}"])
        .output();

    if let Ok(out) = output {
        if out.status.success() {
            let stdout = String::from_utf8_lossy(&out.stdout);
            for line in stdout.lines() {
                if line.starts_with(TOOL_PREFIX) && line.ends_with(&id_suffix) {
                    let result = kill_tmux_session(TmuxKillRequest {
                        session_name: line,
                        expected_instance_id: session_id,
                        expected_kind: TmuxSessionKind::Tool,
                        reason: "tool_session_cleanup_for_instance",
                        caller: "tmux::kill_all_tool_sessions_for_id",
                        allow_legacy_agent_kind: false,
                    });
                    if let Err(e) = result {
                        tracing::debug!(target: "session.tmux_cleanup", session_id = %session_id, tool_session = %line, error = %e, "tool session cleanup failed");
                    }
                }
            }
        }
    }

    refresh_session_cache();
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn tool_session_create_marks_ownership() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let id = "toolowner123456";
        let title = format!("owner-{}", std::process::id());
        let session = ToolSession::new(id, &title, "echotool");
        let name = session.session_name().to_string();
        let _guard = KillTmuxOnDrop(name.clone());

        session
            .create_with_size("/tmp", "sleep 30", Some((80, 24)))
            .expect("create tool session");

        assert_eq!(
            crate::tmux::env::get_hidden_env(&name, crate::tmux::env::AOE_INSTANCE_ID_KEY)
                .as_deref(),
            Some(id)
        );
        assert_eq!(
            crate::tmux::env::get_hidden_env(&name, crate::tmux::env::AOE_SESSION_KIND_KEY)
                .as_deref(),
            Some(TmuxSessionKind::Tool.as_str())
        );
    }

    #[test]
    #[serial_test::serial]
    fn create_does_not_certify_existing_unmarked_tool_session() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let id = "toolstale123456";
        let title = format!("stale-{}", std::process::id());
        let session = ToolSession::new(id, &title, "echotool");
        let name = session.session_name().to_string();
        let _guard = KillTmuxOnDrop(name.clone());
        let _ = tmux_command().args(["kill-session", "-t", &name]).output();
        let created = tmux_command()
            .args(["new-session", "-d", "-s", &name, "sleep", "30"])
            .status()
            .expect("tmux new-session");
        assert!(created.success());
        refresh_session_cache();

        session
            .create_with_size("/tmp", "sleep 30", Some((80, 24)))
            .expect("existing tool session is left alone");

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
    fn cleanup_for_id_skips_suffix_match_without_ownership_marker() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let id = "skiptool123456";
        let suffix = truncate_id(id, 8);
        let name = format!("{}legacy_session_{}", TOOL_PREFIX, suffix);
        let _guard = KillTmuxOnDrop(name.clone());
        let _ = tmux_command().args(["kill-session", "-t", &name]).output();
        let created = tmux_command()
            .args(["new-session", "-d", "-s", &name, "sleep", "30"])
            .status()
            .expect("tmux new-session");
        assert!(created.success());

        kill_all_tool_sessions_for_id(id);

        let exists = tmux_command()
            .args(["has-session", "-t", &name])
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        assert!(exists, "suffix-only tool cleanup must preserve {name}");
    }

    #[test]
    #[serial_test::serial]
    fn cleanup_for_id_kills_marker_verified_tool_session() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let id = "killtool123456";
        let suffix = truncate_id(id, 8);
        let name = format!("{}verified_session_{}", TOOL_PREFIX, suffix);
        let _guard = KillTmuxOnDrop(name.clone());
        let _ = tmux_command().args(["kill-session", "-t", &name]).output();
        let created = tmux_command()
            .args(["new-session", "-d", "-s", &name, "sleep", "30"])
            .status()
            .expect("tmux new-session");
        assert!(created.success());
        crate::tmux::env::set_hidden_env(&name, crate::tmux::env::AOE_INSTANCE_ID_KEY, id)
            .expect("set instance marker");
        crate::tmux::env::set_hidden_env(
            &name,
            crate::tmux::env::AOE_SESSION_KIND_KEY,
            TmuxSessionKind::Tool.as_str(),
        )
        .expect("set kind marker");

        kill_all_tool_sessions_for_id(id);

        let exists = tmux_command()
            .args(["has-session", "-t", &name])
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        assert!(!exists, "marker-verified tool cleanup must kill {name}");
    }

    #[test]
    fn new_name_includes_prefix_tool_title_and_truncated_id() {
        let s = ToolSession::new("0123456789abcdef", "my-session", "lazygit");
        let name = s.session_name();
        assert!(name.starts_with(TOOL_PREFIX), "name was {}", name);
        assert!(name.contains("lazygit"));
        assert!(name.contains("my-session"));
        assert!(name.ends_with("_01234567"), "name was {}", name);
    }

    #[test]
    fn new_name_sanitizes_unsafe_characters() {
        // tmux session names can't contain ':' or '.'
        let s = ToolSession::new("abc12345", "feature/foo:bar", "my tool.v2");
        let name = s.session_name();
        assert!(!name.contains(':'), "name was {}", name);
        assert!(!name.contains('.'), "name was {}", name);
        assert!(!name.contains(' '), "name was {}", name);
    }

    #[test]
    fn distinct_tools_on_same_session_have_distinct_names() {
        let id = "0123456789abcdef";
        let lazygit = ToolSession::new(id, "x", "lazygit");
        let yazi = ToolSession::new(id, "x", "yazi");
        assert_ne!(lazygit.session_name(), yazi.session_name());
    }

    #[test]
    fn distinct_sessions_for_same_tool_have_distinct_names() {
        let a = ToolSession::new("aaaaaaaa1111", "x", "lazygit");
        let b = ToolSession::new("bbbbbbbb2222", "x", "lazygit");
        assert_ne!(a.session_name(), b.session_name());
    }
}
