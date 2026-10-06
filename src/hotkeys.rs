use crate::config::Bindings;
use anyhow::{Context, Result};
use global_hotkey::hotkey::HotKey;
use global_hotkey::GlobalHotKeyManager;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    VolumeUp,
    VolumeDown,
    PlayPause,
    NextTrack,
    PreviousTrack,
}

impl Action {
    pub fn label(self) -> &'static str {
        match self {
            Action::VolumeUp => "volume up",
            Action::VolumeDown => "volume down",
            Action::PlayPause => "play/pause",
            Action::NextTrack => "next track",
            Action::PreviousTrack => "previous track",
        }
    }

    /// Stable name for the action outside this process. Matches the config keys.
    pub fn id(self) -> &'static str {
        match self {
            Action::VolumeUp => "volume_up",
            Action::VolumeDown => "volume_down",
            Action::PlayPause => "play_pause",
            Action::NextTrack => "next_track",
            Action::PreviousTrack => "previous_track",
        }
    }

    pub fn from_id(id: &str) -> Option<Action> {
        ALL_ACTIONS.into_iter().find(|a| a.id() == id)
    }
}

impl Action {
    /// The configured combination for this action.
    pub fn spec(self, bindings: &Bindings) -> &str {
        match self {
            Action::VolumeUp => &bindings.volume_up,
            Action::VolumeDown => &bindings.volume_down,
            Action::PlayPause => &bindings.play_pause,
            Action::NextTrack => &bindings.next_track,
            Action::PreviousTrack => &bindings.previous_track,
        }
    }
}

pub const ALL_ACTIONS: [Action; 5] = [
    Action::VolumeUp,
    Action::VolumeDown,
    Action::PlayPause,
    Action::NextTrack,
    Action::PreviousTrack,
];

/// Parse the configured combinations into hotkeys.
///
/// Windows matches modifier sets exactly, so combinations that differ only by an
/// extra modifier do not collide with each other.
pub fn parse_bindings(bindings: &Bindings) -> Result<Vec<(Action, HotKey)>> {
    let mut out = Vec::new();
    for action in ALL_ACTIONS {
        let spec = action.spec(bindings);
        let hotkey: HotKey = spec.parse().map_err(|e| {
            anyhow::anyhow!("invalid binding \"{spec}\" for {}: {e}", action.label())
        })?;
        out.push((action, hotkey));
    }

    // Two actions on one combination would make dispatch ambiguous, since the
    // hotkey id is derived purely from (modifiers, key).
    for (i, (a, ha)) in out.iter().enumerate() {
        for (b, hb) in &out[i + 1..] {
            anyhow::ensure!(
                ha.id() != hb.id(),
                "{} and {} are both bound to {}",
                a.label(),
                b.label(),
                a.spec(bindings)
            );
        }
    }
    Ok(out)
}

pub fn default_bindings() -> Vec<(Action, HotKey)> {
    parse_bindings(&Bindings::default()).expect("built-in default bindings must parse")
}

/// Human-readable description of each binding, for status output.
pub fn describe_bindings(bindings: &Bindings) -> Vec<(Action, String)> {
    ALL_ACTIONS
        .into_iter()
        .map(|a| (a, a.spec(bindings).replace('+', " + ")))
        .collect()
}

/// A binding in the XDG shortcuts notation the GlobalShortcuts portal takes as
/// a preferred trigger, e.g. `CTRL+ALT+Right`. `None` for keys it can't name.
pub fn xdg_trigger(spec: &str) -> Option<String> {
    use global_hotkey::hotkey::{Code, Modifiers};

    let hotkey: HotKey = spec.parse().ok()?;
    let name = hotkey.key.to_string();
    let key = match hotkey.key {
        Code::ArrowLeft => "Left".to_string(),
        Code::ArrowRight => "Right".to_string(),
        Code::ArrowUp => "Up".to_string(),
        Code::ArrowDown => "Down".to_string(),
        Code::PageUp => "Page_Up".to_string(),
        Code::PageDown => "Page_Down".to_string(),
        Code::Space => "space".to_string(),
        Code::Home | Code::End | Code::Insert | Code::Delete => name,
        _ => {
            if let Some(letter) = name.strip_prefix("Key") {
                letter.to_lowercase()
            } else if let Some(digit) = name.strip_prefix("Digit") {
                digit.to_string()
            } else if name.starts_with('F') && name[1..].parse::<u8>().is_ok() {
                name
            } else {
                return None;
            }
        }
    };

    let mut parts = Vec::new();
    for (modifier, label) in [
        (Modifiers::CONTROL, "CTRL"),
        (Modifiers::ALT, "ALT"),
        (Modifiers::SHIFT, "SHIFT"),
        (Modifiers::SUPER, "LOGO"),
    ] {
        if hotkey.mods.contains(modifier) {
            parts.push(label.to_string());
        }
    }
    parts.push(key);
    Some(parts.join("+"))
}

