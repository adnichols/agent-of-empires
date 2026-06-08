use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::session::Status;

pub const PI_STATUS_REGISTRY_PARENT: &str = "/tmp/aoe-pi-status";
pub const PI_STATUS_EXTENSION_REL_PATH: &str = ".pi/agent/extensions/aoe-status/index.ts";
pub const PI_STATUS_REGISTRY_TTL: Duration = Duration::from_secs(60);

const PI_STATUS_EXTENSION_SOURCE: &str = r#"import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { existsSync, lstatSync, mkdirSync, renameSync, statSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import process from "node:process";

type AoePiStatus = "idle" | "running" | "stopped";

const schema = 1;
const tool = "pi";
const startedAtMs = Date.now();
const registryParent = "/tmp/aoe-pi-status";
const heartbeatMs = 30000;
let latestStatus: AoePiStatus = "idle";
let latestContext: ExtensionContext | undefined;

function currentUid(): string {
  return typeof process.getuid === "function" ? String(process.getuid()) : "unknown";
}

function safeDirectory(path: string): boolean {
  try {
    const metadata = lstatSync(path);
    return metadata.isDirectory() && !metadata.isSymbolicLink();
  } catch {
    return false;
  }
}

function ensureRegistryDir(): string | undefined {
  try {
    if (existsSync(registryParent) && !safeDirectory(registryParent)) return undefined;
    mkdirSync(registryParent, { recursive: true, mode: 0o755 });
    if (!safeDirectory(registryParent)) return undefined;

    const dir = join(registryParent, currentUid());
    if (existsSync(dir) && !safeDirectory(dir)) return undefined;
    mkdirSync(dir, { recursive: true, mode: 0o700 });
    const metadata = statSync(dir);
    if (typeof process.getuid === "function" && metadata.uid !== process.getuid()) return undefined;
    return dir;
  } catch {
    return undefined;
  }
}

function writeStatus(status: AoePiStatus, ctx?: ExtensionContext): void {
  latestStatus = status;
  latestContext = ctx;
  try {
    const dir = ensureRegistryDir();
    if (!dir) return;
    const record = {
      schema,
      tool,
      status,
      pid: process.pid,
      cwd: ctx?.cwd ?? process.cwd(),
      sessionFile: ctx?.sessionManager?.getSessionFile?.(),
      updatedAtMs: Date.now(),
      startedAtMs,
    };
    const finalPath = join(dir, `${process.pid}.json`);
    const tempPath = join(dir, `.${process.pid}.${Date.now()}.tmp`);
    writeFileSync(tempPath, JSON.stringify(record), { mode: 0o600 });
    renameSync(tempPath, finalPath);
  } catch {
    // Status reporting must never break Pi startup or turns.
  }
}

export default function (pi: ExtensionAPI) {
  writeStatus("idle");
  const heartbeat = setInterval(() => writeStatus(latestStatus, latestContext), heartbeatMs);
  (heartbeat as { unref?: () => void }).unref?.();
  pi.on("session_start", async (_event, ctx) => writeStatus("idle", ctx));
  pi.on("agent_start", async (_event, ctx) => writeStatus("running", ctx));
  pi.on("turn_start", async (_event, ctx) => writeStatus("running", ctx));
  pi.on("agent_end", async (_event, ctx) => writeStatus("idle", ctx));
  pi.on("session_shutdown", async (_event, ctx) => writeStatus("stopped", ctx));
}
"#;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PiStatusRecord {
    schema: u32,
    tool: String,
    status: PiRegistryStatus,
    pid: u32,
    cwd: Option<PathBuf>,
    updated_at_ms: u64,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum PiRegistryStatus {
    Running,
    Idle,
    Stopped,
}

impl PiRegistryStatus {
    fn to_session_status(self) -> Option<Status> {
        match self {
            Self::Running => Some(Status::Running),
            Self::Idle => Some(Status::Idle),
            Self::Stopped => None,
        }
    }
}

