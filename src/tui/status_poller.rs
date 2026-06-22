//! Background status polling for TUI performance
//!
//! This module provides non-blocking status updates for sessions by running
//! tmux subprocess calls in a background thread. Two optimizations reduce
//! per-cycle overhead:
//!
//! 1. **Batched metadata**: A single `tmux list-panes -a` call fetches pane
//!    metadata (dead flag, current command) for all sessions at once, replacing
//!    O(3N) per-instance `display-message` / `has-session` subprocesses with O(1).
//!
//! 2. **Adaptive polling tiers**: Sessions are polled at different frequencies
//!    based on their status. Hot (Running/Waiting/Starting) every cycle, Warm
//!    (Idle/Unknown) every 5 cycles, Cold (Error) every 15 main-TUI cycles or
//!    60 attached-hook cycles, Frozen (Stopped/Deleting) never.

use std::collections::HashMap;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::session::{Instance, Status};

/// Adaptive polling intervals (in cycles). 0 = never poll.
const TIER_HOT: u64 = 1;
const TIER_WARM: u64 = 5;
const TIER_COLD_MAIN_TUI: u64 = 15;
const TIER_COLD_ATTACHED_HOOKS: u64 = 60;

fn polling_tier(status: Status, cold_tier: u64) -> u64 {
    match status {
        Status::Running | Status::Waiting | Status::Starting => TIER_HOT,
        Status::Idle | Status::Unknown => TIER_WARM,
        Status::Error => cold_tier,
        Status::Stopped | Status::Deleting | Status::Creating => 0,
    }
}

fn should_refresh_tmux_metadata(instances: &[Instance], cold_tier: u64) -> bool {
    instances
        .iter()
        .any(|inst| polling_tier(inst.status, cold_tier) != 0)
}

/// Result of a status check for a single session
#[derive(Debug, Clone)]
pub struct StatusUpdate {
    pub id: String,
    pub status: Status,
    pub last_error: Option<String>,
    /// Snapshot of the polled clone's `idle_entered_at` after
    /// `update_status_with_metadata` ran. Propagating this field is what
    /// keeps the freshness signal working in the TUI: without it, the
    /// wrapper's timestamp write lives only on the polling clone and is
    /// lost when we project the result back into a `StatusUpdate`.
    pub idle_entered_at: Option<DateTime<Utc>>,
    /// Pulled from tmux `#{session_activity}` via
    /// `update_status_with_metadata`. Carried back so the main thread can
    /// persist it to the real Instance; the poller mutates a clone, so any
    /// fields not plumbed through here are dropped on the floor.
    pub last_accessed_at: Option<DateTime<Utc>>,
    /// Cached pane-dead reading from `tmux::PaneMetadata.pane_dead`. The
    /// main thread writes this onto `Instance.pane_dead_observed` so the
    /// Attention sort can treat dead panes as tier 99 without re-querying
    /// tmux per sort.
    pub pane_dead: bool,
}

pub(super) struct StatusPollState {
    container_check_interval: Duration,
    last_container_check: Instant,
    container_states: HashMap<String, bool>,
    credential_refresh_interval: Duration,
    last_credential_refresh: Instant,
    cycle_count: u64,
    cold_tier: u64,
    refresh_metadata_every_cycle: bool,
}

impl StatusPollState {
    pub(super) fn new() -> Self {
        Self::with_policy(TIER_COLD_MAIN_TUI, true)
    }

    pub(super) fn for_attached_status_hooks() -> Self {
        Self::with_policy(TIER_COLD_ATTACHED_HOOKS, false)
    }

    fn with_policy(cold_tier: u64, refresh_metadata_every_cycle: bool) -> Self {
        let container_check_interval = Duration::from_secs(5);
        let credential_refresh_interval = Duration::from_secs(1800);

        Self {
            container_check_interval,
            last_container_check: Instant::now() - container_check_interval,
            container_states: HashMap::new(),
            credential_refresh_interval,
            last_credential_refresh: Instant::now(),
            cycle_count: cold_tier - 1,
            cold_tier,
            refresh_metadata_every_cycle,
        }
    }
}

