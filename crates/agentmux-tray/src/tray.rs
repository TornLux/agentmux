//! System-tray icon + dynamic right-click menu.
//!
//! The menu structure is rebuilt every time `refresh_sessions` runs
//! so newly-created / killed sessions show up in five seconds (the
//! poll interval) without needing per-session push events.
//!
//! Icon colour reflects the worst current session state:
//!   * red    — at least one session waiting on tool approval / crashed
//!   * yellow — at least one session running (claude is mid-turn)
//!   * green  — sessions exist, all idle
//!   * gray   — broker reachable but no sessions
//!   * white  — broker unreachable (last poll failed)
//!
//! Menu actions fire async work on the shared tokio runtime; we map
//! `MenuEvent.id()` strings (e.g. `"session:default:hibernate"`)
//! back to (session, action) tuples in `dispatch_menu_id`.

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use tao::event_loop::EventLoopProxy;
use tokio::runtime::Handle;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use tracing::{debug, info, warn};

use crate::broker::{BrokerClient, SessionInfo};
use crate::deeplink::DeepLink;
use crate::UserEvent;

const STATIC_ID_OPEN_WEB: &str = "static:open_web";
const STATIC_ID_QUIT_BROKER: &str = "static:quit_broker";
const STATIC_ID_QUIT_TRAY: &str = "static:quit_tray";
/// Whole-stack restart via broker's POST /restart-agentmux. Broker
/// spawns a detached respawner then exits; the respawner re-runs
/// `agentmux restart` so all three processes (broker + tray + discord)
/// reload their config from disk. Only useful while broker is reachable.
const STATIC_ID_RESTART_ALL: &str = "static:restart_all";
/// Stop everything in one click: discord bot first (so it doesn't
/// churn reconnects when broker drops out), then broker via HTTP
/// `/shutdown`, then exit the tray itself. Useful when you suspect
/// stale processes from a previous run that the wrapper script
/// somehow missed (e.g. `agentmux stop` not run before reboot, or
/// the bot was started outside `agentmux start`).
const STATIC_ID_QUIT_ALL: &str = "static:quit_all";

/// What an icon should look like in a given health state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IconState {
    Disconnected,
    NoSessions,
    AllIdle,
    AnyRunning,
    NeedsAttention,
    /// At least one session is locally-owned (demoted) — broker has
    /// no claude in it, the user's local terminal owns the
    /// transcript. Distinct color so the user remembers without
    /// hovering.
    AnyLocallyOwned,
}

pub struct TrayState {
    icon: TrayIcon,
    broker: Arc<BrokerClient>,
    runtime: Handle,
    proxy: EventLoopProxy<UserEvent>,
    last_snapshot: Vec<SessionInfo>,
    connected: bool,
    last_icon_state: Option<IconState>,
}

impl TrayState {
    pub fn new(
        broker: Arc<BrokerClient>,
        runtime: Handle,
        proxy: EventLoopProxy<UserEvent>,
    ) -> Result<Self> {
        let menu = build_initial_menu()?;
        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("agentmux: starting…")
            .with_icon(build_icon(IconState::Disconnected))
            .build()
            .context("build tray icon")?;
        Ok(Self {
            icon,
            broker,
            runtime,
            proxy,
            last_snapshot: Vec::new(),
            connected: false,
            last_icon_state: None,
        })
    }

    pub fn broker_client(&self) -> Arc<BrokerClient> {
        self.broker.clone()
    }

    pub fn runtime_handle(&self) -> Handle {
        self.runtime.clone()
    }

