//! Per-channel segment retention watermarks for safe cache pruning.
//!
//! Egress (DASH / MPEG-TS) and ingest (packager / stitcher) share this registry
//! so janitor and sliding-window deletes never remove segments clients may read.

use dashmap::DashMap;
use std::collections::HashSet;
use std::sync::Arc;

/// Shared retention state keyed by channel id.
#[derive(Clone, Default)]
pub struct RetentionRegistry {
    inner: Arc<DashMap<String, ChannelRetention>>,
    grace_segments: u64,
}

#[derive(Debug, Default)]
struct ChannelRetention {
    /// Lowest segment number touched by any egress reader.
    retain_from: u64,
    /// DASH MPD `startNumber` (lowest segment advertised in the live window).
    mpd_start: u64,
    /// Active MPEG-TS client cursors (each client's current read position).
    mpegts_cursors: HashSet<u64>,
}

impl RetentionRegistry {
    pub fn new(grace_segments: u64) -> Self {
        Self {
            inner: Arc::new(DashMap::new()),
            grace_segments: grace_segments.max(1),
        }
    }

    pub fn grace_segments(&self) -> u64 {
        self.grace_segments
    }

    /// Record that egress is serving (or about to serve) `seg_num`.
    pub fn touch_seg(&self, channel: &str, seg_num: u64) {
        let mut entry = self.inner.entry(channel.to_string()).or_default();
        if seg_num < entry.retain_from || entry.retain_from == 0 {
            entry.retain_from = seg_num;
        }
    }

    /// Update the lowest segment number advertised in the live DASH MPD.
    pub fn set_mpd_start(&self, channel: &str, start: u64) {
        let mut entry = self.inner.entry(channel.to_string()).or_default();
        entry.mpd_start = start;
    }

    /// Register an MPEG-TS client's cursor when it connects or advances.
    pub fn register_mpegts_cursor(&self, channel: &str, cursor: u64) {
        let mut entry = self.inner.entry(channel.to_string()).or_default();
        entry.mpegts_cursors.insert(cursor);
        if cursor < entry.retain_from || entry.retain_from == 0 {
            entry.retain_from = cursor;
        }
    }

    /// Remove an MPEG-TS client cursor on disconnect.
    pub fn unregister_mpegts_cursor(&self, channel: &str, cursor: u64) {
        if let Some(mut entry) = self.inner.get_mut(channel) {
            entry.mpegts_cursors.remove(&cursor);
            recompute_retain_from(&mut entry);
        }
    }

    /// Lowest segment number that may be deleted: everything below this is safe.
    ///
    /// Returns 1 when no retention is recorded (nothing to protect).
    pub fn safe_prune_floor(&self, channel: &str) -> u64 {
        let floor = self
            .inner
            .get(channel)
            .map(|e| {
                let mut min = u64::MAX;
                if e.retain_from > 0 {
                    min = min.min(e.retain_from);
                }
                if e.mpd_start > 0 {
                    min = min.min(e.mpd_start);
                }
                if let Some(&cursor) = e.mpegts_cursors.iter().min() {
                    min = min.min(cursor);
                }
                min
            })
            .unwrap_or(u64::MAX);

        if floor == u64::MAX {
            return 1;
        }
        floor.saturating_sub(self.grace_segments).max(1)
    }

    /// True when `seg_num` may be deleted by prune or janitor.
    pub fn may_delete_seg(&self, channel: &str, seg_num: u64) -> bool {
        seg_num < self.safe_prune_floor(channel)
    }

    /// Clear retention for a removed channel.
    pub fn remove_channel(&self, channel: &str) {
        self.inner.remove(channel);
    }
}

fn recompute_retain_from(entry: &mut ChannelRetention) {
    let mut min = u64::MAX;
    if let Some(&c) = entry.mpegts_cursors.iter().min() {
        min = min.min(c);
    }
    if entry.mpd_start > 0 {
        min = min.min(entry.mpd_start);
    }
    entry.retain_from = if min == u64::MAX { 0 } else { min };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_extends_retention_floor() {
        let reg = RetentionRegistry::new(2);
        reg.touch_seg("ch1", 10);
        assert_eq!(reg.safe_prune_floor("ch1"), 8);
        reg.touch_seg("ch1", 5);
        assert_eq!(reg.safe_prune_floor("ch1"), 3);
    }

    #[test]
    fn mpegts_cursor_protects_segments() {
        let reg = RetentionRegistry::new(2);
        reg.register_mpegts_cursor("ch1", 20);
        assert!(!reg.may_delete_seg("ch1", 19));
        assert!(reg.may_delete_seg("ch1", 17));
        reg.unregister_mpegts_cursor("ch1", 20);
        assert_eq!(reg.safe_prune_floor("ch1"), 1);
    }

    #[test]
    fn mpd_start_protects_advertised_segments() {
        let reg = RetentionRegistry::new(2);
        reg.set_mpd_start("ch1", 15);
        assert!(!reg.may_delete_seg("ch1", 14));
        assert!(reg.may_delete_seg("ch1", 12));
    }
}
