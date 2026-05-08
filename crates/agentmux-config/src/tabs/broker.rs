use eframe::egui;

use crate::io::{BrokerForm, ToolApprovalChoice};

pub fn draw(ui: &mut egui::Ui, form: &mut BrokerForm) {
    ui.heading("Broker");
    ui.label("Settings live in %LOCALAPPDATA%\\agentmux\\config.toml. \
              Changes apply on next agentmux start, or click 'Save & Restart' below.");
    ui.add_space(8.0);

    egui::Grid::new("broker_grid")
        .num_columns(2)
        .spacing([12.0, 8.0])
        .striped(true)
        .show(ui, |ui| {
            ui.label("HTTP listen address").on_hover_text(
                "host:port the broker control plane binds. \
                 0.0.0.0 enables LAN; loopback by default.",
            );
            ui.text_edit_singleline(&mut form.http_addr);
            ui.end_row();

            ui.label("Default cwd for new sessions").on_hover_text(
                "Empty = use the broker process's launch cwd. Set this so new \
                 sessions don't depend on which folder you happened to be in.",
            );
            ui.horizontal(|ui| {
                ui.text_edit_singleline(&mut form.default_cwd);
                if ui.button("📁").on_hover_text("Pick a folder").clicked() {
                    if let Some(p) = pick_directory() {
                        form.default_cwd = p;
                    }
                }
            });
            ui.end_row();

            ui.label("Tool approval gate").on_hover_text(
                "off (recommended) lets every tool call through. \
                 ask re-enables hook-pretool's classifier + Discord/tray approval round-trip.",
            );
            ui.horizontal(|ui| {
                ui.radio_value(&mut form.tool_approval, ToolApprovalChoice::Off, "off");
                ui.radio_value(&mut form.tool_approval, ToolApprovalChoice::Ask, "ask");
            });
            ui.end_row();

            ui.label("Auto-resume new sessions").on_hover_text(
                "Default value of `auto_resume` for newly-created sessions. false = ephemeral \
                 (forgotten on broker restart); true = always restored. \
                 Per-session value still wins.",
            );
            ui.checkbox(&mut form.auto_resume_default, "persist by default");
            ui.end_row();

            ui.label("Hibernate idle (seconds)").on_hover_text(
                "Auto-hibernate Idle sessions whose user-side activity has been quiet for this \
                 many seconds. 0 disables the scanner. 86400 = 1 day.",
            );
            ui.add(egui::DragValue::new(&mut form.hibernate_idle_secs).range(0..=i64::MAX));
            ui.end_row();

            ui.label("Attach token (LAN auth)").on_hover_text(
                "Bearer token required on non-loopback HTTP/WS requests. \
                 Empty = LAN access disabled. Loopback bypasses auth regardless.",
            );
            ui.horizontal(|ui| {
                ui.text_edit_singleline(&mut form.attach_token);
                if ui
                    .button("🎲 Generate")
                    .on_hover_text("Generate a random 32-byte hex token")
                    .clicked()
                {
                    form.attach_token = random_hex(32);
                }
                if ui.button("Clear").clicked() {
                    form.attach_token.clear();
                }
            });
            ui.end_row();
        });
}

fn pick_directory() -> Option<String> {
    // No native picker dependency — egui doesn't bundle one. We could
    // add `rfd` later; for now ask the user to paste a path.
    None
}

fn random_hex(bytes: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}
