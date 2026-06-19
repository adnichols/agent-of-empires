//! Migration v017: backfill missing `worktree_info` on sessions that already
//! point at linked git worktrees.
//!
//! Older or manually attached sessions can store only `project_path`. Project
//! grouping then falls back to the worktree directory name instead of the main
//! repository name. This migration records unmanaged worktree metadata when the
//! path is an existing linked git worktree, leaving plain repositories,
//! missing paths, detached heads, and already annotated sessions untouched.

use anyhow::Result;
use std::fs;
use std::path::Path;
use tracing::{debug, info};

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    run_in(&app_dir)
}

pub(crate) fn run_in(app_dir: &Path) -> Result<()> {
    let profiles_dir = app_dir.join("profiles");
    if profiles_dir.exists() {
        for entry in fs::read_dir(&profiles_dir)? {
            let entry = entry?;
            if entry.path().is_dir() {
                backfill_worktree_info(&entry.path().join("sessions.json"))?;
            }
        }
    }
    backfill_worktree_info(&app_dir.join("sessions.json"))?;
    Ok(())
}

fn backfill_worktree_info(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let content = fs::read_to_string(path)?;
    let mut value: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            debug!("v017: failed to parse {}: {e}, skipping", path.display());
            return Ok(());
        }
    };

    let mut healed = 0usize;
    if let Some(array) = value.as_array_mut() {
        for session in array.iter_mut() {
            let Some(obj) = session.as_object_mut() else {
                continue;
            };
            if obj.contains_key("worktree_info") {
                continue;
            }
            let Some(project_path) = obj.get("project_path").and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(info) =
                crate::session::builder::detect_existing_worktree_info(Path::new(project_path))
            else {
                continue;
            };
            obj.insert("worktree_info".to_string(), serde_json::to_value(info)?);
            healed += 1;
        }
    }

    if healed > 0 {
        fs::write(path, serde_json::to_string_pretty(&value)?)?;
        info!(
            "v017: backfilled worktree_info on {healed} session(s) in {}",
            path.display()
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git command should run");
        assert!(
            output.status.success(),
            "git {:?} failed\nstdout: {}\nstderr: {}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_repo(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        git(dir, &["init"]);
        git(dir, &["config", "user.email", "aoe@example.test"]);
        git(dir, &["config", "user.name", "AoE Test"]);
        fs::write(dir.join("README.md"), "test\n").unwrap();
        git(dir, &["add", "README.md"]);
        git(dir, &["commit", "-m", "init"]);
    }

    #[test]
    fn backfills_linked_worktree_sessions_only() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let worktree = temp.path().join("repo-worktrees").join("feature");
        init_repo(&repo);
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "feature",
                worktree.to_str().unwrap(),
            ],
        );

        let path = temp.path().join("sessions.json");
        fs::write(
            &path,
            serde_json::to_string_pretty(&serde_json::json!([
                {"id":"wt","title":"wt","project_path": worktree, "group_path":"repo", "command":"", "tool":"claude", "yolo_mode":false, "status":"idle", "created_at":"2026-06-01T00:00:00Z"},
                {"id":"repo","title":"repo","project_path": repo, "group_path":"repo", "command":"", "tool":"claude", "yolo_mode":false, "status":"idle", "created_at":"2026-06-01T00:00:00Z"}
            ]))
            .unwrap(),
        )
        .unwrap();

        backfill_worktree_info(&path).unwrap();

        let updated: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let rows = updated.as_array().unwrap();
        let info = rows[0]
            .get("worktree_info")
            .expect("worktree row backfilled");
        assert_eq!(info["branch"], "feature");
        let expected_main = repo.canonicalize().unwrap().to_string_lossy().to_string();
        assert_eq!(
            info["main_repo_path"].as_str(),
            Some(expected_main.as_str())
        );
        assert_eq!(info["managed_by_aoe"], false);
        assert!(
            rows[1].get("worktree_info").is_none(),
            "plain repo row must stay unannotated"
        );
    }

    #[test]
    fn corrupt_file_is_skipped_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        fs::write(&path, "{ not valid json").unwrap();
        backfill_worktree_info(&path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "{ not valid json");
    }
}
