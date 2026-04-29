//! agentmux-tray: Windows system-tray + toast adapter for the broker.
//!
//! Runs as a separate process so the broker stays headless and async.
//! Architecture:
//!   * Main thread owns the Win32 message pump (tao + tray-icon require
//!     it). All UI work — tray menu rebuilds, toast emission — is
//!     dispatched here via an `EventLoopProxy<UserEvent>` so we never
//!     touch GUI APIs from a tokio worker.
//!   * Worker tokio runtime drives the broker WS subscription and the
//!     periodic `/sessions` poll. Each interesting event forwards a
//!     `UserEvent` into the main loop.
//!   * A second instance launched by Windows protocol activation
//!     (`agentmux://approve/<id>` etc.) detects via the named-pipe
//!     server, forwards the URL string, and exits — the running
//!     instance dispatches the URL the same way as if the user had
//!     clicked an in-tray menu item.

mod broker;
mod deeplink;
mod ipc;
mod toast;
mod tray;
mod ws;

use std::sync::Arc;

use anyhow::{Context, Result};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tracing::{error, info, warn};

use crate::broker::BrokerClient;
use crate::deeplink::DeepLink;

/// Anything happening on a worker thread that needs main-thread
/// follow-up gets wrapped as one of these and posted through the
/// event-loop proxy. Keep variants tiny — large payloads should hold
/// `Arc<...>` so cloning into the proxy is cheap.
#[derive(Debug, Clone)]
pub enum UserEvent {
    /// Periodic sessions snapshot for tray menu rebuild + icon colour.
    SessionsRefreshed(Arc<Vec<broker::SessionInfo>>),
    /// Broker WS event we want to surface as a toast / tray badge.
    /// `kind` is the event "type" field (assistant_message,
    /// notification, tool_request, …); raw is the unparsed JSON value
    /// so each toast builder can pick what it needs.
    BrokerEvent {
        kind: String,
        session_name: Option<String>,
        raw: serde_json::Value,
    },
    /// A second instance forwarded a deeplink URI to us via the IPC
    /// pipe. Dispatch as if we had handled it ourselves at startup.
    DeepLinkReceived(DeepLink),
    /// The WS connection went down or came back up — reflect in the
    /// tray tooltip and (later) icon overlay.
    BrokerConnectivity { connected: bool },
}

fn main() -> Result<()> {
    init_logging();

    // argv[1], if present, is the deeplink Windows protocol activation
    // handed us. Parse early so single-instance forwarding doesn't have
    // to re-derive it.
    let raw_url = std::env::args().nth(1);
    let parsed = raw_url
        .as_deref()
        .filter(|s| s.starts_with("agentmux://"))
        .and_then(DeepLink::parse);

    // Tokio runtime is built BEFORE the singleton check because
    // `interprocess::ListenerOptions::create_tokio()` registers the
    // listener with the current runtime's IO reactor — calling it
    // outside a runtime context panics. The runtime then lives for
    // the rest of the process (handed to `run_primary` below if we
    // win the singleton race).
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("agentmux-tray-tokio")
        .build()
        .context("build tokio runtime")?;

    let role = runtime.block_on(async { ipc::try_become_instance() });

    match role {
        ipc::InstanceRole::Primary(pipe_listener) => {
            run_primary(parsed, pipe_listener, runtime)
        }
        ipc::InstanceRole::Secondary => {
            // Drop the runtime — secondary doesn't need it for the
            // sync forward path.
            drop(runtime);
            if let Some(url) = raw_url {
                if let Err(e) = ipc::forward_deeplink(&url) {
                    eprintln!("agentmux-tray: forward deeplink: {e:#}");
                }
            } else {
                eprintln!("agentmux-tray: another instance is already running");
            }
            Ok(())
        }
    }
}