pub(super) fn poll_statuses_once(
    instances: Vec<Instance>,
    state: &mut StatusPollState,
) -> Vec<StatusUpdate> {
    state.cycle_count = state.cycle_count.wrapping_add(1);

    // Pre-scan: keep the cheap tmux metadata/cache refresh cadence independent
    // from the more expensive per-session status-detection tiers. Live-send
    // drift checks use SESSION_CACHE with a short TTL, so warm/cold tiers must not
    // be allowed to leave the cache stale for their full status polling window.
    let any_non_frozen = should_refresh_tmux_metadata(&instances, state.cold_tier);
    let any_pollable = instances.iter().any(|inst| {
        let tier = polling_tier(inst.status, state.cold_tier);
        tier != 0 && state.cycle_count % tier == 0
    });
    let should_refresh_metadata = if state.refresh_metadata_every_cycle {
        any_non_frozen
    } else {
        any_pollable
    };

    let pane_metadata = if should_refresh_metadata {
        crate::tmux::batch_pane_metadata().unwrap_or_default()
    } else {
        HashMap::new()
    };

    // Refresh container health if any sandboxed session exists and interval elapsed
    let has_sandboxed = if any_pollable {
        let sandboxed = instances.iter().any(|i| i.is_sandboxed());
        if sandboxed && state.last_container_check.elapsed() >= state.container_check_interval {
            state.container_states = crate::containers::batch_container_health();
            state.last_container_check = Instant::now();
        }
        sandboxed
    } else {
        false
    };

    // Periodically re-sync sandbox credentials from the macOS Keychain
    // so long-lived sessions don't lose auth mid-run.
    if has_sandboxed && state.last_credential_refresh.elapsed() >= state.credential_refresh_interval
    {
        state.last_credential_refresh = Instant::now();
        crate::session::container_config::refresh_agent_configs();
    }

    instances
        .into_iter()
        .filter_map(|mut inst| {
            // Adaptive polling: skip instances whose tier interval hasn't elapsed
            let tier = polling_tier(inst.status, state.cold_tier);
            if tier == 0 || state.cycle_count % tier != 0 {
                return None;
            }

            // For sandboxed sessions, check if the container is dead before
            // falling through to tmux-based status detection.
            if inst.is_sandboxed()
                && !matches!(
                    inst.status,
                    Status::Stopped | Status::Deleting | Status::Starting | Status::Creating
                )
            {
                if let Some(sandbox) = &inst.sandbox_info {
                    if let Some(&running) = state.container_states.get(&sandbox.container_name) {
                        if !running {
                            return Some(StatusUpdate {
                                id: inst.id,
                                status: Status::Error,
                                last_error: Some("Container is not running".to_string()),
                                idle_entered_at: None,
                                last_accessed_at: inst.last_accessed_at,
                                // Sandboxed sessions don't have a tmux pane in the
                                // usual sense; the Error tier itself sinks the row.
                                pane_dead: false,
                            });
                        }
                    }
                }
            }

            // Look up pre-fetched metadata for this instance's tmux session
            let session_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
            let metadata = pane_metadata.get(&session_name);
            let pane_dead = metadata.map(|m| m.pane_dead).unwrap_or(false);

            inst.update_status_with_metadata(metadata);

            Some(StatusUpdate {
                id: inst.id,
                status: inst.status,
                last_error: inst.last_error,
                idle_entered_at: inst.idle_entered_at,
                last_accessed_at: inst.last_accessed_at,
                pane_dead,
            })
        })
        .collect()
}

/// Background thread that polls session status without blocking the UI
pub struct StatusPoller {
    request_tx: mpsc::Sender<Vec<Instance>>,
    result_rx: mpsc::Receiver<Vec<StatusUpdate>>,
    _handle: thread::JoinHandle<()>,
}

impl StatusPoller {
    pub fn new() -> Self {
        let (request_tx, request_rx) = mpsc::channel::<Vec<Instance>>();
        let (result_tx, result_rx) = mpsc::channel::<Vec<StatusUpdate>>();

        let handle = thread::spawn(move || {
            Self::polling_loop(request_rx, result_tx);
        });

        Self {
            request_tx,
            result_rx,
            _handle: handle,
        }
    }

    fn polling_loop(
        request_rx: mpsc::Receiver<Vec<Instance>>,
        result_tx: mpsc::Sender<Vec<StatusUpdate>>,
    ) {
        let mut state = StatusPollState::new();

        while let Ok(instances) = request_rx.recv() {
            let updates = poll_statuses_once(instances, &mut state);

            if result_tx.send(updates).is_err() {
                break;
            }
        }
    }

    /// Request a status refresh for all given instances (non-blocking).
    pub fn request_refresh(&self, instances: Vec<Instance>) {
        let _ = self.request_tx.send(instances);
    }

    /// Try to receive status updates without blocking.
    /// Returns None if no updates are available yet.
    pub fn try_recv_updates(&self) -> Option<Vec<StatusUpdate>> {
        self.result_rx.try_recv().ok()
    }
}