/// Minimum gap between repeats. Windows auto-repeats a held hotkey, and each
/// action costs API calls, so unthrottled repeats would trip Spotify's 429s.
///
/// Volume does not come through here — it is accumulated and applied in bulk by
/// the daemon instead, so that a knob's detents are all counted rather than
/// thrown away.
const REPEAT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(400);

/// Drops hotkey repeats that arrive faster than [`REPEAT_INTERVAL`].
#[derive(Default)]
pub struct Debouncer {
    last: HashMap<Action, std::time::Instant>,
}

impl Debouncer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn allow(&mut self, action: Action) -> bool {
        self.allow_at(action, std::time::Instant::now())
    }

    /// Clock is injected so the throttling rules can be tested without sleeping.
    pub fn allow_at(&mut self, action: Action, now: std::time::Instant) -> bool {
        match self.last.get(&action) {
            Some(&prev) if now.duration_since(prev) < REPEAT_INTERVAL => false,
            _ => {
                self.last.insert(action, now);
                true
            }
        }
    }
}

/// A binding the OS refused, because something else already holds it.
#[derive(Debug, Clone)]
pub struct Conflict {
    pub action: Action,
    pub keys: String,
    pub reason: String,
}

/// Owns the registered hotkeys. Dropping this releases them back to the system.
pub struct HotkeyRegistry {
    manager: GlobalHotKeyManager,
    registered: Vec<HotKey>,
    by_id: HashMap<u32, Action>,
    conflicts: Vec<Conflict>,
}

impl HotkeyRegistry {
    /// Must be called on the thread that will run the platform event loop.
    ///
    /// A key another app already owns is reported rather than fatal: losing one
    /// binding shouldn't cost the user the others.
    pub fn register_all(bindings: &Bindings) -> Result<Self> {
        let parsed = parse_bindings(bindings)?;
        let manager =
            GlobalHotKeyManager::new().context("could not create the global hotkey manager")?;

        let descriptions: HashMap<Action, String> = describe_bindings(bindings).into_iter().collect();
        let mut registered = Vec::new();
        let mut by_id = HashMap::new();
        let mut conflicts = Vec::new();

        for (action, hotkey) in parsed {
            match manager.register(hotkey) {
                Ok(()) => {
                    by_id.insert(hotkey.id(), action);
                    registered.push(hotkey);
                }
                Err(e) => conflicts.push(Conflict {
                    action,
                    keys: descriptions.get(&action).cloned().unwrap_or_default(),
                    reason: e.to_string(),
                }),
            }
        }

        anyhow::ensure!(
            !registered.is_empty(),
            "every binding was refused by the system — another instance of \
             spotify-control is probably already running"
        );

        Ok(Self { manager, registered, by_id, conflicts })
    }

    pub fn action_for(&self, id: u32) -> Option<Action> {
        self.by_id.get(&id).copied()
    }

    pub fn conflicts(&self) -> &[Conflict] {
        &self.conflicts
    }

    pub fn active_actions(&self) -> Vec<Action> {
        ALL_ACTIONS
            .into_iter()
            .filter(|a| !self.conflicts.iter().any(|c| c.action == *a))
            .collect()
    }
}

impl Drop for HotkeyRegistry {
    fn drop(&mut self) {
        let _ = self.manager.unregister_all(&self.registered);
    }
}

/// Whether this process runs at high integrity.
///
/// Matters because Windows' UIPI will not deliver `WM_HOTKEY` to a
/// medium-integrity process while an elevated window owns the foreground — the
/// hotkey registers fine and then simply never fires.
#[cfg(target_os = "windows")]
pub fn is_elevated() -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut returned = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        );
        CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

#[cfg(not(target_os = "windows"))]
pub fn is_elevated() -> bool {
    // No UIPI equivalent elsewhere; treat as unrestricted.
    true
}

/// Warn when hotkeys will work everywhere *except* over elevated windows.
pub fn warn_if_privilege_limited() {
    if cfg!(target_os = "windows") && !is_elevated() {
        tracing::warn!(
            "running unelevated: hotkeys will not fire while an administrator window \
             has focus. Run `spotify-control service install` from an Administrator \
             prompt to start elevated at logon."
        );
    }
}

