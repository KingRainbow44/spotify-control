use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use global_hotkey::{GlobalHotKeyEvent, HotKeyState};
use spotify_control::auth::FileTokenProvider;
use spotify_control::config::{self, Config};
use spotify_control::controller::{Controller, Feedback};
use spotify_control::hotkeys::{self, Action, Debouncer, HotkeyRegistry};
use spotify_control::logging;
use spotify_control::osd;
use spotify_control::service;
use spotify_control::spotify::{SpotifyClient, SCOPES};
use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver};

#[derive(Parser)]
#[command(
    name = "spotify-control",
    version,
    about = "Global hotkeys that drive your active Spotify device"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Enable debug logging.
    #[arg(long, global = true)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Write the config file with your Spotify app's client ID.
    Init {
        #[arg(long)]
        client_id: String,
        /// Percentage points per volume keypress.
        #[arg(long, default_value_t = 5)]
        volume_step: u8,
        /// Loopback port for the OAuth redirect; must match your app's settings.
        #[arg(long, default_value_t = 8888)]
        port: u16,
    },
    /// Authorize with Spotify in your browser.
    Login,
    /// Run the hotkey daemon in the foreground.
    Run,
    /// Show config, authorization, service and playback state.
    Status,
    /// Trigger one action directly, without hotkeys.
    Send {
        #[arg(value_enum)]
        action: CliAction,
    },
    /// Manage the auto-start service.
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
}

#[derive(Subcommand)]
enum ServiceCommand {
    /// Register the daemon to start at logon.
    Install,
    /// Remove the auto-start entry.
    Uninstall,
    /// Report whether the auto-start entry exists.
    Status,
}

#[derive(Copy, Clone, ValueEnum)]
enum CliAction {
    VolumeUp,
    VolumeDown,
    PlayPause,
    Next,
    Previous,
}

impl From<CliAction> for Action {
    fn from(a: CliAction) -> Self {
        match a {
            CliAction::VolumeUp => Action::VolumeUp,
            CliAction::VolumeDown => Action::VolumeDown,
            CliAction::PlayPause => Action::PlayPause,
            CliAction::Next => Action::NextTrack,
            CliAction::Previous => Action::PreviousTrack,
        }
    }
}

/// Task Scheduler and Explorer each allocate a fresh console for this process,
/// and it would sit on the desktop for as long as the daemon runs. A shell that
/// launched us is attached to that same console, so the attached-process count
/// tells the two apart — only a console that is ours alone is ours to hide.
///
/// Staying a console-subsystem binary matters: the GUI subsystem would stop the
/// shell waiting for the CLI subcommands, silently losing any redirected or
/// piped output along with the exit code.
#[cfg(windows)]
fn hide_own_console() {
    use windows_sys::Win32::System::Console::{GetConsoleProcessList, GetConsoleWindow};
    use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};

    unsafe {
        let mut pids = [0u32; 2];
        if GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as u32) != 1 {
            return;
        }
        let console = GetConsoleWindow();
        if !console.is_null() {
            ShowWindow(console, SW_HIDE);
        }
    }
}

#[cfg(not(windows))]
fn hide_own_console() {}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Only the daemon outlives its console and needs a log file to fall back on.
    if matches!(cli.command, Commands::Run) {
        // Before the first log line, so nothing flashes up on the desktop.
        hide_own_console();
        logging::init_daemon(cli.verbose);
    } else {
        logging::init_cli(cli.verbose);
    }

    match cli.command {
        // The daemon owns the main thread: `global-hotkey` requires the manager
        // and the platform event loop to live on the same thread. Its errors go
        // through tracing too — under the logon task stderr goes nowhere.
        Commands::Run => run_daemon().inspect_err(|e| tracing::error!("{e:#}")),

        Commands::Init {
            client_id,
            volume_step,
            port,
        } => cmd_init(client_id, volume_step, port),

        Commands::Service { command } => cmd_service(command),

        other => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(async move {
                match other {
                    Commands::Login => cmd_login().await,
                    Commands::Status => cmd_status().await,
                    Commands::Send { action } => cmd_send(action.into()).await,
                    _ => unreachable!("handled synchronously above"),
                }
            }),
    }
}

fn build_controller(cfg: &Config) -> Result<Controller<FileTokenProvider>> {
    let provider = FileTokenProvider::new(cfg.client_id.clone(), config::token_path()?);
    Ok(Controller::new(SpotifyClient::new(provider), cfg.volume_step))
}

