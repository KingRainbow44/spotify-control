use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const APP_DIR: &str = "spotify-control";

fn default_volume_step() -> u8 {
    5
}

fn default_redirect_port() -> u16 {
    8888
}

/// Key combinations, as `global-hotkey` accepts them: modifiers first, joined by
/// `+`. Modifier names are `Ctrl`/`Control`, `Alt`, `Shift`, and
/// `Super`/`Cmd`/`Command` for the Windows/Command key — note that `Win` is not
/// a recognised token.
///
/// Configurable because a combination you want may already be held by another
/// app (launchers like Raycast, graphics-driver utilities, etc.). Remap here if
/// one of the defaults clashes with something you'd rather keep.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Bindings {
    #[serde(default = "default_volume_up")]
    pub volume_up: String,
    #[serde(default = "default_volume_down")]
    pub volume_down: String,
    #[serde(default = "default_play_pause")]
    pub play_pause: String,
    #[serde(default = "default_next_track")]
    pub next_track: String,
    #[serde(default = "default_previous_track")]
    pub previous_track: String,
}

fn default_volume_up() -> String {
    "Ctrl+Alt+ArrowRight".into()
}
fn default_volume_down() -> String {
    "Ctrl+Alt+ArrowLeft".into()
}
fn default_play_pause() -> String {
    "Ctrl+Alt+Home".into()
}
fn default_next_track() -> String {
    "Ctrl+Super+Alt+ArrowRight".into()
}
fn default_previous_track() -> String {
    "Ctrl+Super+Alt+ArrowLeft".into()
}

impl Default for Bindings {
    fn default() -> Self {
        Self {
            volume_up: default_volume_up(),
            volume_down: default_volume_down(),
            play_pause: default_play_pause(),
            next_track: default_next_track(),
            previous_track: default_previous_track(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Client ID from the Spotify Developer Dashboard. There is no secret: the
    /// desktop flow is PKCE, so shipping a secret would be pointless anyway.
    pub client_id: String,

    #[serde(default = "default_volume_step")]
    pub volume_step: u8,

    /// Must match a redirect URI registered on the Spotify app exactly.
    #[serde(default = "default_redirect_port")]
    pub redirect_port: u16,

    #[serde(default)]
    pub bindings: Bindings,
}

impl Config {
    pub fn redirect_uri(&self) -> String {
        // Spotify requires an explicit loopback IP here; "localhost" is rejected
        // for new apps.
        format!("http://127.0.0.1:{}/callback", self.redirect_port)
    }

    pub fn load() -> Result<Self> {
        Self::load_from(&config_path()?)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).with_context(|| {
            format!(
                "no config at {}\n\nRun `spotify-control init --client-id <ID>` first.",
                path.display()
            )
        })?;
        let cfg: Config =
            serde_json::from_str(&raw).with_context(|| format!("invalid config at {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        self.validate()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(!self.client_id.trim().is_empty(), "client_id must not be empty");
        anyhow::ensure!(
            (1..=50).contains(&self.volume_step),
            "volume_step must be between 1 and 50 (got {})",
            self.volume_step
        );
        anyhow::ensure!(self.redirect_port >= 1024, "redirect_port must be >= 1024");
        Ok(())
    }
}

pub fn config_dir() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .context("could not resolve the user config directory")?
        .join(APP_DIR))
}

pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.json"))
}

pub fn token_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("tokens.json"))
}

pub fn log_dir() -> Result<PathBuf> {
    Ok(config_dir()?.join("logs"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Config {
        Config {
            client_id: "abc123".into(),
            volume_step: 5,
            redirect_port: 8888,
            bindings: Bindings::default(),
        }
    }

    #[test]
    fn redirect_uri_uses_loopback_ip_not_localhost() {
        // Spotify rejects "localhost" for newly registered apps.
        assert_eq!(sample().redirect_uri(), "http://127.0.0.1:8888/callback");
    }

    #[test]
    fn rejects_empty_client_id() {
        let mut c = sample();
        c.client_id = "   ".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_out_of_range_volume_step() {
        let mut c = sample();
        c.volume_step = 0;
        assert!(c.validate().is_err());
        c.volume_step = 80;
        assert!(c.validate().is_err());
        c.volume_step = 50;
        assert!(c.validate().is_ok());
    }

    #[test]
    fn rejects_privileged_port() {
        let mut c = sample();
        c.redirect_port = 80;
        assert!(c.validate().is_err());
    }

    #[test]
    fn roundtrips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.json");
        sample().save_to(&path).unwrap();
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded.client_id, "abc123");
        assert_eq!(loaded.volume_step, 5);
    }

    #[test]
    fn applies_defaults_for_omitted_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, r#"{"client_id":"xyz"}"#).unwrap();
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded.volume_step, 5);
        assert_eq!(loaded.redirect_port, 8888);
    }

    #[test]
    fn missing_config_error_mentions_init() {
        let dir = tempfile::tempdir().unwrap();
        let err = Config::load_from(&dir.path().join("nope.json")).unwrap_err();
        assert!(format!("{err}").contains("init"));
    }
}
