use crate::hotkeys::Action;
use crate::spotify::{
    adjust_volume, pick_fallback_device, Device, SpotifyClient, SpotifyError, TokenProvider,
};
use anyhow::{bail, Result};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a locally-tracked volume stays trustworthy. Long enough that a held
/// key ramps without re-reading state, short enough that changes made elsewhere
/// (phone, another client) are picked up quickly.
const VOLUME_CACHE_TTL: Duration = Duration::from_secs(5);

/// How long to wait for a freshly launched Spotify to appear as a Connect device.
const DEVICE_WAIT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
struct VolumeCache {
    device_id: Option<String>,
    percent: u8,
    updated: Instant,
}

pub struct Controller<T: TokenProvider> {
    client: SpotifyClient<T>,
    volume_step: u8,
    /// Also serialises actions, so two fast repeats can't interleave a
    /// read-modify-write on the volume.
    cache: Mutex<Option<VolumeCache>>,
}

impl<T: TokenProvider> Controller<T> {
    pub fn new(client: SpotifyClient<T>, volume_step: u8) -> Self {
        Self {
            client,
            volume_step,
            cache: Mutex::new(None),
        }
    }

    pub fn client(&self) -> &SpotifyClient<T> {
        &self.client
    }

    pub async fn handle(&self, action: Action) -> Result<()> {
        match action {
            Action::VolumeUp => self.nudge_volume(self.volume_step as i16).await,
            Action::VolumeDown => self.nudge_volume(-(self.volume_step as i16)).await,
            Action::PlayPause => self.toggle_play_pause().await,
            Action::NextTrack => {
                self.ensure_target().await?;
                self.client.next_track().await?;
                self.invalidate_volume_cache();
                Ok(())
            }
            Action::PreviousTrack => {
                self.ensure_target().await?;
                self.client.previous_track().await?;
                self.invalidate_volume_cache();
                Ok(())
            }
        }
    }

    fn invalidate_volume_cache(&self) {
        if let Ok(mut guard) = self.cache.lock() {
            *guard = None;
        }
    }

    fn cached_volume(&self) -> Option<(Option<String>, u8)> {
        let guard = self.cache.lock().ok()?;
        let cache = guard.as_ref()?;
        if cache.updated.elapsed() < VOLUME_CACHE_TTL {
            Some((cache.device_id.clone(), cache.percent))
        } else {
            None
        }
    }

    fn store_volume(&self, device_id: Option<String>, percent: u8) {
        if let Ok(mut guard) = self.cache.lock() {
            *guard = Some(VolumeCache {
                device_id,
                percent,
                updated: Instant::now(),
            });
        }
    }

    async fn nudge_volume(&self, step: i16) -> Result<()> {
        let (device_id, current) = match self.cached_volume() {
            Some(hit) => hit,
            None => {
                let device = self.ensure_target().await?;
                if !device.supports_volume {
                    bail!(SpotifyError::VolumeUnsupported(device.name.clone()));
                }
                let current = device.volume_percent.ok_or_else(|| {
                    anyhow::anyhow!(
                        "device \"{}\" did not report its volume level",
                        device.name
                    )
                })?;
                (device.id.clone(), current)
            }
        };

        let target = adjust_volume(current, step);
        if target == current {
            tracing::debug!(current, "already at the volume limit");
            return Ok(());
        }

        self.client
            .set_volume(target, device_id.as_deref())
            .await?;
        self.store_volume(device_id, target);
        tracing::info!(from = current, to = target, "volume");
        Ok(())
    }

    async fn toggle_play_pause(&self) -> Result<()> {
        match self.client.playback_state().await? {
            Some(state) if state.is_playing => {
                self.client.pause().await?;
                tracing::info!("paused");
            }
            // A session can outlive its device (client closed, network dropped),
            // in which case an untargeted play just 404s.
            Some(state) if state.device.as_ref().is_some_and(|d| d.id.is_some()) => {
                self.client.play().await?;
                tracing::info!("playing");
            }
            _ => {
                // Nothing usable to talk to — adopt a device, then start it.
                self.ensure_target().await?;
                self.client.play().await?;
                tracing::info!("playing");
            }
        }
        Ok(())
    }

    /// Resolve a device we can send commands to, activating one if needed.
    async fn ensure_target(&self) -> Result<Device> {
        if let Some(state) = self.client.playback_state().await?
            && let Some(device) = state.device
            && device.id.is_some()
        {
            return Ok(device);
        }

        // Nothing active. Adopt an idle device if Spotify knows about one.
        let devices = self.client.devices().await?;
        if let Some(device) = pick_fallback_device(&devices) {
            let device = device.clone();
            let id = device.id.clone().expect("pick_fallback_device requires an id");
            tracing::info!(device = %device.name, "activating idle device");
            self.client.transfer_playback(&id, false).await?;
            return Ok(device);
        }

        // No devices known to Spotify at all — try the local desktop client.
        tracing::info!("no Spotify devices available; starting the desktop app");
        crate::launcher::ensure_running()?;
        let device = self.wait_for_device().await?;
        let id = device.id.clone().expect("pick_fallback_device requires an id");
        self.client.transfer_playback(&id, false).await?;
        Ok(device)
    }

    async fn wait_for_device(&self) -> Result<Device> {
        let deadline = Instant::now() + DEVICE_WAIT;
        while Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(750)).await;
            let devices = self.client.devices().await?;
            if let Some(d) = pick_fallback_device(&devices) {
                return Ok(d.clone());
            }
        }
        bail!(SpotifyError::NoActiveDevice)
    }
}