pub fn pi_status_extension_path() -> Result<PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
    Ok(home.join(PI_STATUS_EXTENSION_REL_PATH))
}

pub fn install_pi_status_extension() -> Result<PathBuf> {
    let path = pi_status_extension_path()?;
    install_pi_status_extension_at(&path)?;
    Ok(path)
}

pub(crate) fn install_pi_status_extension_at(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        set_owner_only_dir(parent);
    }

    if std::fs::read_to_string(path).ok().as_deref() == Some(PI_STATUS_EXTENSION_SOURCE) {
        return Ok(());
    }

    crate::session::atomic_write(path, PI_STATUS_EXTENSION_SOURCE.as_bytes())
        .with_context(|| format!("failed to write Pi status extension to {}", path.display()))?;
    set_owner_only_file(path);
    Ok(())
}

pub fn read_matching_pi_status(pane_pid: Option<u32>, project_path: &Path) -> Option<Status> {
    read_matching_pi_status_from(PI_STATUS_REGISTRY_PARENT.as_ref(), pane_pid, project_path)
}

pub(crate) fn read_matching_pi_status_from(
    registry_parent: &Path,
    pane_pid: Option<u32>,
    project_path: &Path,
) -> Option<Status> {
    let pane_pid = pane_pid?;
    let candidate_pids = crate::process::collect_pid_tree(pane_pid);
    read_matching_pi_status_with_candidates(registry_parent, &candidate_pids, project_path)
}

fn read_matching_pi_status_with_candidates(
    registry_parent: &Path,
    candidate_pids: &[u32],
    project_path: &Path,
) -> Option<Status> {
    let current_uid = current_uid()?;
    let registry_dir = registry_parent.join(current_uid.to_string());
    validate_shared_parent(registry_parent)?;
    validate_user_registry_dir(&registry_dir, current_uid)?;

    let mut best: Option<PiStatusRecord> = None;
    for entry in std::fs::read_dir(&registry_dir).ok()? {
        let Ok(entry) = entry else {
            continue;
        };
        let Some(record) = read_valid_record(&entry.path(), current_uid, candidate_pids) else {
            continue;
        };
        let record_cwd_matches = cwd_supports_match(&record, project_path);
        let replace_best = best.as_ref().is_none_or(|current| {
            let current_cwd_matches = cwd_supports_match(current, project_path);
            (record_cwd_matches && !current_cwd_matches)
                || (record_cwd_matches == current_cwd_matches
                    && record.updated_at_ms > current.updated_at_ms)
        });
        if replace_best {
            best = Some(record);
        }
    }

    best.and_then(|record| record.status.to_session_status())
}

fn read_valid_record(
    path: &Path,
    current_uid: u32,
    candidate_pids: &[u32],
) -> Option<PiStatusRecord> {
    validate_record_file(path, current_uid)?;
    let content = std::fs::read_to_string(path).ok()?;
    let record: PiStatusRecord = serde_json::from_str(&content).ok()?;
    if record.schema != 1 || record.tool != "pi" {
        return None;
    }
    if path.file_stem().and_then(|s| s.to_str()) != Some(record.pid.to_string().as_str()) {
        return None;
    }
    if !candidate_pids.contains(&record.pid) {
        return None;
    }
    if !crate::process::process_exists(record.pid) {
        return None;
    }
    if !crate::process::process_owner_is_current_user(record.pid) {
        return None;
    }
    if record_is_stale(record.updated_at_ms, PI_STATUS_REGISTRY_TTL) {
        return None;
    }
    Some(record)
}

fn cwd_supports_match(record: &PiStatusRecord, project_path: &Path) -> bool {
    record
        .cwd
        .as_ref()
        .is_none_or(|cwd| cwd == project_path || cwd.starts_with(project_path))
}

