use serde::Deserialize;
use std::future::Future;

pub const API_BASE: &str = "https://api.spotify.com";

/// Scopes needed to read the active device and drive playback.
pub const SCOPES: &str = "user-read-playback-state user-modify-playback-state";

#[derive(Debug, thiserror::Error)]
pub enum SpotifyError {
    #[error("no active Spotify device — open Spotify and start playing something once")]
    NoActiveDevice,

    #[error("Spotify rejected the request: {0} (playback control requires Premium)")]
    Forbidden(String),

    #[error("authorization expired or was revoked — run `spotify-control login` again")]
    Unauthorized,

    #[error("rate limited by Spotify; retry in {0}s")]
    RateLimited(u64),

    #[error("device \"{0}\" does not support remote volume control")]
    VolumeUnsupported(String),

    #[error("Spotify API error {status}: {message}")]
    Api { status: u16, message: String },

    #[error(transparent)]
    Http(#[from] reqwest::Error),
}

pub type Result<T> = std::result::Result<T, SpotifyError>;

/// Supplies a bearer token, refreshing it when needed.
pub trait TokenProvider {
    fn access_token(&self) -> impl Future<Output = anyhow::Result<String>> + Send;

    /// Force the next `access_token` call to refresh. Called after a 401 so a
    /// long-running daemon can recover from a revoked or clock-skewed token.
    fn invalidate(&self) -> impl Future<Output = ()> + Send;
}

/// A fixed token. Useful for tests and for driving the API with a token minted
/// elsewhere.
pub struct StaticToken(pub String);

impl TokenProvider for StaticToken {
    async fn access_token(&self) -> anyhow::Result<String> {
        Ok(self.0.clone())
    }

    async fn invalidate(&self) {}
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Device {
    pub id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub is_active: bool,
    #[serde(default)]
    pub is_restricted: bool,
    pub volume_percent: Option<u8>,
    /// Older clients omit this; assume volume works rather than blocking the user.
    #[serde(default = "default_true")]
    pub supports_volume: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Playback {
    pub device: Option<Device>,
    #[serde(default)]
    pub is_playing: bool,
}

#[derive(Debug, Deserialize)]
struct DevicesResponse {
    devices: Vec<Device>,
}

/// Clamp a volume adjustment into Spotify's 0–100 range.
pub fn adjust_volume(current: u8, step: i16) -> u8 {
    (i16::from(current) + step).clamp(0, 100) as u8
}

/// Pick the best device to take over when nothing is currently active.
/// Restricted devices reject remote commands, so they are never chosen.
pub fn pick_fallback_device(devices: &[Device]) -> Option<&Device> {
    devices
        .iter()
        .find(|d| d.is_active && !d.is_restricted && d.id.is_some())
        .or_else(|| devices.iter().find(|d| !d.is_restricted && d.id.is_some()))
}

pub struct SpotifyClient<T: TokenProvider> {
    http: reqwest::Client,
    base_url: String,
    tokens: T,
}

impl<T: TokenProvider> SpotifyClient<T> {
    pub fn new(tokens: T) -> Self {
        Self::with_base_url(tokens, API_BASE.to_string())
    }

    pub fn with_base_url(tokens: T, base_url: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            tokens,
        }
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<serde_json::Value>,
    ) -> Result<reqwest::Response> {
        // One retry: a 401 here almost always means the cached token went stale,
        // so drop it and let the provider mint a fresh one.
        for attempt in 0..2 {
            let token = self
                .tokens
                .access_token()
                .await
                .map_err(|e| SpotifyError::Api { status: 0, message: e.to_string() })?;

            let mut req = self
                .http
                .request(method.clone(), format!("{}{}", self.base_url, path))
                .bearer_auth(token);

            if !query.is_empty() {
                req = req.query(query);
            }
            // Spotify's player endpoints 400 on a PUT with no body at all.
            req = match &body {
                Some(b) => req.json(b),
                None if method == reqwest::Method::PUT || method == reqwest::Method::POST => {
                    req.header(reqwest::header::CONTENT_LENGTH, "0")
                }
                None => req,
            };

            let resp = req.send().await?;

            if resp.status() == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
                self.tokens.invalidate().await;
                continue;
            }
            return check_status(resp).await;
        }
        unreachable!("loop always returns on the second attempt")
    }

    /// Current playback state. `None` means Spotify reported no active session.
    pub async fn playback_state(&self) -> Result<Option<Playback>> {
        let resp = self
            .send(reqwest::Method::GET, "/v1/me/player", &[], None)
            .await?;

        if resp.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(None);
        }
        Ok(Some(resp.json::<Playback>().await?))
    }