    /// Drain tray-icon's menu-event channel. Called every iteration of
    /// the main loop; non-blocking try_recv.
    pub fn drain_menu_events(&self) {
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            self.dispatch_menu_id(ev.id().0.as_str());
        }
    }

    fn dispatch_menu_id(&self, id: &str) {
        debug!("menu click id={id}");
        match id {
            STATIC_ID_OPEN_WEB => {
                let url = self.broker.http_url();
                let _ = open_in_browser(&url);
            }
            STATIC_ID_QUIT_BROKER => {
                let broker = self.broker.clone();
                self.runtime.spawn(async move {
                    if let Err(e) = broker.shutdown().await {
                        warn!("shutdown broker: {e}");
                    }
                });
            }
            STATIC_ID_RESTART_ALL => {
                info!("restart-agentmux requested via menu");
                let broker = self.broker.clone();
                self.runtime.spawn(async move {
                    if let Err(e) = broker.restart_agentmux().await {
                        // Most common failure: AGENTMUX_LAUNCHER unset
                        // (broker started outside the wrapper). Surface
                        // the broker's 503 message so the user knows
                        // they need to restart from the CLI instead.
                        warn!("restart-agentmux: {e}");
                    }
                });
            }
            STATIC_ID_QUIT_TRAY => {
                info!("quit tray requested via menu");
                std::process::exit(0);
            }
            STATIC_ID_QUIT_ALL => {
                info!("quit all requested via menu");
                let broker = self.broker.clone();
                self.runtime.spawn(async move {
                    // Discord first: if we shut broker down before
                    // killing the bot, the bot's WS subscriber would
                    // see a disconnect and start its reconnect loop,
                    // which is harmless but pollutes logs.
                    // taskkill is fire-and-forget — we just want the
                    // bot gone, don't care if it wasn't running.
                    let _ = std::process::Command::new("taskkill")
                        .args(["/F", "/IM", "platform-discord.exe"])
                        .output();
                    if let Err(e) = broker.shutdown().await {
                        warn!("shutdown broker: {e}");
                    }
                    // Brief wait so broker can release its named
                    // pipe + HTTP port before tray exits — otherwise
                    // a quick `agentmux start` after Quit all races
                    // the still-closing socket.
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    std::process::exit(0);
                });
            }
            other => {
                if let Some((session, action)) = parse_session_id(other) {
                    self.run_session_action(session, action);
                } else {
                    warn!("unhandled menu id: {other}");
                }
            }
        }
    }

    fn run_session_action(&self, session: String, action: SessionAction) {
        let broker = self.broker.clone();
        let proxy = self.proxy.clone();
        self.runtime.spawn(async move {
            let result = match action {
                SessionAction::Attach => {
                    // Reuse the deeplink path so the launcher logic
                    // (which finds claude-attach + spawns wt.exe) lives
                    // in one place.
                    let _ = proxy.send_event(UserEvent::DeepLinkReceived(
                        DeepLink::OpenSession { name: session.clone() },
                    ));
                    return;
                }
                SessionAction::Interrupt => broker.interrupt(&session).await,
                SessionAction::Hibernate => broker.hibernate(&session).await,
                SessionAction::Restart => broker.restart(&session).await,
                SessionAction::Kill => broker.kill(&session).await,
                SessionAction::ReAdopt => broker.re_adopt(&session).await,
            };
            if let Err(e) = result {
                warn!("session action {action:?} on {session}: {e}");
            }
        });
    }

    pub fn refresh_sessions(&mut self, snapshot: &[SessionInfo]) -> Result<()> {
        // Skip rebuild if nothing changed — Win32 tray menu rebuilds
        // are visible to the user as a brief flicker, so we only do
        // them when the displayed content really changed.
        if !sessions_changed(&self.last_snapshot, snapshot) {
            return Ok(());
        }
        self.last_snapshot = snapshot.to_vec();
        let menu = build_session_menu(snapshot)?;
        self.icon
            .set_menu(Some(Box::new(menu)));
        self.refresh_icon_and_tooltip();
        Ok(())
    }

    pub fn set_connected(&mut self, connected: bool) -> Result<()> {
        if self.connected == connected {
            return Ok(());
        }
        self.connected = connected;
        if !connected {
            self.last_snapshot.clear();
            // Replace menu with a "broker offline" stub.
            let menu = build_offline_menu()?;
            self.icon.set_menu(Some(Box::new(menu)));
        }
        self.refresh_icon_and_tooltip();
        Ok(())
    }

    fn refresh_icon_and_tooltip(&mut self) {
        // Priority ordering: a crashed session (NeedsAttention) trumps
        // everything because it implies the user lost work. Locally-
        // owned beats AnyRunning because it's a UI affordance the
        // user explicitly opted into and likely wants visible. Plain
        // running activity stays "yellow" as before.
        let state = if !self.connected {
            IconState::Disconnected
        } else if self.last_snapshot.is_empty() {
            IconState::NoSessions
        } else if self.last_snapshot.iter().any(needs_attention) {
            IconState::NeedsAttention
        } else if self.last_snapshot.iter().any(is_locally_owned) {
            IconState::AnyLocallyOwned
        } else if self.last_snapshot.iter().any(is_running) {
            IconState::AnyRunning
        } else {
            IconState::AllIdle
        };

        if self.last_icon_state != Some(state) {
            let _ = self.icon.set_icon(Some(build_icon(state)));
            self.last_icon_state = Some(state);
        }

        let tooltip = format_tooltip(self.connected, &self.last_snapshot);
        let _ = self.icon.set_tooltip(Some(tooltip));
    }
}