fn run_primary(
    initial_deeplink: Option<DeepLink>,
    pipe: ipc::PipeListener,
    runtime: tokio::runtime::Runtime,
) -> Result<()> {
    info!("agentmux-tray starting (primary instance)");

    // Best-effort: register the agentmux:// URL scheme + Start Menu
    // shortcut for AppUserModelId. Both are idempotent and safe to run
    // every launch. Failure logs a warning but doesn't block startup —
    // the toasts will work without scheme registration; they just
    // can't deliver clicks back to us.
    if let Err(e) = deeplink::register_url_scheme() {
        warn!("URL scheme registration failed: {e:#}");
    }
    if let Err(e) = toast::install_appusermodelid_shortcut() {
        warn!("AppUserModelId shortcut install failed: {e:#}");
    }

    let cfg = Arc::new(shared::config::Config::load());
    let broker = Arc::new(BrokerClient::new(cfg.clone()));

    // Build the user-event-driven event loop FIRST so we have a proxy
    // to hand off to async tasks.
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // WS subscriber: pushes BrokerEvent / Connectivity into the loop.
    {
        let proxy = proxy.clone();
        let cfg = cfg.clone();
        runtime.spawn(async move {
            ws::run_subscriber(cfg, proxy).await;
        });
    }

    // /sessions poller: pushes SessionsRefreshed every PoLL_INTERVAL.
    {
        let proxy = proxy.clone();
        let broker = broker.clone();
        runtime.spawn(async move {
            broker::run_session_poller(broker, proxy).await;
        });
    }

    // Pipe listener: receives forwarded deeplinks from secondary
    // instances (Windows protocol activation launches a new copy of
    // us with the URL as argv[1]).
    {
        let proxy = proxy.clone();
        runtime.spawn(async move {
            ipc::run_pipe_listener(pipe, proxy).await;
        });
    }

    // If we were launched WITH a deeplink, hand it off through the
    // proxy so the dispatch logic in the event loop is the only path —
    // no parallel "dispatch on main vs dispatch on event" branches.
    if let Some(dl) = initial_deeplink {
        let _ = proxy.send_event(UserEvent::DeepLinkReceived(dl));
    }

    // Build tray + run forever. event_loop.run never returns.
    let mut tray_state = tray::TrayState::new(
        broker.clone(),
        runtime.handle().clone(),
        proxy.clone(),
    )
    .context("build tray")?;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        // Drain tray-icon's own menu/icon channels each tick — they
        // bypass tao's event channel.
        tray_state.drain_menu_events();

        match event {
            tao::event::Event::UserEvent(ev) => {
                if let Err(e) = handle_user_event(&mut tray_state, ev) {
                    warn!("user event handler: {e:#}");
                }
            }
            tao::event::Event::LoopDestroyed => {
                info!("event loop terminating");
            }
            _ => {}
        }
    })
}

fn handle_user_event(tray: &mut tray::TrayState, ev: UserEvent) -> Result<()> {
    match ev {
        UserEvent::SessionsRefreshed(snapshot) => {
            tray.refresh_sessions(&snapshot)?;
        }
        UserEvent::BrokerConnectivity { connected } => {
            tray.set_connected(connected)?;
        }
        UserEvent::BrokerEvent { kind, session_name, raw } => {
            match kind.as_str() {
                "assistant_message" => toast::on_assistant_message(&session_name, &raw),
                "notification" => toast::on_notification(&session_name, &raw),
                "tool_request" => toast::on_tool_request(&session_name, &raw),
                _ => {} // ignore unrelated event kinds
            }
        }
        UserEvent::DeepLinkReceived(dl) => {
            let broker = tray.broker_client();
            let runtime = tray.runtime_handle();
            runtime.spawn(async move {
                if let Err(e) = deeplink::dispatch(broker, dl).await {
                    error!("deeplink dispatch: {e:#}");
                }
            });
        }
    }
    Ok(())
}

fn init_logging() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,agentmux_tray=debug"));
    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}
