use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use anyhow::Result;
use chrono::Utc;
use serde_json::json;

use super::env::{get_hidden_env_fresh, AOE_INSTANCE_ID_KEY, AOE_SESSION_KIND_KEY};
use super::refresh_session_cache;
use super::utils::{kill_session_if_present_unverified, tmux_command, SERVER_NAME};

const AUDIT_FILE_NAME: &str = "tmux-kill-audit.log";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TmuxSessionKind {
    Agent,
    Terminal,
    ContainerTerminal,
    Tool,
}

impl TmuxSessionKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            TmuxSessionKind::Agent => "agent",
            TmuxSessionKind::Terminal => "terminal",
            TmuxSessionKind::ContainerTerminal => "container_terminal",
            TmuxSessionKind::Tool => "tool",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TmuxKillRequest<'a> {
    pub(crate) session_name: &'a str,
    pub(crate) expected_instance_id: &'a str,
    pub(crate) expected_kind: TmuxSessionKind,
    pub(crate) reason: &'a str,
    pub(crate) caller: &'a str,
    pub(crate) allow_legacy_agent_kind: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct TmuxKillReport {
    pub(crate) audit_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TmuxSweepNoopReport {
    pub audit_path: Option<PathBuf>,
    pub audit_error: Option<String>,
}

#[derive(Debug, Clone)]
struct OwnershipProof {
    source: &'static str,
    instance_id: String,
    kind: Option<String>,
}

pub(crate) fn audit_global_sweep_disabled(caller: &str) -> TmuxSweepNoopReport {
    let event = base_event("global_sweep", caller).merge(json!({
        "result": "skipped",
        "reason": "global_sweep_disabled",
    }));
    match append_audit_event(event) {
        Ok(path) => TmuxSweepNoopReport {
            audit_path: Some(path),
            audit_error: None,
        },
        Err(err) => {
            tracing::warn!(target: "tmux.audit", error = %err, "failed to write tmux sweep audit event");
            TmuxSweepNoopReport {
                audit_path: None,
                audit_error: Some(err.to_string()),
            }
        }
    }
}

pub(crate) fn kill_tmux_session(request: TmuxKillRequest<'_>) -> Result<TmuxKillReport> {
    let attempt_event = base_kill_event("attempt", &request, None, None).merge(json!({
        "result": "attempting",
    }));
    append_audit_event(attempt_event).map_err(|err| {
        tracing::warn!(target: "tmux.audit", error = %err, session = %request.session_name, "refusing tmux kill without durable attempt audit");
        anyhow::anyhow!("failed to write required tmux kill audit event: {err}")
    })?;

    let mut audit_error = None;

    if !session_exists(request.session_name) {
        record_event(
            &mut audit_error,
            base_kill_event("result", &request, None, None).merge(json!({
                "result": "already_absent",
            })),
        );
        return Ok(TmuxKillReport { audit_error });
    }

    let ownership = match verify_ownership(&request) {
        Ok(proof) => proof,
        Err(reason) => {
            record_event(
                &mut audit_error,
                base_kill_event("skipped", &request, None, None).merge(json!({
                    "result": "skipped",
                    "reason": reason,
                })),
            );
            return Ok(TmuxKillReport { audit_error });
        }
    };

    let pane_pid = crate::process::get_pane_pid(request.session_name);
    if matches!(pane_pid, Some(0 | 1)) {
        let reason = format!("unsafe_pane_pid_{}", pane_pid.unwrap());
        record_event(
            &mut audit_error,
            base_kill_event("skipped", &request, Some(&ownership), pane_pid).merge(json!({
                "result": "skipped",
                "reason": reason,
            })),
        );
        return Ok(TmuxKillReport { audit_error });
    }

    if let Some(pid) = pane_pid {
        crate::process::kill_process_tree(pid);
    }

    let kill_result = kill_session_if_present_unverified(request.session_name);
    match kill_result {
        Ok(()) => {
            refresh_session_cache();
            record_event(
                &mut audit_error,
                base_kill_event("result", &request, Some(&ownership), pane_pid).merge(json!({
                    "result": "killed",
                })),
            );
            Ok(TmuxKillReport { audit_error })
        }
        Err(err) => {
            let message = err.to_string();
            record_event(
                &mut audit_error,
                base_kill_event("result", &request, Some(&ownership), pane_pid).merge(json!({
                    "result": "error",
                    "error": message,
                })),
            );
            if let Some(error) = &audit_error {
                tracing::warn!(target: "tmux.audit", error = %error, "failed to write tmux kill failure audit event");
            }
            Err(err)
        }
    }
}

fn session_exists(name: &str) -> bool {
    tmux_command()
        .args(["has-session", "-t", name])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn verify_ownership(request: &TmuxKillRequest<'_>) -> std::result::Result<OwnershipProof, String> {
    let Some(instance_id) = get_hidden_env_fresh(request.session_name, AOE_INSTANCE_ID_KEY) else {
        return Err("missing_instance_id".to_string());
    };
    if instance_id != request.expected_instance_id {
        return Err("instance_id_mismatch".to_string());
    }

    let kind = get_hidden_env_fresh(request.session_name, AOE_SESSION_KIND_KEY);
    match kind.as_deref() {
        Some(value) if value == request.expected_kind.as_str() => Ok(OwnershipProof {
            source: "hidden_env",
            instance_id,
            kind,
        }),
        Some(_) => Err("session_kind_mismatch".to_string()),
        None if request.allow_legacy_agent_kind
            && request.expected_kind == TmuxSessionKind::Agent =>
        {
            Ok(OwnershipProof {
                source: "hidden_instance_id_legacy_agent",
                instance_id,
                kind,
            })
        }
        None => Err("missing_session_kind".to_string()),
    }
}

fn base_event(event: &str, caller: &str) -> serde_json::Value {
    json!({
        "timestamp": Utc::now(),
        "event": event,
        "caller": caller,
        "process_id": std::process::id(),
        "build_version": env!("CARGO_PKG_VERSION"),
        "tmux_server": SERVER_NAME,
    })
}

fn base_kill_event(
    event: &str,
    request: &TmuxKillRequest<'_>,
    ownership: Option<&OwnershipProof>,
    pane_pid: Option<u32>,
) -> serde_json::Value {
    base_event(event, request.caller).merge(json!({
        "session_name": request.session_name,
        "expected_instance_id": request.expected_instance_id,
        "expected_kind": request.expected_kind.as_str(),
        "reason": request.reason,
        "ownership_proof": ownership.map(|proof| proof.source),
        "actual_instance_id": ownership.map(|proof| proof.instance_id.as_str()),
        "actual_kind": ownership.and_then(|proof| proof.kind.as_deref()),
        "pane_pid": pane_pid,
    }))
}

trait JsonMerge {
    fn merge(self, other: serde_json::Value) -> serde_json::Value;
}

impl JsonMerge for serde_json::Value {
    fn merge(mut self, other: serde_json::Value) -> serde_json::Value {
        if let (Some(base), Some(extra)) = (self.as_object_mut(), other.as_object()) {
            for (key, value) in extra {
                base.insert(key.clone(), value.clone());
            }
        }
        self
    }
}

fn write_event(event: serde_json::Value) -> Option<String> {
    match append_audit_event(event) {
        Ok(_) => None,
        Err(err) => {
            tracing::warn!(target: "tmux.audit", error = %err, "failed to write tmux kill audit event");
            Some(err.to_string())
        }
    }
}

fn record_event(audit_error: &mut Option<String>, event: serde_json::Value) {
    if let Some(error) = write_event(event) {
        audit_error.get_or_insert(error);
    }
}

fn append_audit_event(event: serde_json::Value) -> Result<PathBuf> {
    let path = audit_file_path()?;
    let mut line = serde_json::to_vec(&event)?;
    line.push(b'\n');

    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&path)?
    };

    #[cfg(not(unix))]
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;

    file.write_all(&line)?;
    Ok(path)
}