fn cmd_init(client_id: String, volume_step: u8, port: u16) -> Result<()> {
    let cfg = Config {
        client_id,
        volume_step,
        redirect_port: port,
        bindings: Default::default(),
    };
    let path = config::config_path()?;
    cfg.save_to(&path)?;

    println!("Wrote {}", path.display());
    println!(
        "\nAdd this exact redirect URI to your app at\n  \
         https://developer.spotify.com/dashboard\n\n  {}\n\nThen run: spotify-control login",
        cfg.redirect_uri()
    );
    Ok(())
}

async fn cmd_login() -> Result<()> {
    let cfg = Config::load()?;
    let path = config::token_path()?;

    spotify_control::auth::login(
        spotify_control::auth::ACCOUNTS_BASE,
        &cfg.client_id,
        cfg.redirect_port,
        SCOPES,
        &path,
    )
    .await?;

    println!("Authorized. Tokens stored at {}", path.display());
    println!("Start the daemon with: spotify-control run");
    Ok(())
}

async fn cmd_send(action: Action) -> Result<()> {
    let cfg = Config::load()?;
    let feedback = build_controller(&cfg)?
        .handle(action)
        .await
        .with_context(|| format!("{} failed", action.label()))?;
    println!("{} sent", action.label());

    // The popup lives on its own thread, so a one-shot command has to outlast
    // it deliberately or the process exits before anything is drawn.
    osd::show(&feedback.label(), feedback.hold());
    tokio::time::sleep(feedback.hold() + osd::FADE_TAIL).await;
    Ok(())
}

async fn cmd_status() -> Result<()> {
    let cfg_path = config::config_path()?;
    let loaded = Config::load();
    let binding_cfg = loaded
        .as_ref()
        .map(|c| c.bindings.clone())
        .unwrap_or_default();

    println!("Bindings");
    for (action, keys) in hotkeys::describe_bindings(&binding_cfg) {
        println!("  {keys:<28} {}", action.label());
    }

    println!("\nConfig      {}", cfg_path.display());

    let cfg = match loaded {
        Ok(c) => {
            println!("  client_id   {}", c.client_id);
            println!("  volume step {}%", c.volume_step);
            println!("  redirect    {}", c.redirect_uri());
            Some(c)
        }
        Err(e) => {
            println!("  not configured ({e})");
            None
        }
    };

    println!(
        "\nAuto-start  {}",
        if service::is_installed() {
            "installed"
        } else {
            "not installed"
        }
    );

    if cfg!(target_os = "windows") {
        println!(
            "Privileges  {}",
            if hotkeys::is_elevated() {
                "elevated (works over administrator windows)"
            } else {
                "not elevated (hotkeys suppressed over administrator windows)"
            }
        );
    }

    let Some(cfg) = cfg else { return Ok(()) };

    println!("\nPlayback");
    match build_controller(&cfg)?.client().playback_state().await {
        Ok(Some(state)) => {
            let device = state
                .device
                .map(|d| {
                    format!(
                        "{} ({}%{})",
                        d.name,
                        d.volume_percent.unwrap_or(0),
                        if d.supports_volume { "" } else { ", fixed volume" }
                    )
                })
                .unwrap_or_else(|| "unknown device".into());
            println!(
                "  {} on {device}",
                if state.is_playing { "playing" } else { "paused" }
            );
        }
        Ok(None) => println!("  no active device"),
        Err(e) => println!("  unavailable ({e})"),
    }
    Ok(())
}

fn cmd_service(command: ServiceCommand) -> Result<()> {
    match command {
        ServiceCommand::Install => println!("{}", service::install()?),
        ServiceCommand::Uninstall => println!("{}", service::uninstall()?),
        ServiceCommand::Status => println!(
            "{}",
            if service::is_installed() {
                "Auto-start is installed."
            } else {
                "Auto-start is not installed."
            }
        ),
    }
    Ok(())
}