    pub async fn devices(&self) -> Result<Vec<Device>> {
        let resp = self
            .send(reqwest::Method::GET, "/v1/me/player/devices", &[], None)
            .await?;
        Ok(resp.json::<DevicesResponse>().await?.devices)
    }

    pub async fn set_volume(&self, percent: u8, device_id: Option<&str>) -> Result<()> {
        let mut query = vec![("volume_percent", percent.to_string())];
        if let Some(id) = device_id {
            query.push(("device_id", id.to_string()));
        }
        self.send(reqwest::Method::PUT, "/v1/me/player/volume", &query, None)
            .await?;
        Ok(())
    }

    pub async fn next_track(&self) -> Result<()> {
        self.send(reqwest::Method::POST, "/v1/me/player/next", &[], None)
            .await?;
        Ok(())
    }

    pub async fn previous_track(&self) -> Result<()> {
        self.send(reqwest::Method::POST, "/v1/me/player/previous", &[], None)
            .await?;
        Ok(())
    }

    pub async fn play(&self) -> Result<()> {
        self.send(reqwest::Method::PUT, "/v1/me/player/play", &[], None)
            .await?;
        Ok(())
    }

    pub async fn pause(&self) -> Result<()> {
        self.send(reqwest::Method::PUT, "/v1/me/player/pause", &[], None)
            .await?;
        Ok(())
    }

    /// Move playback to `device_id`, which also makes it the active device.
    pub async fn transfer_playback(&self, device_id: &str, play: bool) -> Result<()> {
        let body = serde_json::json!({ "device_ids": [device_id], "play": play });
        self.send(reqwest::Method::PUT, "/v1/me/player", &[], Some(body))
            .await?;
        Ok(())
    }
}

async fn check_status(resp: reqwest::Response) -> Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }

    match status.as_u16() {
        401 => Err(SpotifyError::Unauthorized),
        403 => Err(SpotifyError::Forbidden(error_message(resp).await)),
        404 => Err(SpotifyError::NoActiveDevice),
        429 => {
            let secs = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .unwrap_or(1);
            Err(SpotifyError::RateLimited(secs))
        }
        other => Err(SpotifyError::Api {
            status: other,
            message: error_message(resp).await,
        }),
    }
}

/// Spotify wraps errors as `{"error":{"status":..,"message":".."}}`, but auth
/// failures and gateways sometimes return plain text.
async fn error_message(resp: reqwest::Response) -> String {
    let body = resp.text().await.unwrap_or_default();
    serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| {
            if body.trim().is_empty() {
                "no details".to_string()
            } else {
                body
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_steps_up_and_down() {
        assert_eq!(adjust_volume(50, 5), 55);
        assert_eq!(adjust_volume(50, -5), 45);
    }

    #[test]
    fn volume_clamps_at_both_rails() {
        assert_eq!(adjust_volume(98, 5), 100);
        assert_eq!(adjust_volume(100, 5), 100);
        assert_eq!(adjust_volume(2, -5), 0);
        assert_eq!(adjust_volume(0, -5), 0);
    }

    #[test]
    fn volume_never_wraps_around() {
        // The whole point of doing the math in i16: u8 arithmetic would wrap.
        assert_eq!(adjust_volume(0, -100), 0);
        assert_eq!(adjust_volume(255, 50), 100);
    }

    fn dev(name: &str, active: bool, restricted: bool, has_id: bool) -> Device {
        Device {
            id: has_id.then(|| format!("id-{name}")),
            name: name.to_string(),
            is_active: active,
            is_restricted: restricted,
            volume_percent: Some(50),
            supports_volume: true,
        }
    }

    #[test]
    fn fallback_prefers_the_active_device() {
        let devices = vec![dev("phone", false, false, true), dev("desktop", true, false, true)];
        assert_eq!(pick_fallback_device(&devices).unwrap().name, "desktop");
    }

    #[test]
    fn fallback_skips_restricted_devices() {
        let devices = vec![dev("tv", true, true, true), dev("desktop", false, false, true)];
        assert_eq!(pick_fallback_device(&devices).unwrap().name, "desktop");
    }

    #[test]
    fn fallback_skips_devices_without_an_id() {
        let devices = vec![dev("ghost", false, false, false), dev("desktop", false, false, true)];
        assert_eq!(pick_fallback_device(&devices).unwrap().name, "desktop");
    }

    #[test]
    fn fallback_returns_none_when_nothing_is_usable() {
        assert!(pick_fallback_device(&[]).is_none());
        assert!(pick_fallback_device(&[dev("tv", true, true, true)]).is_none());
    }

    #[test]
    fn device_defaults_supports_volume_to_true_when_absent() {
        let d: Device = serde_json::from_str(r#"{"id":"x","name":"Old Client"}"#).unwrap();
        assert!(d.supports_volume);
        assert!(!d.is_active);
    }
}