fn audit_file_path() -> Result<PathBuf> {
    Ok(crate::session::get_app_dir()?.join(AUDIT_FILE_NAME))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EnvGuard {
        home: Option<String>,
        xdg_config_home: Option<String>,
    }

    impl EnvGuard {
        fn isolate(temp: &tempfile::TempDir) -> Self {
            let guard = Self {
                home: std::env::var("HOME").ok(),
                xdg_config_home: std::env::var("XDG_CONFIG_HOME").ok(),
            };
            unsafe {
                std::env::set_var("HOME", temp.path());
                std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));
            }
            guard
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.home {
                    Some(value) => std::env::set_var("HOME", value),
                    None => std::env::remove_var("HOME"),
                }
                match &self.xdg_config_home {
                    Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
                    None => std::env::remove_var("XDG_CONFIG_HOME"),
                }
            }
        }
    }

    struct KillTmuxOnDrop(String);

    impl Drop for KillTmuxOnDrop {
        fn drop(&mut self) {
            let _ = tmux_command()
                .args(["kill-session", "-t", &self.0])
                .output();
        }
    }

    fn tmux_available() -> bool {
        tmux_command()
            .arg("-V")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }

    fn create_tmux_session(name: &str) {
        let _ = tmux_command().args(["kill-session", "-t", name]).output();
        let created = tmux_command()
            .args(["new-session", "-d", "-s", name, "sleep", "30"])
            .status()
            .expect("tmux new-session");
        assert!(created.success(), "failed to create {name}");
    }

    fn audit_events() -> Vec<serde_json::Value> {
        let audit = std::fs::read_to_string(audit_file_path().expect("audit path"))
            .expect("read audit file");
        audit
            .lines()
            .map(|line| serde_json::from_str(line).expect("audit json line"))
            .collect()
    }

    #[test]
    fn session_kind_values_are_stable() {
        assert_eq!(TmuxSessionKind::Agent.as_str(), "agent");
        assert_eq!(TmuxSessionKind::Terminal.as_str(), "terminal");
        assert_eq!(
            TmuxSessionKind::ContainerTerminal.as_str(),
            "container_terminal"
        );
        assert_eq!(TmuxSessionKind::Tool.as_str(), "tool");
    }

    #[test]
    fn audit_event_merge_keeps_base_fields() {
        let merged = json!({"event":"attempt", "caller":"test"}).merge(json!({"result":"skipped"}));
        assert_eq!(merged["event"], "attempt");
        assert_eq!(merged["caller"], "test");
        assert_eq!(merged["result"], "skipped");
    }

    #[test]
    #[serial_test::serial]
    fn tmux_kill_audit_global_sweep_writes_json() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let _env = EnvGuard::isolate(&temp);

        let report = audit_global_sweep_disabled("test::global_sweep");

        assert!(report.audit_error.is_none());
        assert!(report.audit_path.is_some());
        let events = audit_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event"], "global_sweep");
        assert_eq!(events[0]["caller"], "test::global_sweep");
        assert_eq!(events[0]["reason"], "global_sweep_disabled");
        assert_eq!(events[0]["result"], "skipped");
    }

    #[test]
    #[serial_test::serial]
    fn tmux_kill_audit_missing_ownership_skips_and_preserves_session() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }
        let temp = tempfile::TempDir::new().expect("tempdir");
        let _env = EnvGuard::isolate(&temp);
        let name = format!("aoe_test_kill_audit_skip_{}", std::process::id());
        let _guard = KillTmuxOnDrop(name.clone());
        create_tmux_session(&name);

        let report = kill_tmux_session(TmuxKillRequest {
            session_name: &name,
            expected_instance_id: "expected-id",
            expected_kind: TmuxSessionKind::Tool,
            reason: "test_missing_marker",
            caller: "test::missing_marker",
            allow_legacy_agent_kind: false,
        })
        .expect("kill helper should skip, not error");

        assert!(report.audit_error.is_none());
        let still_exists = tmux_command()
            .args(["has-session", "-t", &name])
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        assert!(still_exists, "unverified session must be preserved");
        let events = audit_events();
        assert_eq!(events[0]["event"], "attempt");
        let skipped = events
            .iter()
            .find(|event| event["event"] == "skipped")
            .expect("skipped event");
        assert_eq!(skipped["reason"], "missing_instance_id");
        assert_eq!(skipped["result"], "skipped");
    }

    #[test]
    #[serial_test::serial]
    #[cfg(unix)]
    fn tmux_kill_refuses_to_kill_when_attempt_audit_fails() {
        use std::os::unix::fs::PermissionsExt;

        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }
        let temp = tempfile::TempDir::new().expect("tempdir");
        let _env = EnvGuard::isolate(&temp);
        let name = format!("aoe_test_kill_audit_required_{}", std::process::id());
        let _guard = KillTmuxOnDrop(name.clone());
        create_tmux_session(&name);
        super::super::env::set_hidden_env(&name, AOE_INSTANCE_ID_KEY, "expected-id")
            .expect("set instance marker");
        super::super::env::set_hidden_env(
            &name,
            AOE_SESSION_KIND_KEY,
            TmuxSessionKind::Tool.as_str(),
        )
        .expect("set kind marker");
        let app_dir = crate::session::get_app_dir().expect("app dir");
        std::fs::set_permissions(&app_dir, std::fs::Permissions::from_mode(0o500))
            .expect("make app dir unwritable");

        let err = kill_tmux_session(TmuxKillRequest {
            session_name: &name,
            expected_instance_id: "expected-id",
            expected_kind: TmuxSessionKind::Tool,
            reason: "test_audit_failure",
            caller: "test::audit_failure",
            allow_legacy_agent_kind: false,
        })
        .expect_err("kill must fail closed when audit attempt cannot be written");

        std::fs::set_permissions(&app_dir, std::fs::Permissions::from_mode(0o700))
            .expect("restore app dir permissions");
        assert!(
            err.to_string()
                .contains("failed to write required tmux kill audit event"),
            "unexpected error: {err:#}"
        );
        let still_exists = tmux_command()
            .args(["has-session", "-t", &name])
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        assert!(still_exists, "session must survive required audit failure");
    }

    #[test]
    #[serial_test::serial]
    fn tmux_kill_does_not_trust_stale_cached_ownership() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }
        let temp = tempfile::TempDir::new().expect("tempdir");
        let _env = EnvGuard::isolate(&temp);
        let name = format!("aoe_test_kill_stale_cache_{}", std::process::id());
        let _guard = KillTmuxOnDrop(name.clone());
        create_tmux_session(&name);
        super::super::env::set_hidden_env(&name, AOE_INSTANCE_ID_KEY, "expected-id")
            .expect("set instance marker");
        super::super::env::set_hidden_env(
            &name,
            AOE_SESSION_KIND_KEY,
            TmuxSessionKind::Tool.as_str(),
        )
        .expect("set kind marker");
        assert_eq!(
            super::super::env::get_hidden_env(&name, AOE_INSTANCE_ID_KEY).as_deref(),
            Some("expected-id")
        );
        assert_eq!(
            super::super::env::get_hidden_env(&name, AOE_SESSION_KIND_KEY).as_deref(),
            Some(TmuxSessionKind::Tool.as_str())
        );
        let _ = tmux_command().args(["kill-session", "-t", &name]).output();
        create_tmux_session(&name);

        let report = kill_tmux_session(TmuxKillRequest {
            session_name: &name,
            expected_instance_id: "expected-id",
            expected_kind: TmuxSessionKind::Tool,
            reason: "test_stale_cache",
            caller: "test::stale_cache",
            allow_legacy_agent_kind: false,
        })
        .expect("kill helper should skip stale cached ownership");

        assert!(report.audit_error.is_none());
        let still_exists = tmux_command()
            .args(["has-session", "-t", &name])
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        assert!(still_exists, "new unmarked session must be preserved");
        let events = audit_events();
        let skipped = events
            .iter()
            .find(|event| event["event"] == "skipped")
            .expect("skipped event");
        assert_eq!(skipped["reason"], "missing_instance_id");
    }

    #[test]
    #[serial_test::serial]
    fn tmux_kill_audit_verified_session_records_killed() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }
        let temp = tempfile::TempDir::new().expect("tempdir");
        let _env = EnvGuard::isolate(&temp);
        let name = format!("aoe_test_kill_audit_kill_{}", std::process::id());
        let _guard = KillTmuxOnDrop(name.clone());
        create_tmux_session(&name);
        super::super::env::set_hidden_env(&name, AOE_INSTANCE_ID_KEY, "expected-id")
            .expect("set instance marker");
        super::super::env::set_hidden_env(
            &name,
            AOE_SESSION_KIND_KEY,
            TmuxSessionKind::Tool.as_str(),
        )
        .expect("set kind marker");

        let report = kill_tmux_session(TmuxKillRequest {
            session_name: &name,
            expected_instance_id: "expected-id",
            expected_kind: TmuxSessionKind::Tool,
            reason: "test_verified_kill",
            caller: "test::verified_kill",
            allow_legacy_agent_kind: false,
        })
        .expect("verified session kill");

        assert!(report.audit_error.is_none());
        let still_exists = tmux_command()
            .args(["has-session", "-t", &name])
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        assert!(!still_exists, "verified session must be killed");
        let events = audit_events();
        let result = events
            .iter()
            .find(|event| event["event"] == "result")
            .expect("result event");
        assert_eq!(result["result"], "killed");
        assert_eq!(result["reason"], "test_verified_kill");
        assert_eq!(result["ownership_proof"], "hidden_env");
        assert_eq!(result["actual_instance_id"], "expected-id");
        assert_eq!(result["actual_kind"], TmuxSessionKind::Tool.as_str());
    }
}
