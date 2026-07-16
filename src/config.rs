use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

/// Egress mode: exactly one of DASH or continuous MPEG-TS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum OutputMode {
    #[default]
    Dash,
    Mpegts,
}

impl OutputMode {
    pub fn as_str(self) -> &'static str {
        match self {
            OutputMode::Dash => "dash",
            OutputMode::Mpegts => "mpegts",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Listen / bind for HTTP egress (DASH or MPEG-TS).
    pub dash: DashConfig,
    pub cache: CacheConfig,
    /// `dash` or `mpegts` — mutually exclusive egress (2選1).
    #[serde(default)]
    pub output_mode: OutputMode,
    /// MPEG-TS streamer knobs (used when `output_mode: mpegts`).
    #[serde(default)]
    pub mpegts: MpegTsConfig,
    /// Global pull reconnect delay (seconds) for every channel.
    #[serde(default = "default_reconnect_secs")]
    pub reconnect_secs: u64,
    #[serde(default)]
    pub pull: Vec<PullSource>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DashConfig {
    pub listen: String,
    pub port: u16,
}

/// Continuous `/live/<channel>/mpegts` settings (aligned with trans_server).
#[derive(Debug, Clone, Deserialize)]
pub struct MpegTsConfig {
    /// Segments withheld from live edge when a client joins / catches up.
    /// Acts as the primary jitter buffer against delayed HLS arrivals.
    #[serde(default = "default_live_holdback_segments")]
    pub live_holdback_segments: u64,
    /// Do not open `/mpegts` until at least this many stitched segments exist.
    #[serde(default = "default_min_buffer_segments")]
    pub min_buffer_segments: u64,
    /// If client falls more than this many segments behind safe edge, jump forward.
    #[serde(default = "default_max_segment_lag")]
    pub max_segment_lag: u64,
    /// Per-client outbound chunk queue depth (backpressure).
    #[serde(default = "default_mpegts_send_queue")]
    pub send_queue: usize,
    /// Cache poll interval while waiting for the next segment.
    #[serde(default = "default_mpegts_poll_interval_secs")]
    pub poll_interval_secs: u64,
    /// Pace HTTP egress at ~1× media realtime (uses EXTINF / stored duration).
    #[serde(default = "default_true")]
    pub pace_egress: bool,
    /// Playlist poll factor vs TARGETDURATION when filling the buffer (smaller = more aggressive).
    #[serde(default = "default_ingest_poll_factor")]
    pub ingest_poll_factor: f64,
}

impl Default for MpegTsConfig {
    fn default() -> Self {
        Self {
            live_holdback_segments: default_live_holdback_segments(),
            min_buffer_segments: default_min_buffer_segments(),
            max_segment_lag: default_max_segment_lag(),
            send_queue: default_mpegts_send_queue(),
            poll_interval_secs: default_mpegts_poll_interval_secs(),
            pace_egress: true,
            ingest_poll_factor: default_ingest_poll_factor(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CacheConfig {
    pub dir: PathBuf,
    #[serde(default = "default_segment_duration")]
    pub segment_duration_secs: f64,
    #[serde(default = "default_window_segments")]
    pub window_segments: usize,
    #[serde(default)]
    pub ttl_secs: Option<u64>,
    #[serde(default = "default_cleanup_interval_secs")]
    pub cleanup_interval_secs: u64,
}

impl CacheConfig {
    /// Effective TTL used by the janitor (never below the live window duration).
    pub fn effective_ttl_secs(&self) -> u64 {
        let window_secs = (self.window_segments as f64 * self.segment_duration_secs).ceil() as u64;
        let floor = window_secs.saturating_mul(2).max(30);
        match self.ttl_secs {
            Some(t) => t.max(floor),
            None => floor,
        }
    }
}

/// Pull one remote HLS playlist and publish under `/live/<channel>/…`.
#[derive(Debug, Clone, Deserialize)]
pub struct PullSource {
    /// Source HLS URL (path segments like `sh_012` are origin-only, not the play name).
    pub url: String,
    /// Playback channel name (path segment under `/live/<channel>/…`).
    /// Independent of the origin URL path — e.g. url `…/sh_012/…` → channel `cctv1`.
    pub channel: String,
    #[serde(default = "default_enable")]
    pub enable: bool,
}

fn default_enable() -> bool {
    true
}

fn default_segment_duration() -> f64 {
    2.0
}

fn default_window_segments() -> usize {
    90
}

fn default_cleanup_interval_secs() -> u64 {
    10
}

fn default_reconnect_secs() -> u64 {
    3
}

fn default_live_holdback_segments() -> u64 {
    6
}

fn default_min_buffer_segments() -> u64 {
    3
}

fn default_max_segment_lag() -> u64 {
    10
}

fn default_mpegts_send_queue() -> usize {
    32
}

fn default_mpegts_poll_interval_secs() -> u64 {
    2
}

fn default_true() -> bool {
    true
}

fn default_ingest_poll_factor() -> f64 {
    0.25
}

impl Config {
    /// Load and validate configuration from a YAML file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        let cfg: Config = serde_yaml::from_str(&raw)
            .with_context(|| format!("failed to parse config {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if !(self.cache.segment_duration_secs.is_finite() && self.cache.segment_duration_secs > 0.0)
        {
            bail!("cache.segment_duration_secs must be > 0");
        }
        if self.cache.window_segments == 0 {
            bail!("cache.window_segments must be >= 1");
        }
        if self.cache.cleanup_interval_secs == 0 {
            bail!("cache.cleanup_interval_secs must be >= 1");
        }
        if self.reconnect_secs == 0 {
            bail!("reconnect_secs must be >= 1");
        }
        if let Some(ttl) = self.ttl_secs_opt() {
            if ttl == 0 {
                bail!("cache.ttl_secs must be >= 1 when set");
            }
        }
        if self.mpegts.live_holdback_segments == 0 {
            bail!("mpegts.live_holdback_segments must be >= 1");
        }
        if self.mpegts.min_buffer_segments == 0 {
            bail!("mpegts.min_buffer_segments must be >= 1");
        }
        if self.mpegts.max_segment_lag == 0 {
            bail!("mpegts.max_segment_lag must be >= 1");
        }
        if self.mpegts.send_queue == 0 {
            bail!("mpegts.send_queue must be >= 1");
        }
        if self.mpegts.poll_interval_secs == 0 {
            bail!("mpegts.poll_interval_secs must be >= 1");
        }
        if !(self.mpegts.ingest_poll_factor.is_finite() && self.mpegts.ingest_poll_factor > 0.0) {
            bail!("mpegts.ingest_poll_factor must be > 0");
        }

        let mut seen = std::collections::HashSet::new();
        for (i, src) in self.pull.iter().enumerate() {
            if src.channel.trim().is_empty() {
                bail!("pull[{i}].channel must not be empty");
            }
            if !is_safe_channel(&src.channel) {
                bail!("pull[{i}].channel has invalid characters: {}", src.channel);
            }
            if !seen.insert(src.channel.clone()) {
                bail!("duplicate pull channel '{}'", src.channel);
            }
            if src.url.trim().is_empty() {
                bail!("pull[{i}].url must not be empty");
            }
            if !(src.url.starts_with("http://") || src.url.starts_with("https://")) {
                bail!("pull[{i}].url must be http(s)://");
            }
        }
        Ok(())
    }

    fn ttl_secs_opt(&self) -> Option<u64> {
        self.cache.ttl_secs
    }

    /// Resolve the HTTP listen address from config.
    pub fn dash_addr(&self) -> Result<SocketAddr> {
        format!("{}:{}", self.dash.listen, self.dash.port)
            .parse()
            .context("invalid dash.listen/port")
    }

    /// Cache directory path for a given channel id.
    pub fn channel_dir(&self, channel_id: &str) -> PathBuf {
        self.cache.dir.join("live").join(channel_id)
    }

    pub fn is_dash(&self) -> bool {
        self.output_mode == OutputMode::Dash
    }

    pub fn is_mpegts(&self) -> bool {
        self.output_mode == OutputMode::Mpegts
    }
}

/// Return true if `channel` is a safe path segment for cache and HTTP URLs.
pub fn is_safe_channel(channel: &str) -> bool {
    !channel.is_empty()
        && channel.len() <= 128
        && channel
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Default enable for API-created channels.
pub fn default_enabled() -> bool {
    true
}
