//! agentmux-config — eframe GUI for editing the broker + discord
//! TOML config files without leaving Discord-style ergonomics behind.
//!
//! Launchers: tray icon "Settings...", `agentmux config gui`, double-
//! click on tray, or the "open GUI?" prompt at the end of `agentmux
//! init`. All four spawn this binary as a detached child — config
//! lifecycle is independent of broker/tray crashes.
//!
//! Save semantics: format-preserving via `toml_edit`. The user's
//! comments and field ordering survive a save round-trip; only the
//! values change. After a successful Save & Restart, broker + tray +
//! discord all bounce so the new config takes effect.
//!
//! All knobs map 1:1 to the TOML schemas in `shared::config::Config`
//! (broker) and `platform_discord::config::DiscordConfig` (discord),
//! grouped into five tabs: Broker / Discord / Orchestrator / Hooks /
//! Advanced.

// Don't open a console window when launched from the tray on Windows.
// Linux/macOS GUI loops don't have an equivalent concern.
#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

use std::path::PathBuf;

use eframe::egui;

mod io;
mod tabs;

fn main() -> eframe::Result<()> {
    init_logging();
    let broker_path = shared::config::default_config_path();
    let discord_path = shared::config::local_appdata_dir().join("discord.toml");
    let app = App::load(broker_path, discord_path);
    eframe::run_native(
        "agentmux config",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([760.0, 620.0])
                .with_min_inner_size([520.0, 400.0]),
            ..Default::default()
        },
        Box::new(|_cc| Ok(Box::new(app))),
    )
}

fn init_logging() {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info,wgpu_core=warn,wgpu_hal=warn,naga=warn".into());
    tracing_subscriber::fmt().with_env_filter(env_filter).init();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Broker,
    Discord,
    Orchestrator,
    Hooks,
    Advanced,
}

pub struct App {
    /// Format-preserving TOML for the broker config. `None` means the
    /// file didn't exist on load — Save will create it from defaults.
    pub broker_doc: io::Doc,
    pub discord_doc: io::Doc,
    /// Typed snapshot of the loaded values; UI binds against fields
    /// here. On Save we re-serialise these back into the docs so user
    /// comments and ordering survive.
    pub broker: io::BrokerForm,
    pub discord: io::DiscordForm,
    pub tab: Tab,
    /// Toast-style status line at the bottom of the window. Cleared on
    /// next interaction (egui repaints frequently). Tracked here so
    /// it survives at least one frame after the click that produced it.
    pub status: String,
    pub status_is_err: bool,
    pub broker_path: PathBuf,
    pub discord_path: PathBuf,
}

impl App {
    pub fn load(broker_path: PathBuf, discord_path: PathBuf) -> Self {
        let (broker_doc, broker) = io::load_broker(&broker_path);
        let (discord_doc, discord) = io::load_discord(&discord_path);
        Self {
            broker_doc,
            discord_doc,
            broker,
            discord,
            tab: Tab::Broker,
            status: String::new(),
            status_is_err: false,
            broker_path,
            discord_path,
        }
    }

    fn set_status(&mut self, msg: impl Into<String>, is_err: bool) {
        self.status = msg.into();
        self.status_is_err = is_err;
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("agentmux config");
                ui.separator();
                ui.selectable_value(&mut self.tab, Tab::Broker, "Broker");
                ui.selectable_value(&mut self.tab, Tab::Discord, "Discord");
                ui.selectable_value(&mut self.tab, Tab::Orchestrator, "Orchestrator");
                ui.selectable_value(&mut self.tab, Tab::Hooks, "Hooks");
                ui.selectable_value(&mut self.tab, Tab::Advanced, "Advanced");
            });
        });

        egui::TopBottomPanel::bottom("actions").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.button("💾 Save").on_hover_text("Write both TOML files; broker picks up on next start").clicked() {
                    self.do_save(false);
                }
                if ui.button("💾 Save & Restart agentmux")
                    .on_hover_text("Write configs + POST /restart-agentmux so changes take effect now")
                    .clicked()
                {
                    self.do_save(true);
                }
                if ui.button("Cancel").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                ui.separator();
                if !self.status.is_empty() {
                    let color = if self.status_is_err {
                        egui::Color32::from_rgb(0xed, 0x42, 0x45)
                    } else {
                        egui::Color32::from_rgb(0x57, 0xf2, 0x87)
                    };
                    ui.colored_label(color, &self.status);
                }
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| match self.tab {
                Tab::Broker => tabs::broker::draw(ui, &mut self.broker),
                Tab::Discord => tabs::discord::draw(ui, &mut self.discord),
                Tab::Orchestrator => tabs::orchestrator::draw(ui, &mut self.broker, &mut self.discord),
                Tab::Hooks => tabs::hooks::draw(ui, &mut self.status, &mut self.status_is_err),
                Tab::Advanced => tabs::advanced::draw(ui, &mut self.broker, &mut self.discord),
            });
        });
    }
}

impl App {
    fn do_save(&mut self, restart: bool) {
        let broker_res = io::save_broker(&self.broker_path, &mut self.broker_doc, &self.broker);
        let discord_res = io::save_discord(&self.discord_path, &mut self.discord_doc, &self.discord);
        match (broker_res, discord_res) {
            (Ok(_), Ok(_)) => {
                if restart {
                    match io::trigger_restart() {
                        Ok(_) => self.set_status("saved + restart triggered", false),
                        Err(e) => self.set_status(format!("saved; restart failed: {e}"), true),
                    }
                } else {
                    self.set_status("saved", false);
                }
            }
            (Err(e), _) | (_, Err(e)) => self.set_status(format!("save failed: {e}"), true),
        }
    }
}
