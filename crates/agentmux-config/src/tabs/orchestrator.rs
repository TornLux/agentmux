use eframe::egui;

use crate::io::{self, BrokerForm, DiscordForm, SessionRow};

pub fn draw(ui: &mut egui::Ui, broker: &mut BrokerForm, discord: &mut DiscordForm) {
    ui.heading("Orchestrator");
    ui.label(
        "Boss/worker pattern: one main session receives @-mentions, decides whether to dispatch \
         work to other sessions, summarises results back to you. See docs/orchestrator-prompt.md \
         for the full role spec.",
    );
    ui.add_space(8.0);

    // Cache the broker session list for the dropdown — fetched on
    // demand, refreshable via the button. Stored in egui temp data so
    // it survives across frames without bloating App state.
    let cache_id = egui::Id::new("orchestrator-sessions");
    if ui
        .data_mut(|d| d.get_temp::<Option<Vec<SessionRow>>>(cache_id).is_none())
    {
        ui.data_mut(|d| d.insert_temp(cache_id, None::<Vec<SessionRow>>));
    }

    let cached: Option<Vec<SessionRow>> = ui
        .data_mut(|d| d.get_temp::<Option<Vec<SessionRow>>>(cache_id))
        .flatten();

    egui::Grid::new("orchestrator_grid")
        .num_columns(2)
        .spacing([12.0, 8.0])
        .striped(true)
        .show(ui, |ui| {
            ui.label("Main session name").on_hover_text(
                "Session that gets the orchestrator system prompt at broker startup AND that \
                 @-mentions in non-thread channels route to. Must match between config.toml \
                 and discord.toml.",
            );
            ui.horizontal(|ui| {
                ui.text_edit_singleline(&mut broker.main_session);
                if ui.button("⟳ Refresh sessions").clicked() {
                    let url = if discord.broker_http_url.is_empty() {
                        "http://127.0.0.1:8765"
                    } else {
                        discord.broker_http_url.as_str()
                    };
                    let result = io::fetch_sessions(url).ok();
                    ui.data_mut(|d| d.insert_temp(cache_id, result));
                }
            });
            ui.end_row();

            if let Some(sessions) = &cached {
                ui.label("");
                ui.horizontal_wrapped(|ui| {
                    ui.label("Existing sessions:");
                    if sessions.is_empty() {
                        ui.label("(none)");
                    } else {
                        for s in sessions {
                            if ui.button(&s.name).on_hover_text(format!(
                                "state={} cwd={}",
                                s.state, s.cwd
                            )).clicked() {
                                broker.main_session = s.name.clone();
                            }
                        }
                    }
                });
                ui.end_row();
            } else {
                ui.label("");
                ui.label("(click ⟳ Refresh sessions to fetch a list from the broker)");
                ui.end_row();
            }

            ui.label("Worker thread parent channel ID").on_hover_text(
                "Discord channel under which the bot creates a new thread for every spawned \
                 worker. Right-click the channel → Copy ID. 0 = no auto-thread.",
            );
            ui.text_edit_singleline(&mut discord.worker_thread_parent);
            ui.end_row();

            ui.label("Dashboard channel ID").on_hover_text(
                "Discord channel where the bot maintains a single 'current sessions' embed, \
                 updated every 5s. 0 = no dashboard.",
            );
            ui.text_edit_singleline(&mut discord.dashboard_channel_id);
            ui.end_row();

            ui.label("Max active dispatches per caller").on_hover_text(
                "Cap to prevent a runaway main agent from spawning unbounded workers. \
                 0 = unlimited (not recommended).",
            );
            ui.add(
                egui::DragValue::new(&mut broker.max_active_dispatches_per_session).range(0..=100),
            );
            ui.end_row();

            ui.label("Dispatch timeout (seconds)").on_hover_text(
                "Wall-clock deadline before broker auto-fails a callback. \
                 1800 = 30min, generous default.",
            );
            ui.add(
                egui::DragValue::new(&mut broker.dispatch_timeout_secs).range(60..=86_400),
            );
            ui.end_row();
        });

    ui.add_space(10.0);
    ui.separator();
    ui.label("Tip: keep main_session matching between this tab's two halves — broker reads it \
              from config.toml, the bot reads it from discord.toml. The Save button writes both.");
    if broker.main_session != discord.main_session {
        ui.colored_label(
            egui::Color32::from_rgb(0xfe, 0xe7, 0x5c),
            format!(
                "⚠ broker.main_session={:?} ≠ discord.main_session={:?} — \
                 the bot won't route @-mentions to the same session as broker bootstraps",
                broker.main_session, discord.main_session
            ),
        );
        if ui.button("Sync to broker value").clicked() {
            discord.main_session = broker.main_session.clone();
        }
    }

    // The orchestrator's "@bot in any channel" entry point only fires
    // when respond_to_mentions is true — otherwise non-whitelisted
    // channels swallow the @-mention silently. Surface the dependency
    // here (next to the field that creates it) instead of letting the
    // user discover it by hitting the bug.
    if !discord.main_session.is_empty() && !discord.respond_to_mentions {
        ui.add_space(6.0);
        ui.colored_label(
            egui::Color32::from_rgb(0xfe, 0xe7, 0x5c),
            "⚠ main_session is set but discord.respond_to_mentions = false. \
             @-mentions in non-whitelisted channels will be ignored — the orchestrator \
             entry point won't work.",
        );
        if ui
            .button("Enable respond_to_mentions")
            .on_hover_text("Flip discord.respond_to_mentions to true so @-mentions in any channel route to main")
            .clicked()
        {
            discord.respond_to_mentions = true;
        }
    }
}
