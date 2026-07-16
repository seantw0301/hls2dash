//! Live-edge cursor that streams cached continuous `.ts` segments over HTTP.
//!
//! Aligned with `trans_server` `/mpegts` behavior (holdback, lag jump, idle
//! timeout, generation reset → end stream). Segments are already stitched at
//! ingest — this path does **not** remux through CMAF.
//!
//! Egress is paced at ~1× media realtime (from `seg_N.dur` / EXTINF) so a
//! jitter buffer (`live_holdback` + `min_buffer`) absorbs delayed HLS arrivals
//! without bursting into the player.

use anyhow::{anyhow, Result};
use bytes::Bytes;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const DEFAULT_POLL: Duration = Duration::from_secs(2);
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const DEFAULT_SEG_DUR_SECS: f64 = 2.0;

/// Options for one `/mpegts` client stream.
#[derive(Debug, Clone)]
pub struct MpegTsStreamOpts {
    pub poll_interval_secs: u64,
    pub holdback_segments: u64,
    pub min_buffer_segments: u64,
    pub max_segment_lag: u64,
    pub send_queue: usize,
    pub pace_egress: bool,
    pub default_segment_duration_secs: f64,
}

impl Default for MpegTsStreamOpts {
    fn default() -> Self {
        Self {
            poll_interval_secs: 2,
            holdback_segments: 6,
            min_buffer_segments: 3,
            max_segment_lag: 10,
            send_queue: 32,
            pace_egress: true,
            default_segment_duration_secs: DEFAULT_SEG_DUR_SECS,
        }
    }
}

/// Streams multiplexed MPEG-TS bytes for one channel cache directory.
pub struct MpegTsStreamer {
    channel_id: String,
    channel_dir: PathBuf,
    poll: Duration,
    holdback: u64,
    min_buffer: u64,
    max_lag: u64,
    pace_egress: bool,
    default_dur: f64,
    next_number: Option<u64>,
    /// Generation marker: lowest segment number seen at join; rewind past this → reset.
    gen_floor: Option<u64>,
}

/// Spawns the streamer and returns a receiver of TS byte chunks.
pub fn spawn_mpegts_stream(
    channel_id: String,
    channel_dir: PathBuf,
    opts: MpegTsStreamOpts,
) -> mpsc::Receiver<Result<Bytes, std::io::Error>> {
    let (tx, rx) = mpsc::channel(opts.send_queue.max(1));
    let streamer = MpegTsStreamer::new(channel_id, channel_dir, opts);
    tokio::spawn(async move {
        streamer.run(tx).await;
    });
    rx
}

impl MpegTsStreamer {
    fn new(channel_id: String, channel_dir: PathBuf, opts: MpegTsStreamOpts) -> Self {
        Self {
            channel_id,
            channel_dir,
            poll: if opts.poll_interval_secs == 0 {
                DEFAULT_POLL
            } else {
                Duration::from_secs(opts.poll_interval_secs)
            },
            holdback: opts.holdback_segments,
            min_buffer: opts.min_buffer_segments.max(1),
            max_lag: opts.max_segment_lag.max(1),
            pace_egress: opts.pace_egress,
            default_dur: if opts.default_segment_duration_secs.is_finite()
                && opts.default_segment_duration_secs > 0.0
            {
                opts.default_segment_duration_secs
            } else {
                DEFAULT_SEG_DUR_SECS
            },
            next_number: None,
            gen_floor: None,
        }
    }

