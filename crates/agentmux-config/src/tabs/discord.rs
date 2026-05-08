use eframe::egui;

use crate::io::{self, DiscordForm};

pub fn draw(ui: &mut egui::Ui, form: &mut DiscordForm) {
    ui.heading("Discord");
    ui.label(
        "Settings live in %LOCALAPPDATA%\\agentmux\\discord.toml. \
         Bot token persists in the User-scope env var named below — never on disk.",
    );
    ui.add_space(8.0);

    egui::CollapsingHeader::new("Bot token")
        .default_open(true)
        .show(ui, |ui| {
            draw_token(ui, form);
        });

    ui.add_space(6.0);
    egui::CollapsingHeader::new("Whitelists")
        .default_open(true)
        .show(ui, |ui| {
            draw_id_list(
                ui,
                "Channels",
                &mut form.channel_ids,
                "Discord channel IDs the bot reads from. Empty = listen everywhere the bot is.",
            );
            ui.add_space(6.0);
            draw_id_list(
                ui,
                "Allowed user IDs (required)",
                &mut form.allowed_user_ids,
                "Discord user IDs whose messages the bot will act on. Bot refuses to start if empty.",
            );
        });

    ui.add_space(6.0);
    egui::CollapsingHeader::new("Routing & UX")
        .default_open(true)
        .show(ui, |ui| {
            egui::Grid::new("discord_ux")
                .num_columns(2)
                .spacing([12.0, 6.0])
                .striped(true)
                .show(ui, |ui| {
                    ui.label("Default session").on_hover_text(
                        "Session that bot's bound to before the user runs !attach.",
                    );
                    ui.text_edit_singleline(&mut form.default_session);
                    ui.end_row();

                    ui.label("Allow DMs");
                    ui.checkbox(&mut form.allow_dm, "");
                    ui.end_row();

                    ui.label("Notify on idle pings").on_hover_text(
                        "Forward Claude's 'waiting for input' notifications. Off by default \
                         (they fire once after every reply and most users find them noisy).",
                    );
                    ui.checkbox(&mut form.notify_on_idle, "");
                    ui.end_row();

                    ui.label("Respond to mentions").on_hover_text(
                        "Accept messages in non-whitelisted channels when the bot is @mentioned. \
                         Required for the orchestrator @-mention entry point to work outside the \
                         configured channel list.",
                    );
                    ui.checkbox(&mut form.respond_to_mentions, "");
                    ui.end_row();

                    ui.label("Reply quote in prompt").on_hover_text(
                        "Prepend the quoted message text when forwarding a Discord reply.",
                    );
                    ui.checkbox(&mut form.reply_quote_in_prompt, "");
                    ui.end_row();

                    ui.label("Reaction commands").on_hover_text(
                        "Treat 🛑/💤/🔄 reactions on bot replies as interrupt/hibernate/restart \
                         on the source session.",
                    );
                    ui.checkbox(&mut form.react_with_actions, "");
                    ui.end_row();

                    ui.label("Slash command guild ID").on_hover_text(
                        "0 = register globally (1h propagation). Set to a guild ID for instant \
                         registration scoped to one server (recommended for dev).",
                    );
                    ui.text_edit_singleline(&mut form.slash_command_guild_id);
                    ui.end_row();

                    ui.label("Max message chars").on_hover_text(
                        "Discord caps single messages at 2000. Bot splits at this threshold to \
                         leave headroom for decoration.",
                    );
                    ui.add(egui::DragValue::new(&mut form.max_message_chars).range(500..=2000));
                    ui.end_row();
                });
        });
}

fn draw_token(ui: &mut egui::Ui, form: &mut DiscordForm) {
    let env_name = form.token_env.clone();
    let current = io::read_token_from_env(&env_name);
    let suffix = if current.is_empty() {
        "(unset)".to_string()
    } else {
        format!("(set, {} chars)", current.len())
    };
    ui.horizontal(|ui| {
        ui.label("Env var name:");
        ui.text_edit_singleline(&mut form.token_env);
        ui.label(suffix);
    });

    // We keep the typed token in a per-frame textedit; persisting requires
    // an explicit click. Don't bind to a struct field — we don't want to
    // serialise the token anywhere except the env var.
    let token_buf = ui
        .data_mut(|d| d.get_temp_mut_or::<String>(egui::Id::new("token-buf"), String::new()).clone());
    let mut token_buf = token_buf;
    ui.horizontal(|ui| {
        ui.label("New token:");
        ui.add(egui::TextEdit::singleline(&mut token_buf).password(true).desired_width(360.0));
    });
    ui.data_mut(|d| d.insert_temp(egui::Id::new("token-buf"), token_buf.clone()));

    ui.horizontal(|ui| {
        if ui
            .button("🔍 Test")
            .on_hover_text("Hit Discord's /users/@me endpoint with the new token (or current env value if blank)")
            .clicked()
        {
            let to_test = if token_buf.is_empty() {
                current.clone()
            } else {
                token_buf.clone()
            };
            let id = egui::Id::new("token-status");
            if to_test.is_empty() {
                ui.data_mut(|d| {
                    d.insert_temp(id, ("no token to test".to_string(), true));
                });
            } else {
                match io::verify_discord_token(&to_test) {
                    Ok(who) => ui.data_mut(|d| d.insert_temp(id, (format!("✓ {who}"), false))),
                    Err(e) => ui.data_mut(|d| d.insert_temp(id, (format!("✗ {e}"), true))),
                }
            }
        }
        if ui
            .button("💾 Save token to env var")
            .on_hover_text("Persist to User-scope env var; reopens-required for already-running PowerShells")
            .clicked()
        {
            if token_buf.is_empty() {
                ui.data_mut(|d| {
                    d.insert_temp(
                        egui::Id::new("token-status"),
                        ("(typed token is empty)".to_string(), true),
                    )
                });
            } else {
                let id = egui::Id::new("token-status");
                match io::save_token_to_env(&env_name, &token_buf) {
                    Ok(_) => {
                        ui.data_mut(|d| {
                            d.insert_temp(id, (format!("✓ saved to {env_name}"), false))
                        });
                    }
                    Err(e) => {
                        ui.data_mut(|d| d.insert_temp(id, (format!("✗ {e}"), true)));
                    }
                }
            }
        }
    });

    if let Some((msg, is_err)) =
        ui.data_mut(|d| d.get_temp::<(String, bool)>(egui::Id::new("token-status")))
    {
        let color = if is_err {
            egui::Color32::from_rgb(0xed, 0x42, 0x45)
        } else {
            egui::Color32::from_rgb(0x57, 0xf2, 0x87)
        };
        ui.colored_label(color, msg);
    }
}

fn draw_id_list(ui: &mut egui::Ui, label: &str, ids: &mut Vec<String>, hover: &str) {
    ui.label(label).on_hover_text(hover);
    let mut to_remove: Option<usize> = None;
    egui::Grid::new(format!("id_list_{label}"))
        .num_columns(2)
        .spacing([6.0, 3.0])
        .show(ui, |ui| {
            for (i, id) in ids.iter_mut().enumerate() {
                ui.add(egui::TextEdit::singleline(id).desired_width(220.0));
                if ui.button("✕").on_hover_text("Remove").clicked() {
                    to_remove = Some(i);
                }
                ui.end_row();
            }
        });
    if let Some(i) = to_remove {
        ids.remove(i);
    }
    if ui.button("+ Add").clicked() {
        ids.push(String::new());
    }
}
