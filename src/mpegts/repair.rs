//! MPEG-TS packet repair: resync on sync-byte loss instead of truncating.

use anyhow::{bail, Result};

pub const TS_PACKET_SIZE: usize = 188;

/// Result of repairing a TS byte blob.
#[derive(Debug, Clone, Copy, Default)]
pub struct RepairStats {
    pub packets_kept: usize,
    pub packets_dropped: usize,
    pub resync_count: usize,
}

/// Align to the first sync byte and rescan forward on mid-stream sync loss.
///
/// Unlike truncate-on-error, this skips corrupted packet regions and continues
/// with the next valid 0x47 at a 188-byte boundary when possible.
pub fn repair_ts_packets(raw: &[u8]) -> Result<(Vec<u8>, RepairStats)> {
    let start = raw
        .iter()
        .position(|&b| b == 0x47)
        .ok_or_else(|| anyhow::anyhow!("no MPEG-TS sync byte 0x47 in segment"))?;

    let mut stats = RepairStats::default();
    let mut out = Vec::new();
    let mut i = start;

    while i + TS_PACKET_SIZE <= raw.len() {
        if raw[i] == 0x47 {
            out.extend_from_slice(&raw[i..i + TS_PACKET_SIZE]);
            stats.packets_kept += 1;
            i += TS_PACKET_SIZE;
            continue;
        }

        stats.resync_count += 1;
        stats.packets_dropped += 1;
        let mut found = false;
        let search_from = i + 1;
        let search_to = raw.len().saturating_sub(TS_PACKET_SIZE);
        let mut j = search_from;
        while j <= search_to {
            if raw[j] == 0x47 && (j - start) % TS_PACKET_SIZE == 0 {
                i = j;
                found = true;
                break;
            }
            j += 1;
        }
        if !found {
            break;
        }
    }

    if out.is_empty() {
        bail!("no valid TS packets after sync check");
    }
    Ok((out, stats))
}

/// Legacy align-only path (truncate at first bad packet). Used when resync is disabled.
pub fn align_ts_packets_truncate(raw: &[u8]) -> Result<Vec<u8>> {
    let start = raw
        .iter()
        .position(|&b| b == 0x47)
        .ok_or_else(|| anyhow::anyhow!("no MPEG-TS sync byte 0x47 in segment"))?;
    let aligned = &raw[start..];
    let n = aligned.len() / TS_PACKET_SIZE;
    if n == 0 {
        bail!("segment shorter than one TS packet");
    }
    let mut out = aligned[..n * TS_PACKET_SIZE].to_vec();
    let mut i = 0;
    while i + TS_PACKET_SIZE <= out.len() {
        if out[i] != 0x47 {
            out.truncate(i);
            break;
        }
        i += TS_PACKET_SIZE;
    }
    if out.is_empty() {
        bail!("no valid TS packets after sync check");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet() -> [u8; 188] {
        let mut p = [0u8; 188];
        p[0] = 0x47;
        p
    }

    #[test]
    fn repair_resyncs_after_garbage_run() {
        let mut raw = vec![0x00, 0x01];
        raw.extend_from_slice(&packet());
        raw.extend_from_slice(&packet());
        // Corrupt one packet slot then resume sync at boundary.
        raw.extend_from_slice(&[0xff; 188]);
        raw.extend_from_slice(&packet());
        let (out, stats) = repair_ts_packets(&raw).unwrap();
        assert_eq!(out.len(), 188 * 3);
        assert!(stats.resync_count >= 1);
        assert_eq!(out[0], 0x47);
    }

    #[test]
    fn repair_finds_initial_sync() {
        let mut raw = vec![0x00];
        raw.extend_from_slice(&packet());
        let (out, _) = repair_ts_packets(&raw).unwrap();
        assert_eq!(out.len(), 188);
    }
}