fn record_is_stale(updated_at_ms: u64, ttl: Duration) -> bool {
    let now = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis() as u64,
        Err(_) => return true,
    };
    updated_at_ms > now.saturating_add(ttl.as_millis() as u64)
        || now.saturating_sub(updated_at_ms) > ttl.as_millis() as u64
}

fn validate_shared_parent(path: &Path) -> Option<()> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return None;
    }
    Some(())
}

fn validate_user_registry_dir(path: &Path, current_uid: u32) -> Option<()> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return None;
    }
    validate_owner_and_mode(&metadata, current_uid)
}

fn validate_record_file(path: &Path, current_uid: u32) -> Option<()> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return None;
    }
    validate_owner_and_mode(&metadata, current_uid)
}

#[cfg(unix)]
fn validate_owner_and_mode(metadata: &std::fs::Metadata, current_uid: u32) -> Option<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    if metadata.uid() != current_uid {
        return None;
    }
    let mode = metadata.permissions().mode();
    if mode & 0o077 != 0 {
        return None;
    }
    Some(())
}

#[cfg(not(unix))]
fn validate_owner_and_mode(_metadata: &std::fs::Metadata, _current_uid: u32) -> Option<()> {
    Some(())
}

#[cfg(unix)]
fn current_uid() -> Option<u32> {
    let output = std::process::Command::new("id").arg("-u").output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

#[cfg(not(unix))]
fn current_uid() -> Option<u32> {
    Some(0)
}

#[cfg(unix)]
fn set_owner_only_dir(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn set_owner_only_dir(_path: &Path) {}

#[cfg(unix)]
fn set_owner_only_file(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_owner_only_file(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn write_record(parent: &Path, pid: u32, status: &str, updated_at_ms: u64, cwd: &Path) {
        let dir = parent.join(current_uid().unwrap().to_string());
        fs::create_dir_all(&dir).unwrap();
        set_owner_only_dir(&dir);
        let body = json!({
            "schema": 1,
            "tool": "pi",
            "status": status,
            "pid": pid,
            "cwd": cwd,
            "updatedAtMs": updated_at_ms,
            "startedAtMs": updated_at_ms
        });
        let path = dir.join(format!("{pid}.json"));
        fs::write(&path, serde_json::to_vec(&body).unwrap()).unwrap();
        set_owner_only_file(&path);
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    #[test]
    fn pi_status_extension_installs_expected_template() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("aoe-status/index.ts");
        install_pi_status_extension_at(&path).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("pi.on(\"session_start\""));
        assert!(content.contains("writeStatus(\"idle\")"));
        assert!(content.contains("pi.on(\"agent_start\""));
        assert!(content.contains("pi.on(\"turn_start\""));
        assert!(content.contains("pi.on(\"agent_end\""));
        assert!(content.contains("pi.on(\"session_shutdown\""));
        assert!(content.contains("renameSync(tempPath, finalPath)"));
        assert!(content.contains("const registryParent = \"/tmp/aoe-pi-status\""));
        assert!(!content.contains("tmpdir"));
        assert!(content
            .contains("setInterval(() => writeStatus(latestStatus, latestContext), heartbeatMs)"));
        assert!(content.contains("unref?.()"));
    }

    #[test]
    fn pi_status_extension_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("aoe-status/index.ts");
        install_pi_status_extension_at(&path).unwrap();
        let first = fs::read_to_string(&path).unwrap();
        install_pi_status_extension_at(&path).unwrap();
        let second = fs::read_to_string(&path).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn pi_status_extension_contains_no_conversation_fields() {
        let content = PI_STATUS_EXTENSION_SOURCE;
        for forbidden in ["prompt", "messages", "toolArgs", "toolResult", "content"] {
            assert!(!content.contains(forbidden), "forbidden field {forbidden}");
        }
        for required in [
            "schema",
            "tool",
            "status",
            "pid",
            "cwd",
            "updatedAtMs",
            "startedAtMs",
        ] {
            assert!(content.contains(required), "missing field {required}");
        }
    }

    #[test]
    fn pi_status_registry_matching_status_wins_for_candidate_pid() {
        let temp = tempfile::tempdir().unwrap();
        let pid = std::process::id();
        write_record(temp.path(), pid, "idle", now_ms(), temp.path());
        let status = read_matching_pi_status_with_candidates(temp.path(), &[pid], temp.path());
        assert_eq!(status, Some(Status::Idle));
    }

    #[test]
    fn pi_status_registry_external_record_does_not_match_candidate_pid() {
        let temp = tempfile::tempdir().unwrap();
        let pid = std::process::id();
        write_record(temp.path(), pid, "running", now_ms(), temp.path());
        let status = read_matching_pi_status_with_candidates(temp.path(), &[pid + 1], temp.path());
        assert_eq!(status, None);
    }

    #[test]
    fn pi_status_registry_ignores_stale_record() {
        let temp = tempfile::tempdir().unwrap();
        let pid = std::process::id();
        write_record(
            temp.path(),
            pid,
            "running",
            now_ms() - PI_STATUS_REGISTRY_TTL.as_millis() as u64 - 1,
            temp.path(),
        );
        let status = read_matching_pi_status_with_candidates(temp.path(), &[pid], temp.path());
        assert_eq!(status, None);
    }

    #[test]
    fn pi_status_registry_ignores_malformed_external_records() {
        let temp = tempfile::tempdir().unwrap();
        let pid = std::process::id();
        let dir = temp.path().join(current_uid().unwrap().to_string());
        fs::create_dir_all(&dir).unwrap();
        set_owner_only_dir(&dir);
        let malformed = dir.join("not-json.json");
        fs::write(&malformed, "not json").unwrap();
        set_owner_only_file(&malformed);
        write_record(temp.path(), pid, "idle", now_ms(), temp.path());
        let status = read_matching_pi_status_with_candidates(temp.path(), &[pid], temp.path());
        assert_eq!(status, Some(Status::Idle));
    }

    #[test]
    fn pi_status_registry_cwd_only_tiebreaks_pid_matches() {
        let temp = tempfile::tempdir().unwrap();
        let pid = std::process::id();
        let unrelated = tempfile::tempdir().unwrap();
        write_record(temp.path(), pid, "idle", now_ms(), unrelated.path());
        let status = read_matching_pi_status_with_candidates(temp.path(), &[pid], temp.path());
        assert_eq!(status, Some(Status::Idle));
    }

    #[test]
    fn pi_status_registry_stopped_record_does_not_override_liveness() {
        let temp = tempfile::tempdir().unwrap();
        let pid = std::process::id();
        write_record(temp.path(), pid, "stopped", now_ms(), temp.path());
        let status = read_matching_pi_status_with_candidates(temp.path(), &[pid], temp.path());
        assert_eq!(status, None);
    }

    #[cfg(unix)]
    #[test]
    fn pi_status_registry_rejects_symlinked_record() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join(current_uid().unwrap().to_string());
        fs::create_dir_all(&dir).unwrap();
        set_owner_only_dir(&dir);
        let target = temp.path().join("target.json");
        fs::write(&target, "{}").unwrap();
        std::os::unix::fs::symlink(&target, dir.join(format!("{}.json", std::process::id())))
            .unwrap();
        let status = read_matching_pi_status_with_candidates(
            temp.path(),
            &[std::process::id()],
            temp.path(),
        );
        assert_eq!(status, None);
    }

    #[cfg(unix)]
    #[test]
    fn pi_status_registry_rejects_unsafe_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let pid = std::process::id();
        write_record(temp.path(), pid, "idle", now_ms(), temp.path());
        let path = temp
            .path()
            .join(current_uid().unwrap().to_string())
            .join(format!("{pid}.json"));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let status = read_matching_pi_status_with_candidates(temp.path(), &[pid], temp.path());
        assert_eq!(status, None);
    }
}
