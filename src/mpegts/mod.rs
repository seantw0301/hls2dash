//! Continuous MPEG-TS over HTTP from HLS (direct stitch, no CMAF).
//!
//! Behavior aligned with `trans_server` `/live/{channel}/mpegts` egress.

mod continuous;
mod repair;
mod stitch;
mod streamer;

pub use repair::{align_ts_packets_truncate, repair_ts_packets};
pub use stitch::TsStitcher;
pub use streamer::{channel_mpegts_ready, spawn_mpegts_stream, MpegTsStreamOpts};