fn run_daemon() -> Result<()> {
    let cfg = Config::load()?;
    let (actions, presses) = mpsc::channel();

    #[cfg(all(unix, not(target_os = "macos")))]
    if spotify_control::wayland::is_session() {
        spawn_action_worker(cfg.clone(), presses)?;
        return tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(spotify_control::wayland::listen(&cfg.bindings, actions));
    }

    // global-hotkey latches the handler on its first event, and the X11 backend
    // can emit one as soon as a key is grabbed, so this goes in before registering.
    let id_map: HashMap<u32, Action> = hotkeys::parse_bindings(&cfg.bindings)?
        .into_iter()
        .map(|(action, hotkey)| (hotkey.id(), action))
        .collect();
    GlobalHotKeyEvent::set_event_handler(Some(move |event| {
        if let Some(action) = pressed_action(&id_map, event) {
            let _ = actions.send(action);
        }
    }));

    // Register before spawning the worker so the user sees binding problems first.
    let registry = HotkeyRegistry::register_all(&cfg.bindings)?;

    hotkeys::warn_if_privilege_limited();

    let active = registry.active_actions();
    let descriptions = hotkeys::describe_bindings(&cfg.bindings);
    tracing::info!("listening for hotkeys:");
    for (action, keys) in descriptions {
        if active.contains(&action) {
            tracing::info!("  {keys} -> {}", action.label());
        }
    }
    for conflict in registry.conflicts() {
        // Recoverable: free the key in whatever app holds it and restart. Warn
        // rather than error so it doesn't read as a fault in this program.
        tracing::warn!(
            "{} is inactive for now: {} is currently held by another application. \
             Free it there and restart to enable it. ({})",
            conflict.action.label(),
            conflict.keys,
            conflict.reason
        );
    }

    spawn_action_worker(cfg, presses)?;

    // Keeps the registry alive for the life of the process; never returns.
    let _registry = registry;
    hotkeys::run_event_loop();
}

/// API calls happen off the event-loop thread; the event loop must never block.
fn spawn_action_worker(cfg: Config, presses: Receiver<Action>) -> Result<()> {
    std::thread::Builder::new()
        .name("spotify-actions".into())
        .spawn(move || {
            if let Err(e) = action_worker(cfg, presses) {
                tracing::error!("action worker stopped: {e:#}");
                std::process::exit(1);
            }
        })
        .context("could not spawn the action worker thread")?;
    Ok(())
}

/// An action slower than this leaves the user staring at nothing, so anything
/// queued behind it is a re-press rather than fresh intent.
const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(1);

/// How long to keep gathering volume presses before writing.
///
/// A knob bound to the volume keys emits detents far faster than the Web API
/// can answer, so a spin becomes one request instead of twenty. Crucially this
/// *accumulates* rather than throttles: every detent still counts, so the knob
/// stays 1:1 with the level instead of silently losing turns.
const VOLUME_COALESCE: std::time::Duration = std::time::Duration::from_millis(100);

fn volume_detent(action: Action) -> Option<i32> {
    match action {
        Action::VolumeUp => Some(1),
        Action::VolumeDown => Some(-1),
        _ => None,
    }
}

fn pressed_action(id_map: &HashMap<u32, Action>, event: GlobalHotKeyEvent) -> Option<Action> {
    if event.state != HotKeyState::Pressed {
        return None;
    }
    id_map.get(&event.id).copied()
}

fn report(action: Action, outcome: Result<Feedback>) {
    match outcome {
        Ok(feedback) => osd::show(&feedback.label(), feedback.hold()),
        Err(e) => tracing::error!("{} failed: {e:#}", action.label()),
    }
}

fn action_worker(cfg: Config, receiver: Receiver<Action>) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let controller = build_controller(&cfg)?;
    let mut debouncer = Debouncer::new();

    // A non-volume press that interrupted a volume burst, to run next.
    let mut deferred: Option<Action> = None;

    loop {
        let action = match deferred.take() {
            Some(action) => action,
            None => receiver.recv().context("hotkey channel closed")?,
        };

        // Volume is coalesced, not debounced: the window below is what keeps the
        // request rate sane, so no detent has to be thrown away to protect it.
        if let Some(first) = volume_detent(action) {
            let mut detents = first;
            let deadline = std::time::Instant::now() + VOLUME_COALESCE;

            loop {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let Ok(next) = receiver.recv_timeout(remaining) else {
                    break;
                };
                match volume_detent(next) {
                    Some(detent) => detents += detent,
                    // Don't swallow a skip that lands mid-spin.
                    None => {
                        deferred = Some(next);
                        break;
                    }
                }
            }

            tracing::debug!(detents, "applying coalesced volume");
            report(action, runtime.block_on(controller.nudge_volume_by(detents)));
            continue;
        }

        if !debouncer.allow(action) {
            tracing::debug!("throttled repeat of {}", action.label());
            continue;
        }

        // One action at a time; a failed action must not take the daemon down.
        let started = std::time::Instant::now();
        report(action, runtime.block_on(controller.handle(action)));

        // Launching Spotify and waiting for it to appear can take ~15s, and a
        // user who sees nothing happen presses again. Those presses queue up and
        // the debouncer can't catch them — it stamps events when they're
        // dequeued, not when they arrived — so one skip would become five.
        if started.elapsed() > STALE_AFTER {
            let dropped = receiver.try_iter().count();
            if dropped > 0 {
                tracing::debug!(dropped, "discarded presses queued during a slow action");
            }
        }
    }
}
