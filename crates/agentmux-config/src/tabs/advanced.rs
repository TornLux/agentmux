use eframe::egui;

use crate::io::{BrokerForm, DiscordForm};

pub fn draw(ui: &mut egui::Ui, broker: &mut BrokerForm, discord: &mut DiscordForm) {
    ui.heading("Advanced");
    ui.label(
        "Knobs you should rarely need. Defaults are documented in init-config.ps1 and \
         init-discord-config.ps1.",
    );
    ui.add_space(8.0);

    egui::CollapsingHeader::new("Broker — paths & internals")
        .default_open(true)
        .show(ui, |ui| {
            egui::Grid::new("adv_broker")
                .num_columns(2)
                .spacing([12.0, 6.0])
                .striped(true)
                .show(ui, |ui| {
                    ui.label("pipe_name").on_hover_text(
                        "Bare local-socket name. broker expands to \\\\.\\pipe\\Local\\<name> \
                         on Windows, /tmp/<name>.sock on Unix.",
                    );
                    ui.text_edit_singleline(&mut broker.pipe_name);
                    ui.end_row();

                    ui.label("default_command").on_hover_text(
                        "argv used when broker spawns a fresh session. Comma-separated tokens.",
                    );
                    let mut joined = broker.default_command.join(", ");
                    if ui.text_edit_singleline(&mut joined).changed() {
                        broker.default_command = joined
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect();
                    }
                    ui.end_row();

                    ui.label("ring_cap_bytes").on_hover_text(
                        "Per-session PTY ring buffer cap. Bounds the replay sent to a viewer.",
                    );
                    ui.add(egui::DragValue::new(&mut broker.ring_cap_bytes).range(4096..=i64::MAX));
                    ui.end_row();

                    ui.label("sessions_toml_path").on_hover_text(
                        "Override path for sessions.toml. Empty = %LOCALAPPDATA%\\agentmux\\sessions.toml.",
                    );
                    ui.text_edit_singleline(&mut broker.sessions_toml_path);
                    ui.end_row();

                    ui.label("pid_file_path").on_hover_text(
                        "Override path for broker.pid. Empty = %LOCALAPPDATA%\\agentmux\\broker.pid.",
                    );
                    ui.text_edit_singleline(&mut broker.pid_file_path);
                    ui.end_row();

                    ui.label("log_dir").on_hover_text(
                        "Override directory for daily-rolling broker logs. \
                         Empty = %LOCALAPPDATA%\\agentmux\\logs.",
                    );
                    ui.text_edit_singleline(&mut broker.log_dir);
                    ui.end_row();
                });
        });

    ui.add_space(6.0);
    egui::CollapsingHeader::new("Discord — endpoints")
        .default_open(false)
        .show(ui, |ui| {
            egui::Grid::new("adv_discord")
                .num_columns(2)
                .spacing([12.0, 6.0])
                .striped(true)
                .show(ui, |ui| {
                    ui.label("broker_http_url").on_hover_text(
                        "Where the bot reaches the broker for HTTP RPCs.",
                    );
                    ui.text_edit_singleline(&mut discord.broker_http_url);
                    ui.end_row();

                    ui.label("broker_ws_url").on_hover_text(
                        "Where the bot subscribes for WS event stream.",
                    );
                    ui.text_edit_singleline(&mut discord.broker_ws_url);
                    ui.end_row();

                    ui.label("broker_config_path").on_hover_text(
                        "Reserved — empty for now.",
                    );
                    ui.text_edit_singleline(&mut discord.broker_config_path);
                    ui.end_row();
                });
        });

    ui.add_space(10.0);
    if ui.button("↻ Reset all advanced fields to defaults").clicked() {
        let dflt = BrokerForm::default();
        broker.pipe_name = dflt.pipe_name;
        broker.default_command = dflt.default_command;
        broker.ring_cap_bytes = dflt.ring_cap_bytes;
        broker.sessions_toml_path = dflt.sessions_toml_path;
        broker.pid_file_path = dflt.pid_file_path;
        broker.log_dir = dflt.log_dir;

        let dd = DiscordForm::default();
        discord.broker_http_url = dd.broker_http_url;
        discord.broker_ws_url = dd.broker_ws_url;
        discord.broker_config_path = dd.broker_config_path;
    }
}