#[derive(Debug, Clone, Copy)]
enum SessionAction {
    Attach,
    Interrupt,
    Hibernate,
    Restart,
    Kill,
    /// Re-adopt a LocallyOwned session: tray POSTs /adopt; broker
    /// spawns claude with --resume.
    ReAdopt,
}

fn parse_session_id(id: &str) -> Option<(String, SessionAction)> {
    // Format: `session:<name>:<verb>`. Name is taken between the two
    // outermost colons so a colon in the name (rare in practice but
    // not forbidden) doesn't break parsing.
    let body = id.strip_prefix("session:")?;
    let (name, verb) = body.rsplit_once(':')?;
    let action = match verb {
        "attach" => SessionAction::Attach,
        "interrupt" => SessionAction::Interrupt,
        "hibernate" => SessionAction::Hibernate,
        "restart" => SessionAction::Restart,
        "kill" => SessionAction::Kill,
        "adopt" => SessionAction::ReAdopt,
        _ => return None,
    };
    Some((name.to_string(), action))
}

fn build_initial_menu() -> Result<Menu> {
    let m = Menu::new();
    m.append(&MenuItem::with_id(
        STATIC_ID_OPEN_WEB,
        "Open web viewer",
        true,
        None,
    ))?;
    m.append(&PredefinedMenuItem::separator())?;
    m.append(&MenuItem::with_id(
        STATIC_ID_QUIT_TRAY,
        "Quit tray",
        true,
        None,
    ))?;
    Ok(m)
}

fn build_session_menu(sessions: &[SessionInfo]) -> Result<Menu> {
    let m = Menu::new();
    if sessions.is_empty() {
        m.append(&MenuItem::new("No sessions", false, None))?;
    } else {
        for s in sessions {
            let label = format!("{} · {}", s.name, render_state(&s.state));
            let sub = Submenu::new(&label, true);
            if s.state == "locally_owned" {
                // Broker has no claude in this session: Attach would
                // show an empty TUI, Interrupt/Hibernate/Restart/Kill
                // either no-op or 409. Replace with the only useful
                // action: bring the session back under broker.
                // Kill stays available so the user can permanently
                // discard a demoted session.
                sub.append(&MenuItem::with_id(
                    format!("session:{}:adopt", s.name),
                    "Re-adopt to broker",
                    true,
                    None,
                ))?;
                sub.append(&PredefinedMenuItem::separator())?;
                sub.append(&MenuItem::new(
                    "(local terminal owns this session)",
                    false,
                    None,
                ))?;
                sub.append(&PredefinedMenuItem::separator())?;
                sub.append(&MenuItem::with_id(
                    format!("session:{}:kill", s.name),
                    "Discard session record",
                    true,
                    None,
                ))?;
            } else {
                sub.append(&MenuItem::with_id(
                    format!("session:{}:attach", s.name),
                    "Attach",
                    true,
                    None,
                ))?;
                sub.append(&PredefinedMenuItem::separator())?;
                sub.append(&MenuItem::with_id(
                    format!("session:{}:interrupt", s.name),
                    "Interrupt (Ctrl+C)",
                    true,
                    None,
                ))?;
                sub.append(&MenuItem::with_id(
                    format!("session:{}:hibernate", s.name),
                    "Hibernate",
                    true,
                    None,
                ))?;
                sub.append(&MenuItem::with_id(
                    format!("session:{}:restart", s.name),
                    "Restart claude",
                    true,
                    None,
                ))?;
                sub.append(&PredefinedMenuItem::separator())?;
                sub.append(&MenuItem::with_id(
                    format!("session:{}:kill", s.name),
                    "Kill session",
                    true,
                    None,
                ))?;
            }
            m.append(&sub)?;
        }
    }
    m.append(&PredefinedMenuItem::separator())?;
    m.append(&MenuItem::with_id(
        STATIC_ID_OPEN_WEB,
        "Open web viewer",
        true,
        None,
    ))?;
    m.append(&PredefinedMenuItem::separator())?;
    m.append(&MenuItem::with_id(
        STATIC_ID_RESTART_ALL,
        "Restart agentmux (reload config)",
        true,
        None,
    ))?;
    m.append(&MenuItem::with_id(
        STATIC_ID_QUIT_BROKER,
        "Stop broker",
        true,
        None,
    ))?;
    m.append(&MenuItem::with_id(
        STATIC_ID_QUIT_ALL,
        "Quit all (broker + discord + tray)",
        true,
        None,
    ))?;
    m.append(&MenuItem::with_id(
        STATIC_ID_QUIT_TRAY,
        "Quit tray",
        true,
        None,
    ))?;
    Ok(m)
}

