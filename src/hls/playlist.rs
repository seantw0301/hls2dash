//! HLS playlist polling and segment download.

use anyhow::{bail, Context, Result};
use reqwest::Client;
use std::time::Duration;
use tracing::debug;
use url::Url;

/// One media segment listed in a media playlist.
#[derive(Debug, Clone)]
pub struct PlaylistSegment {
    pub seq: u64,
    pub uri: String,
    pub duration_secs: f64,
    pub discontinuity: bool,
}

/// Parsed live media playlist snapshot.
#[derive(Debug, Clone)]
pub struct MediaPlaylist {
    pub media_sequence: u64,
    pub target_duration: f64,
    pub segments: Vec<PlaylistSegment>,
    pub end_list: bool,
}

/// Fetch text body from `url` with a short timeout.
pub async fn fetch_text(client: &Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("GET {url} → HTTP {}", resp.status());
    }
    resp.text()
        .await
        .with_context(|| format!("read body {url}"))
}

/// Fetch binary body (MPEG-TS segment).
pub async fn fetch_bytes(client: &Client, url: &str) -> Result<Vec<u8>> {
    let resp = client
        .get(url)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("GET {url} → HTTP {}", resp.status());
    }
    let bytes = resp
        .bytes()
        .await
        .with_context(|| format!("read bytes {url}"))?;
    Ok(bytes.to_vec())
}

/// Resolve a possibly-relative URI against the playlist URL.
pub fn resolve_uri(playlist_url: &str, uri: &str) -> Result<String> {
    if uri.starts_with("http://") || uri.starts_with("https://") {
        return Ok(uri.to_string());
    }
    let base = Url::parse(playlist_url).with_context(|| format!("parse playlist {playlist_url}"))?;
    Ok(base
        .join(uri)
        .with_context(|| format!("join {playlist_url} + {uri}"))?
        .to_string())
}

/// If `body` is a master playlist, return the first media variant URL; else `None`.
pub fn first_variant_uri(body: &str) -> Option<String> {
    let mut pending_stream_inf = false;
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("#EXT-X-STREAM-INF:") {
            pending_stream_inf = true;
            continue;
        }
        if pending_stream_inf && !line.starts_with('#') {
            return Some(line.to_string());
        }
        if line.starts_with('#') {
            pending_stream_inf = false;
        }
    }
    None
}

/// Parse an HLS media playlist (RFC 8216 subset for live TS).
pub fn parse_media_playlist(body: &str) -> Result<MediaPlaylist> {
    let mut media_sequence = 0u64;
    let mut target_duration = 6.0f64;
    let mut end_list = false;
    let mut segments = Vec::new();
    let mut pending_duration: Option<f64> = None;
    let mut pending_discontinuity = false;
    let mut seq = 0u64;
    let mut saw_media_sequence = false;

    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            media_sequence = rest.trim().parse().unwrap_or(0);
            seq = media_sequence;
            saw_media_sequence = true;
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            target_duration = rest.trim().parse().unwrap_or(6.0);
            continue;
        }
        if line == "#EXT-X-ENDLIST" {
            end_list = true;
            continue;
        }
        if line == "#EXT-X-DISCONTINUITY" {
            pending_discontinuity = true;
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            let dur = rest
                .split(',')
                .next()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(target_duration);
            pending_duration = Some(dur);
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        // URI line
        if let Some(dur) = pending_duration.take() {
            if !saw_media_sequence && segments.is_empty() {
                seq = media_sequence;
            }
            segments.push(PlaylistSegment {
                seq,
                uri: line.to_string(),
                duration_secs: dur,
                discontinuity: pending_discontinuity,
            });
            pending_discontinuity = false;
            seq = seq.saturating_add(1);
        }
    }

    debug!(
        media_sequence,
        segments = segments.len(),
        target_duration,
        end_list,
        "parsed media playlist"
    );
    Ok(MediaPlaylist {
        media_sequence,
        target_duration,
        segments,
        end_list,
    })
}

/// Resolve playlist URL: follow one level of master → media if needed.
pub async fn resolve_media_playlist_url(client: &Client, url: &str) -> Result<(String, String)> {
    let body = fetch_text(client, url).await?;
    if let Some(variant) = first_variant_uri(&body) {
        let media_url = resolve_uri(url, &variant)?;
        let media_body = fetch_text(client, &media_url).await?;
        return Ok((media_url, media_body));
    }
    Ok((url.to_string(), body))
}