    async fn run(mut self, tx: mpsc::Sender<Result<Bytes, std::io::Error>>) {
        if !self.wait_for_min_buffer(&tx).await {
            return;
        }

        let mut last_progress = Instant::now();
        let mut pace_deadline: Option<Instant> = None;
        let mut underrun = false;

        loop {
            if tx.is_closed() {
                break;
            }

            match self.next_chunk().await {
                Ok(Some((chunk, dur_secs))) => {
                    if underrun {
                        // Source caught up after a stall — re-anchor pace so we
                        // do not burst multiple segments to "catch up".
                        pace_deadline = None;
                        underrun = false;
                        tracing::debug!(
                            channel = %self.channel_id,
                            "mpegts underrun recovered; re-anchoring pace"
                        );
                    }

                    if self.pace_egress {
                        if let Some(deadline) = pace_deadline {
                            let now = Instant::now();
                            if now < deadline {
                                tokio::select! {
                                    biased;
                                    _ = tx.closed() => return,
                                    _ = tokio::time::sleep(deadline.saturating_duration_since(now)) => {}
                                }
                            }
                        }
                    }

                    last_progress = Instant::now();
                    if chunk.is_empty() {
                        continue;
                    }
                    if tx.send(Ok(chunk)).await.is_err() {
                        break;
                    }

                    if self.pace_egress {
                        let d = Duration::from_secs_f64(dur_secs.clamp(0.1, 60.0));
                        pace_deadline = Some(Instant::now() + d);
                    }
                }
                Ok(None) => {
                    underrun = true;
                    if last_progress.elapsed() >= IDLE_TIMEOUT {
                        tracing::info!(
                            channel = %self.channel_id,
                            "mpegts stream idle timeout; flushing"
                        );
                        break;
                    }
                    tokio::time::sleep(self.poll).await;
                }
                Err(err) => {
                    tracing::warn!(
                        channel = %self.channel_id,
                        error = %err,
                        "mpegts stream failed"
                    );
                    let _ = tx.send(Err(std::io::Error::other(err.to_string()))).await;
                    break;
                }
            }
        }
    }

    /// Block until `min_buffer` segments exist (or client disconnects / idle).
    async fn wait_for_min_buffer(&self, tx: &mpsc::Sender<Result<Bytes, std::io::Error>>) -> bool {
        let started = Instant::now();
        loop {
            if tx.is_closed() {
                return false;
            }
            match scan_ts_segments(&self.channel_dir) {
                Ok(segs) if (segs.len() as u64) >= self.min_buffer => {
                    tracing::info!(
                        channel = %self.channel_id,
                        buffered = segs.len(),
                        min_buffer = self.min_buffer,
                        "mpegts buffer ready; starting paced egress"
                    );
                    return true;
                }
                Ok(_) | Err(_) => {}
            }
            if started.elapsed() >= IDLE_TIMEOUT {
                tracing::warn!(
                    channel = %self.channel_id,
                    min_buffer = self.min_buffer,
                    "mpegts buffer wait timed out"
                );
                let _ = tx
                    .send(Err(std::io::Error::other(
                        "mpegts buffer not ready (min_buffer timeout)",
                    )))
                    .await;
                return false;
            }
            tokio::time::sleep(self.poll).await;
        }
    }

