use std::fs;
use std::path::{Path, PathBuf};

fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            rust_sources(&path).into_iter().for_each(|p| out.push(p));
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            out.push(path);
        }
    }
    out
}

fn production_source(path: &Path) -> String {
    let source =
        fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    match source.find("#[cfg(test)]") {
        Some(idx) => source[..idx].to_string(),
        None => source,
    }
}

fn is_test_only_source(path: &Path) -> bool {
    path.ends_with("src/tmux/test_helpers.rs")
        || path.file_name().and_then(|name| name.to_str()) == Some("tests.rs")
}

#[test]
fn production_tmux_kill_session_calls_stay_in_low_level_wrapper() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let allowed = root.join("src/tmux/utils.rs");
    let mut offenders = Vec::new();

    for path in rust_sources(&root.join("src")) {
        if path == allowed || is_test_only_source(&path) {
            continue;
        }
        let source = production_source(&path);
        if source.contains("\"kill-session\"") {
            offenders.push(path.strip_prefix(root).unwrap().display().to_string());
        }
    }

    assert!(
        offenders.is_empty(),
        "production tmux kill-session calls must go through src/tmux/kill.rs and the low-level wrapper in src/tmux/utils.rs; offenders: {offenders:?}"
    );
}

#[test]
fn production_never_calls_tmux_kill_server() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut offenders = Vec::new();

    for path in rust_sources(&root.join("src")) {
        if is_test_only_source(&path) {
            continue;
        }
        let source = production_source(&path);
        if source.contains("\"kill-server\"") {
            offenders.push(path.strip_prefix(root).unwrap().display().to_string());
        }
    }

    assert!(
        offenders.is_empty(),
        "production code must not call tmux kill-server; offenders: {offenders:?}"
    );
}

#[test]
fn stop_all_sessions_remains_non_destructive() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source = fs::read_to_string(root.join("src/tmux/mod.rs")).expect("read src/tmux/mod.rs");
    let start = source
        .find("pub fn stop_all_sessions()")
        .expect("stop_all_sessions function exists");
    let body = &source[start..];
    let end = body.find("\n}").expect("stop_all_sessions function ends");
    let body = &body[..end];

    for forbidden in [
        "list-sessions",
        "kill-session",
        "kill_process_tree",
        "get_pane_pid",
    ] {
        assert!(
            !body.contains(forbidden),
            "stop_all_sessions must remain a non-destructive audited no-op, but body contains {forbidden:?}"
        );
    }
}
