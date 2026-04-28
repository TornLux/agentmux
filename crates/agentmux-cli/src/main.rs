//! agentmux-cli — small helper invoked by `agentmux.ps1`.
//!
//! Two responsibilities the wrapper script can't do robustly itself:
//!
//!   1. *Format-preserving* edits to TOML config files. The wrapper
//!      could splice strings, but only `toml_edit` keeps the user's
//!      comments and ordering intact across saves.
//!   2. Per-kind config validation. Parses each file (`broker.toml`,
//!      `discord.toml`, `~/.claude/settings.json`), checks the
//!      invariants the runtime would otherwise only complain about
//!      at start, and reports `✓` / `⚠` / `✗` lines the wrapper
//!      surfaces verbatim.

use std::process::ExitCode;

mod check;
mod ops;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match dispatch(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(args: &[String]) -> anyhow::Result<()> {
    match args.first().map(|s| s.as_str()) {
        Some("config") => dispatch_config(&args[1..]),
        Some("help") | Some("--help") | Some("-h") | None => {
            print_help();
            Ok(())
        }
        Some(other) => anyhow::bail!("unknown command: {other} (try `help`)"),
    }
}

fn dispatch_config(args: &[String]) -> anyhow::Result<()> {
    match args.first().map(|s| s.as_str()) {
        Some("set") => ops::set(&args[1..]),
        Some("unset") => ops::unset(&args[1..]),
        Some("array-add") => ops::array_add(&args[1..]),
        Some("array-remove") => ops::array_remove(&args[1..]),
        Some("check") => check::run(&args[1..]),
        _ => anyhow::bail!(
            "usage: agentmux-cli config <set|unset|array-add|array-remove|check> ..."
        ),
    }
}

fn print_help() {
    println!("agentmux-cli — TOML helper for agentmux.ps1");
    println!();
    println!("Usage: agentmux-cli <command> ...");
    println!();
    println!("Commands:");
    println!("  config set        <file> <key> <value>");
    println!("  config unset      <file> <key>");
    println!("  config array-add  <file> <key> <value>");
    println!("  config array-remove <file> <key> <value>");
    println!("  config check      <file> --kind <broker|discord|hooks>");
    println!();
    println!("Values: numeric strings are parsed as integers, `true`/`false` as");
    println!("booleans, everything else stays a string. To force a string, prefix");
    println!("with @ (e.g. @123 stays \"123\").");
}
