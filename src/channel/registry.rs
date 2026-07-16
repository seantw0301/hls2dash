//! Runtime channel registry: CRUD + instant enable/disable.

use crate::config::{
    is_safe_channel, CacheConfig, Config, MpegTsConfig, OutputMode, PullSource,
};
use crate::hls::{self, ErrorSlot};
use anyhow::{bail, Result};
use reqwest::Client;
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// API / status view of one channel.
#[derive(Debug, Clone, Serialize)]
pub struct ChannelStatus {
    pub id: String,
    pub hls_url: String,
    pub enabled: bool,
    pub running: bool,
    pub mpd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

struct ChannelEntry {
    hls_url: String,
    enabled: bool,
    cancel: Option<CancellationToken>,
    join: Option<JoinHandle<()>>,
    last_error: ErrorSlot,
}

/// In-memory channel table seeded from config; mutated by `/api/channels`.
#[derive(Clone)]
pub struct ChannelRegistry {
    inner: Arc<AsyncMutex<HashMap<String, ChannelEntry>>>,
    cache_dir: PathBuf,
    cache: CacheConfig,
    reconnect_secs: u64,
    output_mode: OutputMode,
    mpegts: MpegTsConfig,
    http: Client,
}

impl ChannelRegistry {
    /// Create an empty registry bound to cache settings.
    pub fn new(cfg: &Config) -> Self {
        Self {
            inner: Arc::new(AsyncMutex::new(HashMap::new())),
            cache_dir: cfg.cache.dir.clone(),
            cache: cfg.cache.clone(),
            reconnect_secs: cfg.reconnect_secs.max(1),
            output_mode: cfg.output_mode,
            mpegts: cfg.mpegts.clone(),
            http: Client::builder()
                .user_agent("hls2dash/0.1")
                .build()
                .unwrap_or_else(|_| Client::new()),
        }
    }

    /// Seed channels from YAML `pull:` list and start enabled workers.
    pub async fn seed_from_config(&self, pull: &[PullSource]) -> Result<()> {
        for src in pull {
            self.add_channel(src.channel.clone(), src.url.clone(), src.enable)
                .await?;
        }
        Ok(())
    }

    /// List all configured channels (enabled or not).
    pub async fn list(&self) -> Vec<ChannelStatus> {
        let guard = self.inner.lock().await;
        let mut out: Vec<_> = guard
            .iter()
            .map(|(id, e)| self.status_of(id, e))
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// Channels that are enabled and currently running (rtmp2dash-compatible discovery).
    pub async fn list_active_discovery(&self) -> Vec<(String, String)> {
        self.list()
            .await
            .into_iter()
            .filter(|c| c.enabled && c.running)
            .map(|c| (c.id, c.mpd))
            .collect()
    }

    /// Count currently running pull workers.
    pub async fn active_count(&self) -> usize {
        self.list().await.iter().filter(|c| c.running).count()
    }

    fn status_of(&self, id: &str, e: &ChannelEntry) -> ChannelStatus {
        let running = e
            .join
            .as_ref()
            .map(|j| !j.is_finished())
            .unwrap_or(false);
        let last_error = e.last_error.lock().ok().and_then(|g| g.clone());
        ChannelStatus {
            id: id.to_string(),
            hls_url: e.hls_url.clone(),
            enabled: e.enabled,
            running,
            mpd: format!("/live/{id}/index.mpd"),
            last_error,
        }
    }

    /// Create a channel; start worker when `enabled`.
    pub async fn add_channel(&self, id: String, hls_url: String, enabled: bool) -> Result<()> {
        if !is_safe_channel(&id) {
            bail!("invalid channel id");
        }
        if !(hls_url.starts_with("http://") || hls_url.starts_with("https://")) {
            bail!("hls_url must be http(s)://");
        }
        let mut guard = self.inner.lock().await;
        if guard.contains_key(&id) {
            bail!("channel already exists: {id}");
        }
        let mut entry = ChannelEntry {
            hls_url,
            enabled: false,
            cancel: None,
            join: None,
            last_error: Arc::new(Mutex::new(None)),
        };
        if enabled {
            self.spawn_worker(&id, &mut entry);
            entry.enabled = true;
        }
        guard.insert(id, entry);
        Ok(())
    }

    /// Update fields; apply enable/disable and URL changes live.
    pub async fn update_channel(
        &self,
        id: &str,
        hls_url: Option<String>,
        enabled: Option<bool>,
    ) -> Result<()> {
        let mut guard = self.inner.lock().await;
        let Some(entry) = guard.get_mut(id) else {
            bail!("channel not found: {id}");
        };
        if let Some(url) = hls_url {
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                bail!("hls_url must be http(s)://");
            }
            let url_changed = url != entry.hls_url;
            entry.hls_url = url;
            if url_changed && entry.enabled {
                self.stop_worker(entry).await;
                self.spawn_worker(id, entry);
            }
        }
        if let Some(want) = enabled {
            if want && !entry.enabled {
                entry.enabled = true;
                if entry.join.as_ref().map(|j| j.is_finished()).unwrap_or(true) {
                    self.spawn_worker(id, entry);
                }
            } else if !want && entry.enabled {
                entry.enabled = false;
                self.stop_worker(entry).await;
            }
        }
        Ok(())
    }