    async fn next_chunk(&mut self) -> Result<Option<(Bytes, f64)>> {
        let segments = scan_ts_segments(&self.channel_dir)?;
        if segments.is_empty() {
            return Ok(None);
        }

        let live_edge = *segments.keys().next_back().unwrap_or(&0);
        let floor = *segments.keys().next().unwrap_or(&0);
        if self.gen_floor.is_none() {
            self.gen_floor = Some(floor);
        }

        // True generation wipe: live edge fell below the floor we joined with.
        if let Some(gen_floor) = self.gen_floor {
            if live_edge < gen_floor {
                return Err(anyhow!("ts cache generation changed; reconnect"));
            }
        }

        let (next, jump) =
            resolve_cursor(self.next_number, live_edge, floor, self.holdback, self.max_lag);
        match jump {
            CursorJump::Forward => {
                tracing::debug!(
                    channel = %self.channel_id,
                    cursor = self.next_number.unwrap_or(0),
                    live_edge,
                    floor,
                    holdback = self.holdback,
                    "mpegts client lagging; jumping to safe live edge"
                );
            }
            CursorJump::Rewind => {
                tracing::info!(
                    channel = %self.channel_id,
                    cursor = self.next_number.unwrap_or(0),
                    live_edge,
                    "upstream sequence reset detected; ending mpegts stream for client reconnect"
                );
                return Err(anyhow!(
                    "upstream generation reset (cursor={} live_edge={})",
                    self.next_number.unwrap_or(0),
                    live_edge
                ));
            }
            CursorJump::None => {}
        }

        self.next_number = Some(next);
        let Some(path) = segments.get(&next) else {
            // Behind pruned window → snap to safe edge on next poll.
            if next < floor {
                let safe = live_edge.saturating_sub(self.holdback).max(floor);
                self.next_number = Some(safe);
            }
            return Ok(None);
        };

        let bytes = tokio::fs::read(path)
            .await
            .map_err(|e| anyhow!("read {}: {e}", path.display()))?;
        if bytes.is_empty() || bytes.len() % 188 != 0 || bytes[0] != 0x47 {
            return Err(anyhow!("invalid TS segment {}", path.display()));
        }

        let dur = read_segment_duration(path, self.default_dur).await;
        self.next_number = Some(next.saturating_add(1));
        Ok(Some((Bytes::from(bytes), dur)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CursorJump {
    None,
    Forward,
    Rewind,
}

fn resolve_cursor(
    current: Option<u64>,
    live_edge: u64,
    floor: u64,
    holdback: u64,
    max_lag: u64,
) -> (u64, CursorJump) {
    let max_lag = max_lag.max(1);
    // Never aim below the oldest cached segment (holdback can exceed buffer depth).
    let safe_edge = live_edge.saturating_sub(holdback).max(floor);
    match current {
        Some(n) if safe_edge > n.saturating_add(max_lag) => (safe_edge, CursorJump::Forward),
        Some(n) if n > live_edge.saturating_add(max_lag) => (safe_edge, CursorJump::Rewind),
        Some(n) if n < floor => (safe_edge, CursorJump::Forward),
        Some(n) => (n, CursorJump::None),
        None => (safe_edge, CursorJump::None),
    }
}

fn scan_ts_segments(dir: &Path) -> Result<BTreeMap<u64, PathBuf>> {
    let mut segments = BTreeMap::new();
    if !dir.is_dir() {
        return Ok(segments);
    }
    let entries = std::fs::read_dir(dir).map_err(|e| anyhow!("read_dir {}: {e}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if let Some(num) = parse_ts_seg_number(name) {
            segments.insert(num, path);
        }
    }
    Ok(segments)
}

fn parse_ts_seg_number(name: &str) -> Option<u64> {
    let rest = name.strip_prefix("seg_")?.strip_suffix(".ts")?;
    rest.parse().ok()
}

async fn read_segment_duration(ts_path: &Path, fallback: f64) -> f64 {
    let dur_path = ts_path.with_extension("dur");
    match tokio::fs::read_to_string(&dur_path).await {
        Ok(s) => s
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|d| d.is_finite() && *d > 0.0)
            .unwrap_or(fallback),
        Err(_) => fallback,
    }
}

/// True when channel cache has at least `min_buffer` continuous TS segments.
pub fn channel_mpegts_ready(channel_dir: &Path, min_buffer: u64) -> bool {
    let need = min_buffer.max(1);
    match scan_ts_segments(channel_dir) {
        Ok(s) => (s.len() as u64) >= need,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_holds_within_lag_window() {
        assert_eq!(
            resolve_cursor(Some(10), 16, 1, 4, 10),
            (10, CursorJump::None)
        );
        assert_eq!(resolve_cursor(None, 16, 1, 4, 10), (12, CursorJump::None));
    }

    #[test]
    fn cursor_jumps_forward_to_safe_edge() {
        assert_eq!(
            resolve_cursor(Some(10), 40, 1, 4, 10),
            (36, CursorJump::Forward)
        );
    }

    #[test]
    fn cursor_rewinds_after_sequence_reset() {
        assert_eq!(
            resolve_cursor(Some(501), 3, 1, 0, 10),
            (3, CursorJump::Rewind)
        );
    }

    #[test]
    fn cursor_clamps_safe_edge_to_floor_when_holdback_exceeds_depth() {
        // live=4, holdback=6 → raw safe would be 0; clamp to floor=1
        assert_eq!(resolve_cursor(None, 4, 1, 6, 10), (1, CursorJump::None));
    }
}
