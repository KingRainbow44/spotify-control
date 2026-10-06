use crate::config::Bindings;
use crate::hotkeys::{self, Action, ALL_ACTIONS};
use crate::service::APP_ID;
use anyhow::{bail, Context, Result};
use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
use ashpd::zbus::fdo::{DBusProxy, RequestNameFlags, RequestNameReply};
use futures_util::StreamExt;
use std::sync::mpsc::Sender;

const PORTAL_BUS_NAME: &str = "org.freedesktop.portal.Desktop";
const BACKEND_BUS_PREFIX: &str = "org.freedesktop.impl.portal.desktop.";

/// Wayland gives clients no way to grab keys, so a Wayland session has to go
/// through the GlobalShortcuts portal instead of `global-hotkey`.
pub fn is_session() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
}

/// Binds every action through the GlobalShortcuts portal and forwards
/// activations to `actions`. Only returns on failure, including the portal or
/// its backend restarting, since that silently drops the session and its shortcuts.
pub async fn listen(bindings: &Bindings, actions: Sender<Action>) -> Result<()> {
    ensure_desktop_entry()?;
    // Without this the portal sees an empty app id, and Hyprland binds
    // shortcuts by "<app id>:<shortcut id>".
    ashpd::register_host_app(APP_ID.parse()?)
        .await
        .context("could not register with the desktop portal")?;

    let portal = GlobalShortcuts::new()
        .await
        .context("the desktop portal does not offer GlobalShortcuts")?;
    let bus = DBusProxy::new(portal.connection()).await?;

    // The portal accepts a second registration of the same ids and then never
    // activates it, so a duplicate instance would sit there deaf.
    if bus.request_name(APP_ID.try_into()?, RequestNameFlags::DoNotQueue.into()).await?
        != RequestNameReply::PrimaryOwner
    {
        bail!("another instance of spotify-control is already running");
    }

    let mut activated = portal.receive_activated().await?;
    // Hyprland drops the shortcuts with xdph's Wayland client, and nothing
    // rebinds them when xdph comes back, so watch the backend as well.
    let mut owner_changes = bus.receive_name_owner_changed().await?;

    let session = portal
        .create_session(Default::default())
        .await
        .context("could not open a GlobalShortcuts session")?;
    let mut closed = session.receive_closed().await?;

    let shortcuts: Vec<NewShortcut> = ALL_ACTIONS
        .into_iter()
        .map(|action| {
            let trigger = hotkeys::xdg_trigger(action.spec(bindings));
            NewShortcut::new(action.id(), action.label()).preferred_trigger(trigger.as_deref())
        })
        .collect();

    let bound = portal
        .bind_shortcuts(&session, &shortcuts, None, Default::default())
        .await
        .context("could not bind shortcuts")?
        .response()
        .context("the portal refused the shortcuts")?;

    tracing::info!("registered with the GlobalShortcuts portal as {APP_ID}:");
    for shortcut in bound.shortcuts() {
        // Hyprland leaves this empty: its keys live in the compositor config.
        let trigger = match shortcut.trigger_description() {
            "" => String::new(),
            t => format!(" ({t})"),
        };
        tracing::info!("  {APP_ID}:{}{trigger} -> {}", shortcut.id(), shortcut.description());
    }

    loop {
        tokio::select! {
            event = activated.next() => {
                let Some(event) = event else {
                    bail!("lost the portal's Activated signal stream");
                };
                match Action::from_id(event.shortcut_id()) {
                    Some(action) => actions.send(action).context("action worker is gone")?,
                    None => tracing::debug!("ignoring unknown shortcut {}", event.shortcut_id()),
                }
            }
            change = owner_changes.next() => {
                let Some(change) = change else {
                    bail!("lost the bus's NameOwnerChanged signal stream");
                };
                let args = change.args()?;
                let name = args.name.as_str();
                // A backend appearing for the first time is not a restart.
                if args.old_owner.is_some()
                    && (name == PORTAL_BUS_NAME || name.starts_with(BACKEND_BUS_PREFIX))
                {
                    bail!("{name} restarted");
                }
            }
            _ = closed.next() => bail!("the portal closed the GlobalShortcuts session"),
        }
    }
}

/// The portal's host registry refuses an app id with no desktop entry behind it.
fn ensure_desktop_entry() -> Result<()> {
    let path = dirs::data_dir()
        .context("could not resolve the user data directory")?
        .join("applications")
        .join(format!("{APP_ID}.desktop"));
    let exe = std::env::current_exe().context("could not determine the running executable's path")?;
    let entry = desktop_entry(&exe.to_string_lossy());

    if std::fs::read_to_string(&path).ok().as_deref() != Some(entry.as_str()) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, entry).with_context(|| format!("could not write {}", path.display()))?;
    }
    Ok(())
}

fn desktop_entry(exe: &str) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Spotify Control\n\
         Comment=Global Spotify playback hotkeys\n\
         Exec=\"{exe}\" run\n\
         NoDisplay=true\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_entry_is_hidden_and_runs_the_daemon() {
        let entry = desktop_entry("/home/me/.cargo/bin/spotify-control");
        assert!(entry.starts_with("[Desktop Entry]\n"));
        assert!(entry.contains("Exec=\"/home/me/.cargo/bin/spotify-control\" run\n"));
        assert!(entry.contains("NoDisplay=true\n"));
    }
}
