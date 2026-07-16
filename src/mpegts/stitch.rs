//! Stitch HLS MPEG-TS segments into a continuous live TS cache.

use super::continuous::ContinuityState;
use super::repair::{align_ts_packets_truncate, repair_ts_packets};
use crate::cache::RetentionRegistry;
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Writes continuous MPEG-TS segments under `out_dir` (`seg_1.ts`, …).
pub struct TsStitcher {
    out_dir: PathBuf,
    channel_id: String,
    retention: Arc<RetentionRegistry>,
    ts_resync: bool,
    window_segments: usize,
    continuity: ContinuityState,
    next_segment_number: u64,
    window_start: u64,
}

impl TsStitcher {
    /// Resume numbering after wipe of leftover media (fresh session).
    pub fn resume(
        out_dir: PathBuf,
        window_segments: usize,
        channel_id: &str,
        retention: Arc<RetentionRegistry>,
        ts_resync: bool,
    ) -> Result<Self> {
        fs::create_dir_all(&out_dir)
            .with_context(|| format!("create channel dir {}", out_dir.display()))?;
        let next = scan_next_ts_number(&out_dir);
        wipe_ts_media(&out_dir)?;
        info!(
            dir = %out_dir.display(),
            next_segment = next,
            "TS stitcher resume: wiped prior media, continuing numbering"
        );
        Ok(Self {
            out_dir,
            channel_id: channel_id.to_string(),
            retention,
            ts_resync,
            window_segments: window_segments.max(1),
            continuity: ContinuityState::default(),
            next_segment_number: next,
            window_start: next,
        })
    }

    /// Reset continuity + wipe media (HLS discontinuity / reconnect).
    pub fn prepare_for_reconnect(&mut self) -> Result<()> {
        wipe_ts_media(&self.out_dir)?;
        self.continuity = ContinuityState::default();
        self.window_start = self.next_segment_number;
        info!(
            dir = %self.out_dir.display(),
            next_segment = self.next_segment_number,
            "TS stitcher prepare_for_reconnect"
        );
        Ok(())
    }

    /// Advance segment numbering without writing `.ts` (missing/corrupt HLS segment).
    pub fn skip_segment(&mut self, duration_secs: f64) -> Result<u64> {
        let number = self.next_segment_number;
        let dur = if duration_secs.is_finite() && duration_secs > 0.0 {
            duration_secs
        } else {
            2.0
        };
        let _ = fs::write(
            self.out_dir.join(format!("seg_{number}.dur")),
            format!("{dur}"),
        );
        self.next_segment_number = number.saturating_add(1);
        self.prune_window();
        warn!(number, duration_secs = dur, "skipped TS segment (no media file)");
        Ok(number)
    }

    /// Ingest one HLS `.ts` segment: align packets, rewrite CC, write `seg_N.ts` + duration.
    pub fn push_segment(&mut self, raw: &[u8], duration_secs: f64) -> Result<u64> {
        let ts = self.prepare_ts(raw)?;
        if ts.is_empty() {
            bail!("empty TS segment after alignment");
        }
        let mut ts = ts;
        self.continuity.rewrite(&mut ts);

        let number = self.next_segment_number;
        let name = format!("seg_{number}.ts");
        atomic_write(&self.out_dir.join(&name), &ts)?;
        let dur = if duration_secs.is_finite() && duration_secs > 0.0 {
            duration_secs
        } else {
            2.0
        };
        let _ = fs::write(
            self.out_dir.join(format!("seg_{number}.dur")),
            format!("{dur}"),
        );
        self.next_segment_number = number.saturating_add(1);
        self.prune_window();
        debug!(number, bytes = ts.len(), duration_secs = dur, "wrote continuous TS segment");
        Ok(number)
    }

    fn prepare_ts(&self, raw: &[u8]) -> Result<Vec<u8>> {
        if self.ts_resync {
            let (ts, stats) = repair_ts_packets(raw)?;
            if stats.packets_dropped > 0 || stats.resync_count > 0 {
                warn!(
                    packets_kept = stats.packets_kept,
                    packets_dropped = stats.packets_dropped,
                    resync_count = stats.resync_count,
                    "TS packet repair applied"
                );
            }
            Ok(ts)
        } else {
            align_ts_packets_truncate(raw)
        }
    }
}

fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, data)?;
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = fs::remove_file(&tmp);
            Err(err.into())
        }
    }
}

fn wipe_ts_media(out_dir: &Path) -> Result<()> {
    if !out_dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(out_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with("seg_")
            && (name.ends_with(".ts") || name.ends_with(".dur") || name.ends_with(".tmp"))
        {
            let _ = fs::remove_file(&path);
        }
        if name == "init.mp4"
            || name == "index.mpd"
            || (name.starts_with("seg_") && name.ends_with(".m4s"))
        {
            let _ = fs::remove_file(&path);
        }
    }
    Ok(())
}

fn scan_next_ts_number(out_dir: &Path) -> u64 {
    let mut next = 1u64;
    let Ok(entries) = fs::read_dir(out_dir) else {
        return next;
    };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if let Some(n) = parse_ts_seg_number(&name) {
            next = next.max(n.saturating_add(1));
        }
    }
    next
}

fn parse_ts_seg_number(name: &str) -> Option<u64> {
    let rest = name.strip_prefix("seg_")?.strip_suffix(".ts")?;
    rest.parse().ok()
}

impl TsStitcher {
    fn prune_window(&mut self) {
        let keep = self.window_segments as u64;
        let floor = self.retention.safe_prune_floor(&self.channel_id);
        while self.next_segment_number.saturating_sub(self.window_start) > keep + 2 {
            let old = self.window_start;
            if old >= floor {
                break;
            }
            let _ = fs::remove_file(self.out_dir.join(format!("seg_{old}.ts")));
            let _ = fs::remove_file(self.out_dir.join(format!("seg_{old}.dur")));
            self.window_start = old.saturating_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::RetentionRegistry;

    #[test]
    fn prune_respects_retention_floor() {
        let dir = tempfile::tempdir().unwrap();
        let retention = Arc::new(RetentionRegistry::new(2));
        retention.touch_seg("ch1", 5);
        let mut stitcher = TsStitcher {
            out_dir: dir.path().to_path_buf(),
            channel_id: "ch1".into(),
            retention,
            ts_resync: true,
            window_segments: 2,
            continuity: ContinuityState::default(),
            next_segment_number: 10,
            window_start: 1,
        };
        stitcher.prune_window();
        assert_eq!(stitcher.window_start, 3);
    }
}
