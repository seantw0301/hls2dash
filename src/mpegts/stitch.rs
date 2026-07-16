//! Stitch HLS MPEG-TS segments into a continuous live TS cache.
//!
//! Origin `.ts` segments usually restart continuity counters each cut. This
//! module rewrites CC across segments and writes numbered `seg_N.ts` files for
//! the `/live/<channel>/mpegts` HTTP streamer — **no CMAF remux**.

use super::continuous::ContinuityState;
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{debug, info};

const TS_PACKET_SIZE: usize = 188;

/// Writes continuous MPEG-TS segments under `out_dir` (`seg_1.ts`, …).
pub struct TsStitcher {
    out_dir: PathBuf,
    window_segments: usize,
    continuity: ContinuityState,
    next_segment_number: u64,
    window_start: u64,
}

impl TsStitcher {
    /// Resume numbering after wipe of leftover media (fresh session).
    pub fn resume(out_dir: PathBuf, window_segments: usize) -> Result<Self> {
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

    /// Ingest one HLS `.ts` segment: align packets, rewrite CC, write `seg_N.ts` + duration.
    pub fn push_segment(&mut self, raw: &[u8], duration_secs: f64) -> Result<u64> {
        let mut ts = align_ts_packets(raw)?;
        if ts.is_empty() {
            bail!("empty TS segment after alignment");
        }
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
}

fn align_ts_packets(raw: &[u8]) -> Result<Vec<u8>> {
    // Find first sync byte.
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
    // Drop packets that lost sync mid-stream.
    let mut i = 0;
    while i + TS_PACKET_SIZE <= out.len() {
        if out[i] != 0x47 {
            // Truncate at first bad packet boundary.
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
        // Leftover CMAF from a previous dash-mode run.
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
        while self.next_segment_number.saturating_sub(self.window_start) > keep + 2 {
            let old = self.window_start;
            let _ = fs::remove_file(self.out_dir.join(format!("seg_{old}.ts")));
            let _ = fs::remove_file(self.out_dir.join(format!("seg_{old}.dur")));
            self.window_start = old.saturating_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align_finds_sync_and_truncates() {
        let mut raw = vec![0x00, 0x01];
        raw.extend_from_slice(&[0x47u8; 188]);
        raw.extend_from_slice(&[0x47u8; 188]);
        raw.push(0xff); // trailing junk
        let out = align_ts_packets(&raw).unwrap();
        assert_eq!(out.len(), 376);
        assert_eq!(out[0], 0x47);
    }
}
