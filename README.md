# spotify-control

Global media hotkeys for Spotify. The bindings drive your **active Spotify Connect device**
through the Web API, so they work even when the music is playing on a different machine,
your phone, or a speaker.

Primary target is Windows; macOS and Linux (X11) are supported.

## Bindings

| Keys | Action |
| --- | --- |
| `Ctrl` + `Alt` + `Right` | Volume up |
| `Ctrl` + `Alt` + `Left` | Volume down |
| `Ctrl` + `Alt` + `Home` | Play / pause |
| `Ctrl` + `Win` + `Alt` + `Right` | Next track |
| `Ctrl` + `Win` + `Alt` + `Left` | Previous track |

On macOS, `Win` is `Cmd`. The bindings are claimed exclusively — while the daemon runs, other
applications will not receive them.

If another app already holds one of these combinations (a launcher like Raycast, a graphics-driver
utility, etc.), that single binding starts **inactive** and the daemon logs a warning naming it;
the rest still work. Free the key in the other app and restart to enable it, or remap it — every
binding is configurable in `config.json` (see [Configuration](#configuration)). The combination
string uses `Super` (not `Win`) for the Windows/Command key, e.g. `Ctrl+Super+Alt+ArrowRight`.

## Requirements

- Spotify **Premium** (the Web API refuses playback control on free accounts)
- A Spotify application registered at <https://developer.spotify.com/dashboard>

## Setup

1. Create an app in the Spotify dashboard. Note its **Client ID** — there is no secret, this
   uses PKCE.

2. Configure the client ID:

   ```sh
   spotify-control init --client-id <YOUR_CLIENT_ID>
   ```

3. Add the redirect URI it prints to your app's settings in the dashboard. It must match
   **exactly**:

   ```
   http://127.0.0.1:8888/callback
   ```

   Spotify rejects `localhost` for newly registered apps, which is why this uses the loopback IP.

4. Authorize:

   ```sh
   spotify-control login
   ```

5. Run it:

   ```sh
   spotify-control run
   ```

## Running at startup

```sh
spotify-control service install     # start automatically at logon
spotify-control service status
spotify-control service uninstall
```

| Platform | Mechanism |
| --- | --- |
| Windows | Task Scheduler logon task (`SpotifyControl`) |
| macOS | launchd LaunchAgent (`~/Library/LaunchAgents`) |
| Linux | systemd user unit (`systemctl --user`) |

### Windows: hotkeys over administrator windows

Windows' UIPI will not deliver a hotkey to a normal-privilege process while an **elevated**
window has focus. The hotkey registers fine and then simply never fires — a silent failure.

`service install` therefore registers the task with `HighestAvailable` privileges. A
logon-triggered scheduled task is the one mechanism that runs elevated *without* a UAC prompt
on every boot, so the bindings keep working over admin windows (terminals, installers, RegEdit).

Registering that task requires an **elevated** prompt — being in the Administrators group is not
enough, since Windows hands ordinary shells a filtered token. Run `service install` from an
Administrator terminal; it refuses with an explanation otherwise.

Without it everything still works — just not while an elevated window is focused.
`spotify-control status` reports which mode you're in, and the daemon logs a warning at startup.

Started from the service, `run` hides its own console window, so nothing sits on the desktop for
the life of the session. A `run` you started from a terminal keeps its window — the daemon only
hides a console it is the sole owner of.

### Logs

That hidden console means the daemon's output would otherwise go nowhere, so `run` also writes to
a rotating daily log in the `logs` directory beside `config.json`, keeping a week of history:

```
%APPDATA%\spotify-control\logs\spotify-control.<date>.log
```

Startup failures and the fatal errors that stop the daemon land there too. It's the first place to
look if the hotkeys go quiet.

Hotkeys do not fire while the system is locked or while the UAC secure desktop is up. That is
enforced by the OS and is expected.

## Other commands

```sh
spotify-control status              # config, auth, service and playback state
spotify-control send volume-up      # trigger one action without hotkeys
spotify-control run --verbose       # debug logging
```

`send` is the quickest way to confirm authorization works independently of hotkey delivery.

## Configuration

`config.json` lives in your user config directory (`%APPDATA%\spotify-control` on Windows,
`~/.config/spotify-control` on Linux, `~/Library/Application Support/spotify-control` on macOS):

```json
{
  "client_id": "...",
  "volume_step": 5,
  "redirect_port": 8888,
  "bindings": {
    "volume_up": "Ctrl+Alt+ArrowRight",
    "volume_down": "Ctrl+Alt+ArrowLeft",
    "play_pause": "Ctrl+Alt+Home",
    "next_track": "Ctrl+Super+Alt+ArrowRight",
    "previous_track": "Ctrl+Super+Alt+ArrowLeft"
  }
}
```

Binding strings list modifiers first, joined by `+`. Recognised modifiers are `Ctrl`/`Control`,
`Alt`, `Shift`, and `Super`/`Cmd`/`Command` (the Windows/Command key — `Win` is not accepted).
Keys use [`keyboard-types`](https://docs.rs/keyboard-types) names such as `ArrowRight`, `Home`,
`End`, `PageUp`, `Space`, `KeyP`. The `bindings` block is optional; omitted entries fall back to
the defaults above.

Tokens are stored beside it in `tokens.json` and refreshed automatically. On Unix that file is
written `0600`; on Windows it inherits the user-profile ACL.

## Behavior notes

- **No active device.** If Spotify knows about an idle device, it is adopted silently. If it knows
  of none, the local desktop client is launched and adopted once it appears.
- **Volume.** Read-modify-write against the active device, clamped to 0–100. The level is cached
  for 5 seconds so holding a key ramps smoothly without a request per repeat.
- **Fixed-volume devices.** Some Connect targets (TVs, certain speakers) report
  `supports_volume: false` and will refuse volume changes with a clear error.
- **Rate limiting.** Repeats are throttled — 120 ms for volume, 400 ms for track changes — so a
  held key cannot trip Spotify's 429s.

## Known conflicts

- `Ctrl` + `Alt` + `Left`/`Right` is also used by some Intel graphics drivers for screen rotation.
  Disable those hotkeys in Intel Graphics Command Center if arrows rotate your display.
- Linux support is **X11 only**. Wayland does not permit global hotkey grabs through this
  mechanism.

## Development

```sh
cargo test          # unit + mock-server integration tests
cargo clippy --all-targets
```

The Spotify layer is tested against a `wiremock` mock server rather than the live API: request
shapes, volume clamping, device adoption, token refresh, and the 401/403/404/429 error mappings
all have coverage.