/// Pump the platform event loop forever. `global-hotkey` delivers events only
/// while this runs, on the same thread that created the manager.
#[cfg(target_os = "windows")]
pub fn run_event_loop() -> ! {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, TranslateMessage, MSG,
    };
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    std::process::exit(0)
}

#[cfg(target_os = "macos")]
pub fn run_event_loop() -> ! {
    // Must be the main thread; CFRunLoopRun never returns on its own.
    unsafe { core_foundation_sys::runloop::CFRunLoopRun() };
    std::process::exit(0)
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn run_event_loop() -> ! {
    // The X11 backend runs its own thread, so we just need to stay alive.
    loop {
        std::thread::park();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use global_hotkey::hotkey::{Code, Modifiers};

    const CTRL_ALT: Modifiers = Modifiers::CONTROL.union(Modifiers::ALT);
    const CTRL_ALT_SHIFT: Modifiers = CTRL_ALT.union(Modifiers::SHIFT);

    #[test]
    fn all_five_bindings_are_present() {
        let actions: Vec<_> = default_bindings().into_iter().map(|(a, _)| a).collect();
        assert_eq!(actions, ALL_ACTIONS.to_vec());
    }

    #[test]
    fn defaults_parse_to_the_requested_key_combinations() {
        let map: HashMap<Action, HotKey> = default_bindings().into_iter().collect();

        assert_eq!(map[&Action::VolumeUp].key, Code::ArrowRight);
        assert_eq!(map[&Action::VolumeUp].mods, CTRL_ALT);

        assert_eq!(map[&Action::VolumeDown].key, Code::ArrowLeft);
        assert_eq!(map[&Action::VolumeDown].mods, CTRL_ALT);

        assert_eq!(map[&Action::PlayPause].key, Code::Home);
        assert_eq!(map[&Action::PlayPause].mods, CTRL_ALT);

        assert_eq!(map[&Action::NextTrack].key, Code::ArrowRight);
        assert_eq!(map[&Action::NextTrack].mods, CTRL_ALT_SHIFT);

        assert_eq!(map[&Action::PreviousTrack].key, Code::ArrowLeft);
        assert_eq!(map[&Action::PreviousTrack].mods, CTRL_ALT_SHIFT);
    }

    #[test]
    fn plain_and_shift_variants_stay_distinguishable() {
        // The ids derive from (mods, key), so the arrow-key pairs must differ —
        // otherwise volume and track-skip would be indistinguishable at dispatch.
        let map: HashMap<Action, HotKey> = default_bindings().into_iter().collect();
        assert_ne!(map[&Action::VolumeUp].id(), map[&Action::NextTrack].id());
        assert_ne!(
            map[&Action::VolumeDown].id(),
            map[&Action::PreviousTrack].id()
        );
    }

    #[test]
    fn every_hotkey_id_is_unique() {
        let ids: std::collections::HashSet<u32> =
            default_bindings().iter().map(|(_, h)| h.id()).collect();
        assert_eq!(ids.len(), 5, "hotkey ids collide, dispatch would misfire");
    }

    #[test]
    fn shift_modifier_is_only_on_track_bindings() {
        for (action, hotkey) in default_bindings() {
            let has_shift = hotkey.mods.contains(Modifiers::SHIFT);
            let expected = matches!(action, Action::NextTrack | Action::PreviousTrack);
            assert_eq!(has_shift, expected, "wrong SHIFT modifier on {action:?}");
            // Hyprland setups put window moves on SUPER + CTRL + ALT + arrows.
            assert!(!hotkey.mods.contains(Modifiers::SUPER), "SUPER on {action:?}");
        }
    }

    #[test]
    fn accepts_super_and_cmd_spellings_of_the_super_key() {
        for spec in ["Ctrl+Super+Alt+ArrowRight", "Ctrl+Cmd+Alt+ArrowRight"] {
            let parsed: HotKey = spec.parse().unwrap_or_else(|e| panic!("{spec}: {e}"));
            assert_eq!(parsed.key, Code::ArrowRight, "{spec}");
            assert_eq!(parsed.mods, CTRL_ALT.union(Modifiers::SUPER), "{spec}");
        }
    }

    #[test]
    fn action_ids_round_trip() {
        for action in ALL_ACTIONS {
            assert_eq!(Action::from_id(action.id()), Some(action));
        }
        assert_eq!(Action::from_id("nope"), None);
    }

    #[test]
    fn defaults_translate_to_xdg_triggers() {
        let b = Bindings::default();
        assert_eq!(xdg_trigger(&b.volume_up).as_deref(), Some("CTRL+ALT+Right"));
        assert_eq!(xdg_trigger(&b.play_pause).as_deref(), Some("CTRL+ALT+Home"));
        assert_eq!(xdg_trigger(&b.previous_track).as_deref(), Some("CTRL+ALT+SHIFT+Left"));
    }

    #[test]
    fn xdg_triggers_cover_letters_digits_and_function_keys() {
        assert_eq!(xdg_trigger("Super+KeyP").as_deref(), Some("LOGO+p"));
        assert_eq!(xdg_trigger("Ctrl+Digit5").as_deref(), Some("CTRL+5"));
        assert_eq!(xdg_trigger("Alt+F12").as_deref(), Some("ALT+F12"));
        assert_eq!(xdg_trigger("Ctrl+PageDown").as_deref(), Some("CTRL+Page_Down"));
        assert_eq!(xdg_trigger("Ctrl+Semicolon"), None);
        assert_eq!(xdg_trigger("garbage"), None);
    }

    #[test]
    fn rejects_an_unparseable_binding() {
        let bad = Bindings {
            play_pause: "Ctrl+Alt+NotAKey".into(),
            ..Bindings::default()
        };
        let err = parse_bindings(&bad).unwrap_err().to_string();
        assert!(err.contains("play/pause"), "got: {err}");
        assert!(err.contains("NotAKey"), "got: {err}");
    }

    #[test]
    fn rejects_two_actions_bound_to_the_same_keys() {
        // Hotkey ids come from (mods, key) alone, so a duplicate would make
        // dispatch pick whichever won the HashMap insert.
        let clashing = Bindings {
            next_track: "Ctrl+Alt+ArrowRight".into(),
            ..Bindings::default()
        };
        let err = parse_bindings(&clashing).unwrap_err().to_string();
        assert!(err.contains("volume up"), "got: {err}");
        assert!(err.contains("next track"), "got: {err}");
    }

    #[test]
    fn descriptions_render_the_configured_specs() {
        let map: HashMap<Action, String> =
            describe_bindings(&Bindings::default()).into_iter().collect();
        assert_eq!(map[&Action::VolumeUp], "Ctrl + Alt + ArrowRight");
    }

    #[test]
    fn descriptions_cover_every_binding() {
        let b = Bindings::default();
        let described: Vec<_> = describe_bindings(&b).into_iter().map(|(a, _)| a).collect();
        let bound: Vec<_> = parse_bindings(&b).unwrap().into_iter().map(|(a, _)| a).collect();
        assert_eq!(described, bound);
    }

    #[test]
    fn debouncer_allows_the_first_press() {
        let mut d = Debouncer::new();
        assert!(d.allow_at(Action::NextTrack, std::time::Instant::now()));
    }

    #[test]
    fn debouncer_drops_fast_repeats() {
        let mut d = Debouncer::new();
        let t0 = std::time::Instant::now();
        assert!(d.allow_at(Action::NextTrack, t0));
        assert!(!d.allow_at(Action::NextTrack, t0 + Duration::from_millis(100)));
        assert!(!d.allow_at(Action::NextTrack, t0 + Duration::from_millis(399)));
    }

    #[test]
    fn debouncer_allows_repeats_past_the_interval() {
        let mut d = Debouncer::new();
        let t0 = std::time::Instant::now();
        assert!(d.allow_at(Action::NextTrack, t0));
        assert!(d.allow_at(Action::NextTrack, t0 + Duration::from_millis(401)));
    }

    #[test]
    fn debouncer_tracks_actions_independently() {
        let mut d = Debouncer::new();
        let t0 = std::time::Instant::now();
        assert!(d.allow_at(Action::NextTrack, t0));
        // A different action must not be throttled by the first one.
        assert!(d.allow_at(Action::PreviousTrack, t0));
        assert!(d.allow_at(Action::PlayPause, t0));
    }

    #[test]
    fn a_held_key_does_not_skip_a_pile_of_tracks() {
        let mut d = Debouncer::new();
        let t0 = std::time::Instant::now();
        assert!(d.allow_at(Action::NextTrack, t0));
        assert!(!d.allow_at(Action::NextTrack, t0 + Duration::from_millis(150)));
    }

    #[test]
    fn debouncer_does_not_slide_the_window_on_rejected_presses() {
        // A rejected press must not push the deadline out, or holding a key
        // could starve the action forever.
        let mut d = Debouncer::new();
        let t0 = std::time::Instant::now();
        assert!(d.allow_at(Action::NextTrack, t0));
        assert!(!d.allow_at(Action::NextTrack, t0 + Duration::from_millis(300)));
        assert!(d.allow_at(Action::NextTrack, t0 + Duration::from_millis(410)));
    }
}
