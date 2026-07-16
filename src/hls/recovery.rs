//! HLS ingest recovery helpers: segment fetch retry and sequence-gap detection.

use crate::config::RecoveryConfig;
use anyhow::Result;
use reqwest::Client;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::warn;

/// Fetch segment bytes with per-segment retries.
pub async fn fetch_segment_bytes(
    client: &Client,
    cancel: &CancellationToken,
    url: &str,
    seq: u64,
    recovery: &RecoveryConfig,
) -> Result<Vec<u8>> {
    let retries = recovery.segment_fetch_retries;
    let retry_delay = Duration::from_millis(recovery.segment_fetch_retry_ms.max(50));
    let mut last_err = None;

    for attempt in 0..=retries {
        if cancel.is_cancelled() {
            anyhow::bail!("cancelled");
        }
        match crate::hls::playlist::fetch_bytes(client, url).await {
            Ok(bytes) => return Ok(bytes),
            Err(err) => {
                last_err = Some(err);
                if attempt < retries {
                    warn!(
                        seq,
                        attempt = attempt + 1,
                        max = retries,
                        "segment fetch failed; retrying"
                    );
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => anyhow::bail!("cancelled"),
                        _ = tokio::time::sleep(retry_delay) => {}
                    }
                }
            }
        }
    }
    Err(last_err.unwrap())
}

/// Log when MEDIA-SEQUENCE jumps (missing segments in the playlist).
pub fn detect_sequence_gap(channel: &str, last_seq: Option<u64>, seq: u64) -> bool {
    let Some(last) = last_seq else {
        return false;
    };
    if seq > last.saturating_add(1) {
        let gap = seq - last - 1;
        warn!(
            channel,
            last_seq = last,
            seq,
            missing = gap,
            "HLS MEDIA-SEQUENCE gap detected"
        );
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_gap_when_sequence_jumps() {
        assert!(!detect_sequence_gap("ch", None, 5));
        assert!(!detect_sequence_gap("ch", Some(5), 6));
        assert!(detect_sequence_gap("ch", Some(5), 8));
    }
}
