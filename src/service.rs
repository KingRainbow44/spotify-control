use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::process::Command;

pub const SERVICE_NAME: &str = "spotify-control";
pub const WINDOWS_TASK_NAME: &str = "SpotifyControl";
pub const MACOS_LABEL: &str = "com.github.spotify-control";

fn exe_path() -> Result<PathBuf> {
    std::env::current_exe().context("could not determine the running executable's path")
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Task Scheduler definition for an at-logon daemon.
pub fn windows_task_xml(exe: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Global Spotify playback hotkeys.</Description>
    <URI>\{task}</URI>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <LogonType>InteractiveToken</LogonType>
      <!-- High integrity is required or UIPI silently swallows WM_HOTKEY
           whenever an elevated window holds focus. A logon-triggered task is
           also the only way to get this without a UAC prompt every boot. -->
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>3</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
      <Arguments>run</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
        task = WINDOWS_TASK_NAME,
        exe = xml_escape(exe)
    )
}

pub fn macos_plist(exe: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>run</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ProcessType</key>
  <string>Interactive</string>
</dict>
</plist>
"#,
        label = MACOS_LABEL,
        exe = xml_escape(exe)
    )
}

pub fn linux_unit(exe: &str) -> String {
    format!(
        "[Unit]\n\
         Description=Global Spotify playback hotkeys\n\
         After=graphical-session.target\n\
         PartOf=graphical-session.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exe} run\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=graphical-session.target\n"
    )
}

// ---------------------------------------------------------------- Windows ---

#[cfg(target_os = "windows")]
fn utf16le_with_bom(s: &str) -> Vec<u8> {
    // schtasks /XML expects UTF-16 to match the declaration in the document.
    let mut bytes = vec![0xFF, 0xFE];
    for unit in s.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

#[cfg(target_os = "windows")]
pub fn install() -> Result<String> {
    // Both the logon trigger and HighestAvailable need an elevated caller;
    // schtasks otherwise just says "Access is denied" with no hint why.
    if !crate::hotkeys::is_elevated() {
        bail!(
            "registering the logon task requires elevation — re-run this from an \
             Administrator prompt (belonging to the Administrators group is not \
             enough, the process itself must be elevated)"
        );
    }

    let exe = exe_path()?;
    let xml = windows_task_xml(&exe.to_string_lossy());

    let path = std::env::temp_dir().join("spotify-control-task.xml");
    std::fs::write(&path, utf16le_with_bom(&xml))?;

    let out = Command::new("schtasks")
        .args(["/Create", "/TN", WINDOWS_TASK_NAME, "/XML"])
        .arg(&path)
        .arg("/F")
        .output()
        .context("could not run schtasks")?;

    let _ = std::fs::remove_file(&path);

    if !out.status.success() {
        bail!(
            "schtasks failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(format!(
        "Registered scheduled task \"{WINDOWS_TASK_NAME}\" to run at logon."
    ))
}

#[cfg(target_os = "windows")]
pub fn uninstall() -> Result<String> {
    let out = Command::new("schtasks")
        .args(["/Delete", "/TN", WINDOWS_TASK_NAME, "/F"])
        .output()
        .context("could not run schtasks")?;

    if !out.status.success() {
        bail!(
            "schtasks failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(format!("Removed scheduled task \"{WINDOWS_TASK_NAME}\"."))
}

#[cfg(target_os = "windows")]
pub fn is_installed() -> bool {
    Command::new("schtasks")
        .args(["/Query", "/TN", WINDOWS_TASK_NAME])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ------------------------------------------------------------------ macOS ---

#[cfg(target_os = "macos")]
fn plist_path() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .context("no home directory")?
        .join("Library/LaunchAgents")
        .join(format!("{MACOS_LABEL}.plist")))
}

