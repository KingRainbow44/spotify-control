use anyhow::{Context, Result};
use std::process::{Command, Stdio};

/// A candidate way to start the Spotify desktop client. Different install
/// methods (native, Store, flatpak, snap) need different invocations, so we try
/// them in order until one spawns.
#[derive(Debug, Clone, PartialEq)]
pub struct LaunchCandidate {
    pub program: String,
    pub args: Vec<String>,
}

impl LaunchCandidate {
    fn new(program: &str, args: &[&str]) -> Self {
        Self {
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
        }
    }
}

#[cfg(target_os = "windows")]
pub fn launch_candidates() -> Vec<LaunchCandidate> {
    let mut out = Vec::new();

    // The per-user install is the common case and the cheapest to hit.
    if let Ok(appdata) = std::env::var("APPDATA") {
        let exe = format!("{appdata}\\Spotify\\Spotify.exe");
        if std::path::Path::new(&exe).exists() {
            out.push(LaunchCandidate::new(&exe, &[]));
        }
    }

    // Covers the Microsoft Store build, which has no stable exe path but does
    // register the `spotify:` protocol handler.
    out.push(LaunchCandidate::new("cmd", &["/C", "start", "", "spotify:"]));
    out
}

#[cfg(target_os = "macos")]
pub fn launch_candidates() -> Vec<LaunchCandidate> {
    vec![LaunchCandidate::new("open", &["-a", "Spotify"])]
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn launch_candidates() -> Vec<LaunchCandidate> {
    vec![
        LaunchCandidate::new("spotify", &[]),
        LaunchCandidate::new("flatpak", &["run", "com.spotify.Client"]),
        LaunchCandidate::new("snap", &["run", "spotify"]),
    ]
}

/// True if the tasklist output actually lists a process, rather than the
/// "INFO: No tasks are running..." banner it prints on no match.
#[cfg(target_os = "windows")]
pub fn parse_tasklist_output(out: &str) -> bool {
    out.to_ascii_lowercase().contains("spotify.exe")
}

#[cfg(unix)]
pub fn parse_pgrep_output(out: &str) -> bool {
    out.lines().any(|l| !l.trim().is_empty())
}

#[cfg(target_os = "windows")]
pub fn is_running() -> bool {
    Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq Spotify.exe", "/NH"])
        .stderr(Stdio::null())
        .output()
        .map(|o| parse_tasklist_output(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or(false)
}

#[cfg(unix)]
pub fn is_running() -> bool {
    // macOS names the process "Spotify"; Linux builds use "spotify".
    ["Spotify", "spotify"].iter().any(|name| {
        Command::new("pgrep")
            .args(["-x", name])
            .stderr(Stdio::null())
            .output()
            .map(|o| parse_pgrep_output(&String::from_utf8_lossy(&o.stdout)))
            .unwrap_or(false)
    })
}

/// Start the desktop client if it is not already up. Returns whether a launch
/// was actually attempted.
pub fn ensure_running() -> Result<bool> {
    if is_running() {
        tracing::debug!("Spotify is already running");
        return Ok(false);
    }

    let candidates = launch_candidates();
    let mut last_err = None;

    for candidate in &candidates {
        tracing::debug!(program = %candidate.program, "trying to launch Spotify");
        match Command::new(&candidate.program)
            .args(&candidate.args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(_) => {
                tracing::info!(program = %candidate.program, "launched Spotify");
                return Ok(true);
            }
            Err(e) => last_err = Some(e),
        }
    }

    Err(last_err
        .map(anyhow::Error::from)
        .unwrap_or_else(|| anyhow::anyhow!("no launch candidates for this platform")))
        .context("could not start the Spotify desktop app — is it installed?")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn always_offers_at_least_one_candidate() {
        assert!(!launch_candidates().is_empty());
    }

    #[test]
    fn candidates_have_non_empty_programs() {
        for c in launch_candidates() {
            assert!(!c.program.trim().is_empty(), "empty program in {c:?}");
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_always_falls_back_to_the_protocol_handler() {
        let candidates = launch_candidates();
        let last = candidates.last().unwrap();
        assert_eq!(last.program, "cmd");
        assert!(last.args.contains(&"spotify:".to_string()));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn tasklist_banner_is_not_mistaken_for_a_process() {
        assert!(!parse_tasklist_output(
            "INFO: No tasks are running which match the specified criteria."
        ));
        assert!(!parse_tasklist_output(""));
        assert!(parse_tasklist_output(
            "Spotify.exe                  12345 Console                    1    180,000 K"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn pgrep_output_parsing() {
        assert!(!parse_pgrep_output(""));
        assert!(!parse_pgrep_output("\n  \n"));
        assert!(parse_pgrep_output("4821\n"));
    }
}
