//! Shared access-unit types for the DASH packager.

use transmux::{CodecConfig, Sample};

/// Media timescale used by the packager (milliseconds).
pub const TIMESCALE: u32 = 1000;

pub const VIDEO_TRACK_ID: u32 = 1;
pub const AUDIO_TRACK_ID: u32 = 2;

/// One codec config or media sample ready for CMAF packaging.
#[derive(Debug, Clone)]
pub enum AccessUnit {
    VideoConfig { config: CodecConfig },
    AudioConfig { config: CodecConfig },
    VideoSample(Sample),
    AudioSample(Sample),
}
