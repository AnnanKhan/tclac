//! tclac — control TCL Home smart air conditioners over the internet.
//!
//! Run with no arguments for the interactive TUI dashboard, or with a verb
//! (status/power/mode/temp/fan/…) for scriptable one-shot control. See
//! `tclac help` or AGENTS.md for the full command reference.

mod app;
mod auth;
mod cache;
mod cli;
mod config;
mod device;
mod iot;
mod rest;
mod ui;

use anyhow::{anyhow, Context, Result};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let first = args.first().map(|s| s.as_str()).unwrap_or("");

    match first {
        "" | "tui" => run_tui(&args).await,
        "-h" | "--help" | "help" => {
            print_help();
            Ok(())
        }
        "-V" | "--version" | "version" => {
            println!("tclac {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        // legacy flag aliases
        "--list" => cli::run(vec!["list".to_string()]).await,
        "--dump" => cli::run(vec!["dump".to_string()]).await,
        // everything else is a scriptable subcommand
        _ => cli::run(args).await,
    }
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("building HTTP client")
}

/// Launch the interactive TUI. `args` may carry `--device <id>`.
async fn run_tui(args: &[String]) -> Result<()> {
    let device_override = flag_value(args, "--device").or_else(|| flag_value(args, "-D"));

    let cfg = config::Config::load()?;
    let client = http_client()?;

    eprintln!("Logging in to TCL Home…");
    let session = auth::login(&client, &cfg.username, &cfg.password)
        .await
        .context("authentication failed")?;

    eprintln!("Fetching devices…");
    let devices = rest::get_things(&client, &session)
        .await
        .context("could not list devices")?;
    if devices.is_empty() {
        return Err(anyhow!("no devices found on this account"));
    }

    let want = device_override.or_else(|| cfg.device_id.clone());
    let device = match want.as_deref() {
        Some(id) => devices
            .iter()
            .find(|d| d.device_id == id)
            .ok_or_else(|| anyhow!("configured device_id `{id}` not found on account"))?,
        None => &devices[0],
    };
    cache::save(&session, device); // speed up subsequent one-shot CLI calls
    eprintln!(
        "Controlling: {} ({})",
        display_name(device),
        device.device_id
    );

    let app = app::App::new(cfg, client, session, device.clone());
    app::run(app).await
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1).cloned())
}

fn display_name(d: &rest::Device) -> &str {
    if d.nick_name.is_empty() {
        &d.device_name
    } else {
        &d.nick_name
    }
}

fn print_help() {
    println!(
        r#"tclac — control TCL Home smart ACs over the internet

USAGE
  tclac                         launch the interactive TUI dashboard
  tclac <command> [args] [flags]  one-shot scriptable control

COMMANDS
  status                        print current state (add --json for machine output)
  get <field>                   print one field (power, mode, target, current, fan, …)
  on | off                      power on / off (shortcuts)
  power <on|off|toggle>
  mode <auto|cool|dry|fan|heat>
  temp <°C | +N | -N>           set target temperature (clamped to device range)
  fan <auto|1..6|turbo>
  swing <on|off>                vertical swing
  hswing <on|off>               horizontal swing
  vent <h> <v>                  fix vanes at an aim point; h: left..right|1-5, v: top..bottom|1-5
  feature <name> <on|off>       eco, sleep, health, screen, beep, selfclean, antimold, eightheat
  telemetry                     live sensor readout (compressor, coils, outdoor unit, filter, faults)
  energy                        monthly power consumption (kWh) + runtime
  list                          list the ACs on the account
  dump                          print the raw device shadow JSON
  help | version

FLAGS
  --json        machine-readable JSON output (status/get/list/set commands)
  --wait        after a set, read back and print the resulting state
  --device <id> target a specific device (default: first / TCL_DEVICE_ID)
  --no-cache    ignore the cached session and re-authenticate

CREDENTIALS (checked in order)
  1. env vars  TCL_USERNAME / TCL_PASSWORD  (and optional TCL_DEVICE_ID)
  2. config    ~/.config/tclac/config.toml
  3. interactive prompt

EXAMPLES
  tclac status --json
  tclac on && tclac temp 22 && tclac fan turbo
  tclac get current
  tclac mode cool --wait
  tclac feature eco on

Exit codes: 0 ok · 1 error · 2 usage error.
TUI keys: p power · +/- temp · m/1-5 mode · f/F fan · s/h swing · v aim vents (click/arrows) · e/z/d/b/g/c/n/8 features · a anim · r refresh · q quit"#
    );
}
