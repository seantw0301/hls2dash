//! HLS segment seq + content fingerprint deduplication.
//!
//! Detects playlist rewind, duplicate payloads, and rewritten segments so
//! egress does not replay stale media.

use std::collections::hash_map::DefaultHasher;
use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use tracing::warn;

const FINGERPRINT_HEAD_BYTES: usize = 32 * 1024;
/// Large backward jump in MEDIA-SEQUENCE is treated as a playlist reset.
const SEQUENCE_RESET_THRESHOLD: u64 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeqOrderAction {
    /// `seq` advanced normally.
    Advance,
    /// `seq` moved backward within the live window — skip to avoid replay.
    SkipRewind,
    /// `seq` jumped far backward — origin likely reset MEDIA-SEQUENCE.
    SequenceReset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentDedupAction {
    /// Ingest this segment.
    Accept,
    /// Identical `seq` + payload already ingested.
    SkipDuplicate,
    /// Payload matches a different, older `seq` (replay).
    SkipReplay,
    /// Same `seq` arrived again with different bytes (origin rewrite).
    ResetOutput,
}

/// Rolling window of recent `(MEDIA-SEQUENCE, fingerprint)` pairs.
#[derive(Debug)]
pub struct SegmentDedup {
    recent: VecDeque<(u64, u64)>,
    capacity: usize,
}

impl SegmentDedup {
    pub fn new(capacity: usize) -> Self {
        Self {
            recent: VecDeque::new(),
            capacity: capacity.max(16),
        }
    }

    pub fn clear(&mut self) {
        self.recent.clear();
    }

    /// Check monotonicity before fetching segment bytes.
    pub fn check_seq_order(last_seq: Option<u64>, seq: u64) -> SeqOrderAction {
        let Some(last) = last_seq else {
            return SeqOrderAction::Advance;
        };
        if seq > last {
            return SeqOrderAction::Advance;
        }
        if seq == last {
            return SeqOrderAction::SkipRewind;
        }
        if last.saturating_sub(seq) > SEQUENCE_RESET_THRESHOLD {
            SeqOrderAction::SequenceReset
        } else {
            SeqOrderAction::SkipRewind
        }
    }

    /// Check payload fingerprint after a successful fetch.
    pub fn check_content(
        &mut self,
        channel: &str,
        seq: u64,
        bytes: &[u8],
    ) -> ContentDedupAction {
        let fingerprint = segment_fingerprint(bytes);

        if self
            .recent
            .iter()
            .any(|&(s, fp)| s == seq && fp == fingerprint)
        {
            warn!(channel, seq, "duplicate HLS segment (same seq + content); skipping");
            return ContentDedupAction::SkipDuplicate;
        }

        if let Some((_, old_fp)) = self.recent.iter().find(|&&(s, _)| s == seq) {
            if *old_fp != fingerprint {
                warn!(
                    channel,
                    seq,
                    "HLS segment rewritten at same MEDIA-SEQUENCE; resetting output"
                );
                return ContentDedupAction::ResetOutput;
            }
        }

        if let Some((matched_seq, _)) = self
            .recent
            .iter()
            .find(|&&(s, fp)| s != seq && fp == fingerprint)
        {
            warn!(
                channel,
                seq,
                matched_seq,
                "duplicate HLS segment content (replay); skipping"
            );
            return ContentDedupAction::SkipReplay;
        }

        self.record(seq, fingerprint);
        ContentDedupAction::Accept
    }

    fn record(&mut self, seq: u64, fingerprint: u64) {
        self.recent.push_back((seq, fingerprint));
        while self.recent.len() > self.capacity {
            self.recent.pop_front();
        }
    }
}

fn segment_fingerprint(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.len().hash(&mut hasher);
    let head_len = bytes.len().min(FINGERPRINT_HEAD_BYTES);
    bytes[..head_len].hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seq_order_detects_rewind_and_reset() {
        assert_eq!(
            SegmentDedup::check_seq_order(None, 10),
            SeqOrderAction::Advance
        );
        assert_eq!(
            SegmentDedup::check_seq_order(Some(10), 11),
            SeqOrderAction::Advance
        );
        assert_eq!(
            SegmentDedup::check_seq_order(Some(10), 9),
            SeqOrderAction::SkipRewind
        );
        assert_eq!(
            SegmentDedup::check_seq_order(Some(200), 5),
            SeqOrderAction::SequenceReset
        );
    }

    #[test]
    fn content_dedup_skips_replay() {
        let mut dedup = SegmentDedup::new(8);
        let a = b"segment-payload-a";
        let b = b"segment-payload-b";
        assert_eq!(
            dedup.check_content("ch", 1, a),
            ContentDedupAction::Accept
        );
        assert_eq!(
            dedup.check_content("ch", 2, a),
            ContentDedupAction::SkipReplay
        );
        assert_eq!(
            dedup.check_content("ch", 1, a),
            ContentDedupAction::SkipDuplicate
        );
        assert_eq!(
            dedup.check_content("ch", 1, b),
            ContentDedupAction::ResetOutput
        );
    }
}
