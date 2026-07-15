//! MPEG-TS → [`AccessUnit`] bridge for the CMAF packager.

use super::types::{AccessUnit, TIMESCALE};
use anyhow::Result;
use tracing::warn;
use transmux::{CodecConfig, DemuxEvent, Sample, StreamingTsDemux};

/// Media timescale used by TS video access units (90 kHz).
const VIDEO_TS_TIMESCALE: u32 = 90_000;

/// Incremental MPEG-TS demux that emits packager-ready access units.
///
/// Sample timing is rescaled to [`TIMESCALE`] (milliseconds) so
/// [`crate::dash::DashPackager`] can keep a single movie timescale.
#[derive(Default)]
pub struct TsDemuxBridge {
    inner: StreamingTsDemux,
    video_pid_track: Option<u32>,
    audio_pid_track: Option<u32>,
    audio_sample_rate: u32,
    saw_discontinuity: bool,
}

impl TsDemuxBridge {
    /// Create an empty demuxer.
    pub fn new() -> Self {
        Self::default()
    }

    /// True if a TS discontinuity indicator was seen since last clear.
    pub fn take_discontinuity(&mut self) -> bool {
        let v = self.saw_discontinuity;
        self.saw_discontinuity = false;
        v
    }

    /// Feed arbitrary MPEG-TS bytes and drain completed access units.
    pub fn feed(&mut self, data: &[u8]) -> Result<Vec<AccessUnit>> {
        self.inner.feed(data);
        let mut out = Vec::new();
        while let Some(ev) = self.inner.poll_event() {
            self.handle_event(ev, &mut out);
        }
        Ok(out)
    }

    /// Flush trailing partial access units at end of a `.ts` segment / session.
    pub fn finish_segment(&mut self) -> Result<Vec<AccessUnit>> {
        self.inner.finish();
        let mut out = Vec::new();
        while let Some(ev) = self.inner.poll_event() {
            self.handle_event(ev, &mut out);
        }
        Ok(out)
    }

    fn handle_event(&mut self, ev: DemuxEvent, out: &mut Vec<AccessUnit>) {
        match ev {
            DemuxEvent::TrackAdded(track) => match &track.spec.config {
                CodecConfig::Avc { .. } => {
                    self.video_pid_track = Some(track.spec.track_id);
                    out.push(AccessUnit::VideoConfig {
                        config: track.spec.config.clone(),
                    });
                }
                CodecConfig::Aac { sample_rate, .. } => {
                    self.audio_pid_track = Some(track.spec.track_id);
                    self.audio_sample_rate = (*sample_rate).max(1);
                    out.push(AccessUnit::AudioConfig {
                        config: track.spec.config.clone(),
                    });
                }
                _ => {
                    warn!(
                        track_id = track.spec.track_id,
                        "skipping unsupported TS track"
                    );
                }
            },
            DemuxEvent::Sample { track_id, sample } => {
                if Some(track_id) == self.video_pid_track {
                    out.push(AccessUnit::VideoSample(rescale_sample(
                        sample,
                        VIDEO_TS_TIMESCALE,
                        TIMESCALE,
                    )));
                } else if Some(track_id) == self.audio_pid_track {
                    let from = self.audio_sample_rate.max(1);
                    out.push(AccessUnit::AudioSample(rescale_sample(
                        sample, from, TIMESCALE,
                    )));
                }
            }
            DemuxEvent::Discontinuity { .. } => {
                self.saw_discontinuity = true;
            }
            DemuxEvent::TrackUpdated(_)
            | DemuxEvent::Pcr(_)
            | DemuxEvent::TracksResolved => {}
            _ => {}
        }
    }
}

/// Rescale sample duration / composition offset into `to` timescale.
fn rescale_sample(mut sample: Sample, from: u32, to: u32) -> Sample {
    if from == 0 || from == to {
        return sample;
    }
    sample.duration = rescale_u32(sample.duration, from, to).max(1);
    sample.composition_offset = rescale_i32(sample.composition_offset, from, to);
    sample
}

fn rescale_u32(ticks: u32, from: u32, to: u32) -> u32 {
    ((u64::from(ticks) * u64::from(to)) / u64::from(from.max(1))) as u32
}

fn rescale_i32(ticks: i32, from: u32, to: u32) -> i32 {
    let from = i64::from(from.max(1));
    ((i64::from(ticks) * i64::from(to)) / from) as i32
}