fn build_offline_menu() -> Result<Menu> {
    let m = Menu::new();
    m.append(&MenuItem::new("Broker offline", false, None))?;
    m.append(&PredefinedMenuItem::separator())?;
    // Even with broker offline, a stray discord-platform.exe may
    // still be running (the case that motivated adding Quit all
    // in the first place). Offer it here too — broker.shutdown()
    // will fail harmlessly, taskkill on the bot still works.
    m.append(&MenuItem::with_id(
        STATIC_ID_QUIT_ALL,
        "Quit all (kill discord + tray)",
        true,
        None,
    ))?;
    m.append(&MenuItem::with_id(
        STATIC_ID_QUIT_TRAY,
        "Quit tray",
        true,
        None,
    ))?;
    Ok(m)
}

fn sessions_changed(a: &[SessionInfo], b: &[SessionInfo]) -> bool {
    if a.len() != b.len() {
        return true;
    }
    for (x, y) in a.iter().zip(b.iter()) {
        if x.id != y.id || x.name != y.name || x.state != y.state || x.viewers != y.viewers {
            return true;
        }
    }
    false
}

fn render_state(s: &str) -> &'static str {
    match s {
        "idle" => "idle",
        "hibernated" => "💤 hib",
        "crashed" => "❌ crash",
        "locally_owned" => "🌐 local",
        _ => "?",
    }
}

fn needs_attention(s: &SessionInfo) -> bool {
    matches!(s.state.as_str(), "crashed")
}

fn is_locally_owned(s: &SessionInfo) -> bool {
    s.state.as_str() == "locally_owned"
}

fn is_running(s: &SessionInfo) -> bool {
    // We don't get a "running" state per session yet — viewer count is
    // a reasonable proxy for "someone's actively in there". Phase 4+
    // can extend this with a real "claude is mid-turn" state pushed
    // from broker on tool_request / assistant_message arrival.
    s.viewers > 0
}

fn format_tooltip(connected: bool, snapshot: &[SessionInfo]) -> String {
    if !connected {
        return "agentmux · broker offline".to_string();
    }
    if snapshot.is_empty() {
        return "agentmux · 0 sessions".to_string();
    }
    let active = snapshot.iter().filter(|s| s.state == "idle").count();
    let hib = snapshot
        .iter()
        .filter(|s| s.state == "hibernated")
        .count();
    let crashed = snapshot.iter().filter(|s| s.state == "crashed").count();
    let local = snapshot
        .iter()
        .filter(|s| s.state == "locally_owned")
        .count();
    let mut parts = Vec::new();
    parts.push(format!("{} session(s)", snapshot.len()));
    if active > 0 {
        parts.push(format!("{active} idle"));
    }
    if hib > 0 {
        parts.push(format!("{hib} hibernated"));
    }
    if crashed > 0 {
        parts.push(format!("{crashed} crashed"));
    }
    if local > 0 {
        parts.push(format!("{local} local"));
    }
    format!("agentmux · {}", parts.join(", "))
}

/// Programmatically build a 32×32 RGBA tray icon coloured per state.
/// Solid-fill square is intentionally minimal — we want to ship
/// without bundling .ico files. Future polish: replace with a proper
/// vector source rendered to PNG at multiple resolutions.
fn build_icon(state: IconState) -> Icon {
    let (r, g, b) = match state {
        IconState::Disconnected => (160, 160, 160),
        IconState::NoSessions => (128, 128, 128),
        IconState::AllIdle => (40, 167, 69),         // green
        IconState::AnyRunning => (255, 193, 7),      // yellow
        IconState::NeedsAttention => (220, 53, 69),  // red
        IconState::AnyLocallyOwned => (138, 99, 210), // purple
    };
    const SIZE: u32 = 32;
    let mut data = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            // Round corners by treating pixels outside a circle of
            // radius (SIZE/2 - 1) as transparent. Cheap antialias-free
            // disc but recognisable as an icon rather than a brick.
            let cx = SIZE as f32 / 2.0 - 0.5;
            let cy = SIZE as f32 / 2.0 - 0.5;
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            let dist = (dx * dx + dy * dy).sqrt();
            let radius = SIZE as f32 / 2.0 - 1.0;
            let alpha = if dist <= radius { 255u8 } else { 0u8 };
            data.push(r);
            data.push(g);
            data.push(b);
            data.push(alpha);
        }
    }
    Icon::from_rgba(data, SIZE, SIZE).expect("build solid-disc icon")
}

fn open_in_browser(url: &str) -> Result<()> {
    use std::process::Command;
    Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn()
        .map_err(|e| anyhow!("open browser: {e}"))?;
    Ok(())
}