    /// Remove channel, stop worker, delete cache dir.
    pub async fn remove_channel(&self, id: &str) -> Result<()> {
        let mut guard = self.inner.lock().await;
        let Some(mut entry) = guard.remove(id) else {
            bail!("channel not found: {id}");
        };
        entry.enabled = false;
        self.stop_worker(&mut entry).await;
        drop(guard);
        crate::cache::remove_channel_dir(&self.cache_dir, id);
        Ok(())
    }

    /// Enable and start pull immediately.
    pub async fn enable_channel(&self, id: &str) -> Result<()> {
        self.update_channel(id, None, Some(true)).await
    }

    /// Disable and stop pull immediately.
    pub async fn disable_channel(&self, id: &str) -> Result<()> {
        self.update_channel(id, None, Some(false)).await
    }

    /// Enable every channel.
    pub async fn enable_all(&self) -> usize {
        let ids: Vec<String> = self.list().await.into_iter().map(|c| c.id).collect();
        let mut n = 0;
        for id in ids {
            if self.enable_channel(&id).await.is_ok() {
                n += 1;
            }
        }
        n
    }

    /// Disable every channel.
    pub async fn disable_all(&self) -> usize {
        let ids: Vec<String> = self.list().await.into_iter().map(|c| c.id).collect();
        let mut n = 0;
        for id in ids {
            if self.disable_channel(&id).await.is_ok() {
                n += 1;
            }
        }
        n
    }

    fn spawn_worker(&self, id: &str, entry: &mut ChannelEntry) {
        let cancel = CancellationToken::new();
        let client = self.http.clone();
        let hls_url = entry.hls_url.clone();
        let channel = id.to_string();
        let out_dir = self.cache_dir.join("live").join(id);
        let cache = self.cache.clone();
        let reconnect_secs = self.reconnect_secs;
        let output_mode = self.output_mode;
        let mpegts = self.mpegts.clone();
        let last_error = Arc::clone(&entry.last_error);
        let token = cancel.clone();

        let join = tokio::spawn(async move {
            if let Err(err) = std::fs::create_dir_all(&out_dir) {
                warn!(channel = %channel, "create cache dir failed: {err}");
                return;
            }
            hls::run_forever(
                client,
                hls_url,
                channel,
                out_dir,
                cache,
                reconnect_secs,
                output_mode,
                mpegts,
                token,
                last_error,
            )
            .await;
        });

        entry.cancel = Some(cancel);
        entry.join = Some(join);
        info!(channel = %id, "pull worker spawned");
    }

    async fn stop_worker(&self, entry: &mut ChannelEntry) {
        if let Some(cancel) = entry.cancel.take() {
            cancel.cancel();
        }
        if let Some(join) = entry.join.take() {
            let _ = join.await;
        }
    }
}