#[cfg(target_os = "macos")]
pub fn install() -> Result<String> {
    let exe = exe_path()?;
    let path = plist_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, macos_plist(&exe.to_string_lossy()))?;

    // Reload so an existing agent picks up the new definition.
    let _ = Command::new("launchctl").arg("unload").arg(&path).output();
    let out = Command::new("launchctl")
        .args(["load", "-w"])
        .arg(&path)
        .output()
        .context("could not run launchctl")?;

    if !out.status.success() {
        bail!(
            "launchctl load failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(format!("Installed LaunchAgent at {}.", path.display()))
}

#[cfg(target_os = "macos")]
pub fn uninstall() -> Result<String> {
    let path = plist_path()?;
    if !path.exists() {
        return Ok("No LaunchAgent installed.".to_string());
    }
    let _ = Command::new("launchctl").arg("unload").arg(&path).output();
    std::fs::remove_file(&path)?;
    Ok(format!("Removed LaunchAgent at {}.", path.display()))
}

#[cfg(target_os = "macos")]
pub fn is_installed() -> bool {
    plist_path().map(|p| p.exists()).unwrap_or(false)
}

// ------------------------------------------------------------------ Linux ---

#[cfg(all(unix, not(target_os = "macos")))]
fn unit_path() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .context("no config directory")?
        .join("systemd/user")
        .join(format!("{SERVICE_NAME}.service")))
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn install() -> Result<String> {
    let exe = exe_path()?;
    let path = unit_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, linux_unit(&exe.to_string_lossy()))?;

    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .output();
    let out = Command::new("systemctl")
        .args(["--user", "enable", "--now", SERVICE_NAME])
        .output()
        .context("could not run systemctl")?;

    if !out.status.success() {
        bail!(
            "systemctl enable failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(format!("Installed and started systemd user unit at {}.", path.display()))
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn uninstall() -> Result<String> {
    let path = unit_path()?;
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", SERVICE_NAME])
        .output();

    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .output();
    Ok(format!("Removed systemd user unit at {}.", path.display()))
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn is_installed() -> bool {
    unit_path().map(|p| p.exists()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_task_runs_the_daemon_subcommand() {
        let xml = windows_task_xml(r"C:\Tools\spotify-control.exe");
        assert!(xml.contains(r"<Command>C:\Tools\spotify-control.exe</Command>"));
        assert!(xml.contains("<Arguments>run</Arguments>"));
        assert!(xml.contains("<LogonTrigger>"));
    }

    #[test]
    fn windows_task_has_no_execution_time_limit() {
        // schtasks otherwise defaults to 72 hours and would kill the daemon.
        assert!(windows_task_xml("x.exe").contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
    }

    #[test]
    fn windows_task_requests_elevation() {
        // Without this, UIPI blocks hotkeys while an admin window has focus.
        let xml = windows_task_xml("x.exe");
        assert!(xml.contains("<RunLevel>HighestAvailable</RunLevel>"));
        assert!(!xml.contains("LeastPrivilege"));
        // InteractiveToken keeps it in the user's desktop session.
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
    }

    #[test]
    fn windows_task_survives_battery_and_idle() {
        let xml = windows_task_xml("x.exe");
        assert!(xml.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"));
        assert!(xml.contains("<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>"));
        assert!(xml.contains("<RunOnlyIfIdle>false</RunOnlyIfIdle>"));
    }

    #[test]
    fn xml_paths_are_escaped() {
        let xml = windows_task_xml(r"C:\A & B\app.exe");
        assert!(xml.contains(r"C:\A &amp; B\app.exe"));
        assert!(!xml.contains("A & B"));
    }

    #[test]
    fn macos_plist_is_well_formed_and_runs_at_load() {
        let plist = macos_plist("/usr/local/bin/spotify-control");
        assert!(plist.contains("<string>/usr/local/bin/spotify-control</string>"));
        assert!(plist.contains("<string>run</string>"));
        assert!(plist.contains("<key>RunAtLoad</key>"));
        assert!(plist.contains(MACOS_LABEL));
    }

    #[test]
    fn linux_unit_starts_the_daemon_and_restarts_on_failure() {
        let unit = linux_unit("/home/me/.cargo/bin/spotify-control");
        assert!(unit.contains("ExecStart=/home/me/.cargo/bin/spotify-control run"));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("WantedBy=graphical-session.target"));
    }

    #[test]
    fn linux_unit_has_all_three_sections() {
        let unit = linux_unit("x");
        for section in ["[Unit]", "[Service]", "[Install]"] {
            assert!(unit.contains(section), "missing {section}");
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn install_explains_that_elevation_is_required() {
        // Only meaningful unelevated; when elevated the real path would go on
        // to register an actual scheduled task, which a test must not do.
        if crate::hotkeys::is_elevated() {
            return;
        }
        let err = install().unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("elevation"), "got: {msg}");
        assert!(msg.contains("Administrator"), "got: {msg}");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn utf16_output_starts_with_a_little_endian_bom() {
        let bytes = utf16le_with_bom("ab");
        assert_eq!(&bytes[..2], &[0xFF, 0xFE]);
        assert_eq!(&bytes[2..], &[b'a', 0, b'b', 0]);
    }
}
