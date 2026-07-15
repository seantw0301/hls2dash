use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub dash: DashConfig,
    pub cache: CacheConfig,
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

/// Pull one remote HLS playlist and publish DASH under `/live/<channel>/index.mpd`.
#[derive(Debug, Clone, Deserialize)]
pub struct PullSource {
    /// Source HLS URL, e.g. `http://origin/live/index.m3u8`
    pub url: String,
    /// Output channel id (DASH path segment)
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
        if let Some(ttl) = self.cache.ttl_secs {
            if ttl == 0 {
                bail!("cache.ttl_secs must be >= 1 when set");
            }
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

    /// Resolve the DASH HTTP listen address from config.
    pub fn dash_addr(&self) -> Result<SocketAddr> {
        format!("{}:{}", self.dash.listen, self.dash.port)
            .parse()
            .context("invalid dash.listen/port")
    }

    /// Cache directory path for a given channel id.
    pub fn channel_dir(&self, channel_id: &str) -> PathBuf {
        self.cache.dir.join("live").join(channel_id)
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
