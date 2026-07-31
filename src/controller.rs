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

/// What an action actually did, for the on-screen readout. Derived from the
/// outcome rather than the keypress, so the volume shown is the level Spotify
/// ended up at and play/pause reflects the state we actually reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feedback {
    Playing,
    Paused,
    NextTrack,
    PreviousTrack,
    Volume(u8),
}

impl Feedback {
    pub fn label(self) -> String {
        match self {
            Feedback::Playing => "playing".into(),
            Feedback::Paused => "paused".into(),
            Feedback::NextTrack => "skipped to next".into(),
            Feedback::PreviousTrack => "skipped to previous".into(),
            Feedback::Volume(percent) => format!("volume {percent}%"),
        }
    }

    /// How long the readout should sit on screen. Volume comes in bursts and is
    /// its own confirmation, so it goes sooner; a skip or a pause is a discrete
    /// act worth a beat longer.
    pub fn hold(self) -> Duration {
        match self {
            Feedback::Volume(_) => Duration::from_millis(500),
            _ => Duration::from_millis(850),
        }
    }
}

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

    /// Apply `detents` worth of volume in a single write.
    ///
    /// A knob emits presses far faster than the API can answer them, so they
    /// are accumulated upstream and arrive here as one signed count. Doing it
    /// this way also keeps the level from lurching: one read-modify-write per
    /// burst cannot interleave with itself the way twenty of them would.
    pub async fn nudge_volume_by(&self, detents: i32) -> Result<Feedback> {
        let step = (detents * self.volume_step as i32).clamp(-100, 100) as i16;
        self.nudge_volume(step).await
    }

    pub async fn handle(&self, action: Action) -> Result<Feedback> {
        match action {
            Action::VolumeUp => self.nudge_volume_by(1).await,
            Action::VolumeDown => self.nudge_volume_by(-1).await,
            Action::PlayPause => self.toggle_play_pause().await,
            Action::NextTrack => {
                let device = self.ensure_target().await?;
                self.client.next_track(device.id.as_deref()).await?;
                self.invalidate_volume_cache();
                Ok(Feedback::NextTrack)
            }
            Action::PreviousTrack => {
                let device = self.ensure_target().await?;
                self.client.previous_track(device.id.as_deref()).await?;
                self.invalidate_volume_cache();
                Ok(Feedback::PreviousTrack)
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

    async fn nudge_volume(&self, step: i16) -> Result<Feedback> {
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
            // Still worth showing: the readout confirms the key registered.
            return Ok(Feedback::Volume(current));
        }

        self.client
            .set_volume(target, device_id.as_deref())
            .await?;
        self.store_volume(device_id, target);
        tracing::info!(from = current, to = target, "volume");
        Ok(Feedback::Volume(target))
    }

    async fn toggle_play_pause(&self) -> Result<Feedback> {
        let state = self.client.playback_state().await?;

        if let Some(playing) = &state
            && playing.is_playing
        {
            let id = playing.device.as_ref().and_then(|d| d.id.as_deref());
            self.client.pause(id).await?;
            tracing::info!("paused");
            return Ok(Feedback::Paused);
        }

        // Paused with a device we can still address, or nothing usable at all —
        // a session can outlive its device (client closed, network dropped).
        let device_id = match state.and_then(|s| s.device).filter(|d| d.id.is_some()) {
            Some(device) => device.id,
            None => self.ensure_target().await?.id,
        };
        self.client.play(device_id.as_deref()).await?;
        tracing::info!("playing");
        Ok(Feedback::Playing)
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