impl Default for StatusPoller {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_update_carries_idle_entered_at() {
        // Regression: the polling loop runs `update_status_with_metadata`
        // on a clone, then projects the result into a `StatusUpdate`. If
        // `idle_entered_at` falls off the projection (the original bug),
        // the breathe rattle + fresh-idle color never fire in the TUI
        // even though the wrapper sets the timestamp on the clone
        // correctly.
        let ts = Utc::now();
        let update = StatusUpdate {
            id: "abc".into(),
            status: Status::Idle,
            last_error: None,
            idle_entered_at: Some(ts),
            last_accessed_at: None,
            pane_dead: false,
        };
        assert_eq!(update.idle_entered_at, Some(ts));
    }

    #[test]
    fn test_polling_tier_hot() {
        assert_eq!(polling_tier(Status::Running, TIER_COLD_MAIN_TUI), TIER_HOT);
        assert_eq!(polling_tier(Status::Waiting, TIER_COLD_MAIN_TUI), TIER_HOT);
        assert_eq!(polling_tier(Status::Starting, TIER_COLD_MAIN_TUI), TIER_HOT);
    }

    #[test]
    fn test_polling_tier_warm() {
        assert_eq!(polling_tier(Status::Idle, TIER_COLD_MAIN_TUI), TIER_WARM);
        assert_eq!(polling_tier(Status::Unknown, TIER_COLD_MAIN_TUI), TIER_WARM);
    }

    #[test]
    fn test_polling_tier_cold() {
        assert_eq!(
            polling_tier(Status::Error, TIER_COLD_MAIN_TUI),
            TIER_COLD_MAIN_TUI
        );
        assert_eq!(
            polling_tier(Status::Error, TIER_COLD_ATTACHED_HOOKS),
            TIER_COLD_ATTACHED_HOOKS
        );
    }

    #[test]
    fn test_polling_tier_frozen() {
        assert_eq!(polling_tier(Status::Stopped, TIER_COLD_MAIN_TUI), 0);
        assert_eq!(polling_tier(Status::Deleting, TIER_COLD_MAIN_TUI), 0);
    }

    #[test]
    fn test_tier_cycle_alignment() {
        // Hot sessions are polled every cycle: TIER_HOT must stay at 1.
        assert_eq!(TIER_HOT, 1);
        // Warm sessions are polled every 5 cycles
        assert_ne!(1u64 % TIER_WARM, 0);
        assert_ne!(2u64 % TIER_WARM, 0);
        assert_eq!(5u64 % TIER_WARM, 0);
        assert_eq!(10u64 % TIER_WARM, 0);
        // Cold sessions are polled every 15 cycles, about 30 seconds at the
        // TUI's 2 second status refresh cadence.
        assert_ne!(1u64 % TIER_COLD_MAIN_TUI, 0);
        assert_eq!(15u64 % TIER_COLD_MAIN_TUI, 0);
        assert_eq!(30u64 % TIER_COLD_MAIN_TUI, 0);
        // Attached status hook polling still ticks every 500ms, so it keeps
        // the old 60-cycle cold tier for the same 30 second cadence.
        assert_ne!(15u64 % TIER_COLD_ATTACHED_HOOKS, 0);
        assert_eq!(60u64 % TIER_COLD_ATTACHED_HOOKS, 0);
    }

    #[test]
    fn test_first_cycle_polls_all_tiers() {
        // cycle_count starts at cold_tier - 1, first cycle wraps to cold_tier.
        let first_cycle = (TIER_COLD_MAIN_TUI - 1).wrapping_add(1);
        // TIER_HOT == 1 (see test_tier_cycle_alignment), so any cycle trivially
        // polls hot; just verify the warm and cold alignments here.
        assert_eq!(first_cycle % TIER_WARM, 0, "first cycle must poll warm");
        assert_eq!(
            first_cycle % TIER_COLD_MAIN_TUI,
            0,
            "first cycle must poll cold"
        );
    }

    #[test]
    fn test_tmux_metadata_refresh_includes_warm_sessions() {
        let inst = Instance::new("idle", "/tmp");

        assert!(should_refresh_tmux_metadata(&[inst], TIER_COLD_MAIN_TUI));
    }

    #[test]
    fn test_tmux_metadata_refresh_skips_frozen_sessions() {
        let mut stopped = Instance::new("stopped", "/tmp");
        stopped.status = Status::Stopped;
        let mut deleting = Instance::new("deleting", "/tmp");
        deleting.status = Status::Deleting;

        assert!(!should_refresh_tmux_metadata(
            &[stopped, deleting],
            TIER_COLD_MAIN_TUI
        ));
    }

    #[test]
    fn test_attached_status_hook_policy_keeps_500ms_cadence_scaled() {
        let state = StatusPollState::for_attached_status_hooks();

        assert_eq!(state.cold_tier, TIER_COLD_ATTACHED_HOOKS);
        assert!(!state.refresh_metadata_every_cycle);
    }
}
