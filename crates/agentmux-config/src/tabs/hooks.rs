use std::process::Command;

use eframe::egui;

pub fn draw(ui: &mut egui::Ui, status: &mut String, status_is_err: &mut bool) {
    ui.heading("Hooks");
    ui.label(
        "Claude Code's user-global ~/.claude/settings.json carries four hooks pointing at \
         agentmux's hook-* binaries. Idempotent — re-installing converges to a single \
         canonical entry per hook even across folder moves.",
    );
    ui.add_space(8.0);

    ui.label("Stop          — turn-complete events (IM replies, toasts)");
    ui.label("Notification  — permission prompts / idle pings");
    ui.label("PreToolUse    — tool-use approval (auto-allow safe verbs)");
    ui.label("PostToolUse   — live tool-progress narration in Discord placeholder");
    ui.add_space(12.0);

    ui.horizontal(|ui| {
        if ui.button("Install / Re-sync").clicked() {
            run_install(status, status_is_err);
        }
        if ui.button("Uninstall").clicked() {
            run_uninstall(status, status_is_err);
        }
        if ui.button("Check").clicked() {
            run_check(status, status_is_err);
        }
    });

    ui.add_space(8.0);
    ui.label("Output is shown in the status bar at the bottom. Run from a terminal for full output.");
}

fn run_install(status: &mut String, status_is_err: &mut bool) {
    run_script("install-hooks.ps1", &[], status, status_is_err, "hooks installed");
}

fn run_uninstall(status: &mut String, status_is_err: &mut bool) {
    run_script(
        "install-hooks.ps1",
        &["-Uninstall"],
        status,
        status_is_err,
        "hooks uninstalled",
    );
}

fn run_check(status: &mut String, status_is_err: &mut bool) {
    run_script(
        "install-hooks.ps1",
        &["-CheckOnly"],
        status,
        status_is_err,
        "check passed",
    );
}

fn run_script(
    script: &str,
    extra_args: &[&str],
    status: &mut String,
    status_is_err: &mut bool,
    ok_msg: &str,
) {
    // Resolve relative to the agentmux-config.exe binary path so this
    // works whether the user installed from the release zip (bin/ next
    // to scripts/) or is running from a cargo build (target/release).
    let script_path = match resolve_script(script) {
        Some(p) => p,
        None => {
            *status = format!("could not locate {script} in scripts/");
            *status_is_err = true;
            return;
        }
    };
    let mut args = vec![
        "-NoProfile".to_string(),
        "-NonInteractive".to_string(),
        "-File".to_string(),
        script_path.to_string_lossy().to_string(),
    ];
    for a in extra_args {
        args.push((*a).to_string());
    }
    let result = Command::new("powershell.exe").args(&args).output();
    match result {
        Ok(out) if out.status.success() => {
            *status = ok_msg.to_string();
            *status_is_err = false;
        }
        Ok(out) => {
            *status = format!(
                "exit {} — {}",
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).lines().next().unwrap_or("(no stderr)")
            );
            *status_is_err = true;
        }
        Err(e) => {
            *status = format!("spawn powershell: {e}");
            *status_is_err = true;
        }
    }
}

fn resolve_script(script: &str) -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let here = exe.parent()?;
    // Try sibling layouts: bin/.. for release zip, target/release/..
    // for cargo build, target/debug/.. for dev.
    for up in [1usize, 2] {
        let mut candidate = here.to_path_buf();
        for _ in 0..up {
            candidate.pop();
        }
        candidate.push("scripts");
        candidate.push(script);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}
