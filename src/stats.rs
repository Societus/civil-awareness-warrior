//! Pollable progress handle for embedders (GUI / tray apps).
//!
//! The runner updates a [`StatsHandle`] as the loop progresses; the host
//! application polls [`StatsHandle::snapshot`] on its own schedule (UI tick,
//! status file, whatever) — no channels to drive, no async needed on the
//! polling side.

use crate::api::ChunkType;
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// What one worker slot is doing right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum SlotActivity {
    /// Polling the coordinator or sleeping off a backoff (no lease held).
    Polling,
    /// Lease claimed; ed25519 contract verification in progress.
    Verifying {
        chunk_id: String,
        chunk_type: ChunkType,
    },
    /// Contract verified; the type-specific executor is running.
    Executing {
        chunk_id: String,
        chunk_type: ChunkType,
    },
    /// Uploading blobs / submitting the result or failure report.
    Submitting { chunk_id: String },
    /// Slot wound down (shutdown received or `--once` settled).
    Stopped,
}

/// Point-in-time view of the runner's progress, for GUI polling.
#[derive(Debug, Clone, Serialize)]
pub struct StatsSnapshot {
    /// Unix ms when the runner was constructed.
    pub started_at_ms: i64,
    /// Wall time since construction.
    pub uptime: Duration,
    /// Chunks whose result was accepted for coordinator-side verification
    /// (`POST /v1/result` returned a receipt).
    pub chunks_completed: u64,
    /// Chunks reported failed via `POST /v1/fail` (including dry runs, which
    /// report nothing — those settle without counting here).
    pub chunks_failed: u64,
    /// Leases whose ed25519 contract verified OK — i.e. chunks actually
    /// executed. A chunk counts here before it can count as completed or
    /// failed; a rejected signature never does.
    pub chunks_verified: u64,
    /// Stored (compressed) bytes successfully pushed through the stager.
    pub bytes_staged: u64,
    /// Per-slot activity; index = `slot_id` (0..concurrency).
    pub slots: Vec<SlotActivity>,
}

#[derive(Debug)]
struct StatsShared {
    started_at_ms: i64,
    started: Instant,
    chunks_completed: AtomicU64,
    chunks_failed: AtomicU64,
    chunks_verified: AtomicU64,
    bytes_staged: AtomicU64,
    slots: RwLock<Vec<SlotActivity>>,
}

/// Clone-able handle onto the runner's live counters. The runner updates it;
/// embedders clone it and poll [`StatsHandle::snapshot`].
#[derive(Debug, Clone)]
pub struct StatsHandle {
    shared: Arc<StatsShared>,
}

impl StatsHandle {
    pub(crate) fn new(slot_count: usize) -> StatsHandle {
        StatsHandle {
            shared: Arc::new(StatsShared {
                started_at_ms: crate::api::unix_ms(),
                started: Instant::now(),
                chunks_completed: AtomicU64::new(0),
                chunks_failed: AtomicU64::new(0),
                chunks_verified: AtomicU64::new(0),
                bytes_staged: AtomicU64::new(0),
                slots: RwLock::new(vec![SlotActivity::Polling; slot_count]),
            }),
        }
    }

    /// Take a consistent-ish snapshot: counters are read atomically one by
    /// one (no cross-counter lock), which is exactly right for display.
    pub fn snapshot(&self) -> StatsSnapshot {
        let slots = self.shared.slots.read().expect("stats slot lock").clone();
        StatsSnapshot {
            started_at_ms: self.shared.started_at_ms,
            uptime: self.shared.started.elapsed(),
            chunks_completed: self.shared.chunks_completed.load(Ordering::Relaxed),
            chunks_failed: self.shared.chunks_failed.load(Ordering::Relaxed),
            chunks_verified: self.shared.chunks_verified.load(Ordering::Relaxed),
            bytes_staged: self.shared.bytes_staged.load(Ordering::Relaxed),
            slots,
        }
    }

    pub(crate) fn record_completed(&self) {
        self.shared.chunks_completed.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_failed(&self) {
        self.shared.chunks_failed.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_verified(&self) {
        self.shared.chunks_verified.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_bytes_staged(&self, stored_len: u64) {
        self.shared
            .bytes_staged
            .fetch_add(stored_len, Ordering::Relaxed);
    }

    pub(crate) fn set_slot(&self, slot: usize, activity: SlotActivity) {
        if let Some(entry) = self
            .shared
            .slots
            .write()
            .expect("stats slot lock")
            .get_mut(slot)
        {
            *entry = activity;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_starts_at_zero_with_polling_slots() {
        let stats = StatsHandle::new(3);
        let snap = stats.snapshot();
        assert_eq!(snap.chunks_completed, 0);
        assert_eq!(snap.chunks_failed, 0);
        assert_eq!(snap.chunks_verified, 0);
        assert_eq!(snap.bytes_staged, 0);
        assert_eq!(
            snap.slots,
            vec![
                SlotActivity::Polling,
                SlotActivity::Polling,
                SlotActivity::Polling
            ]
        );
        assert!(snap.started_at_ms > 0);
    }

    #[test]
    fn counters_accumulate_and_slots_update() {
        let stats = StatsHandle::new(2);
        stats.record_verified();
        stats.record_verified();
        stats.record_completed();
        stats.record_failed();
        stats.record_bytes_staged(100);
        stats.record_bytes_staged(23);
        stats.set_slot(
            1,
            SlotActivity::Executing {
                chunk_id: "c1".to_string(),
                chunk_type: ChunkType::Ingest,
            },
        );
        let snap = stats.snapshot();
        assert_eq!(snap.chunks_verified, 2);
        assert_eq!(snap.chunks_completed, 1);
        assert_eq!(snap.chunks_failed, 1);
        assert_eq!(snap.bytes_staged, 123);
        assert_eq!(snap.slots[0], SlotActivity::Polling);
        assert_eq!(
            snap.slots[1],
            SlotActivity::Executing {
                chunk_id: "c1".to_string(),
                chunk_type: ChunkType::Ingest
            }
        );
        // snapshot is a copy: mutating after doesn't retroactively change it
        stats.set_slot(0, SlotActivity::Stopped);
        assert_eq!(snap.slots[0], SlotActivity::Polling);
    }

    #[test]
    fn out_of_range_slot_is_ignored_and_uptime_monotonic() {
        let stats = StatsHandle::new(1);
        stats.set_slot(7, SlotActivity::Stopped); // no panic
        assert_eq!(stats.snapshot().slots.len(), 1);
        let first = stats.snapshot().uptime;
        std::thread::sleep(Duration::from_millis(2));
        assert!(stats.snapshot().uptime >= first);
    }

    #[test]
    fn snapshot_serializes_for_gui_json_dumps() {
        let stats = StatsHandle::new(1);
        let json = serde_json::to_string(&stats.snapshot()).expect("serialize");
        assert!(json.contains("\"chunks_completed\""));
        assert!(json.contains("\"Polling\""));
    }
}
