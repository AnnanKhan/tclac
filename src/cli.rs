//! Non-interactive, scriptable command line — designed for shell scripts and
//! agents. Every command authenticates (reusing a cached session when possible),
//! performs one action, prints a result, and exits. `--json` gives machine
//! output; exit code is 0 on success, 1 on error, 2 on usage error.

use anyhow::{anyhow, bail, Context, Result};
use chrono::{Datelike, Duration, Local};
use serde_json::{json, Value};

use crate::auth::{self, Session};
use crate::cache;
use crate::config::Config;
use crate::device::{self, AcState, Fan, Mode};
use crate::iot::ShadowClient;
use crate::rest::{self, Device};

struct Opts {
    json: bool,
    wait: bool,
    no_cache: bool,
    device: Option<String>,
    pos: Vec<String>,
}

fn parse_opts(args: Vec<String>) -> Opts {
    let mut o = Opts {
        json: false,
        wait: false,
        no_cache: false,
        device: None,
        pos: Vec::new(),
    };
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--json" => o.json = true,
            "--wait" => o.wait = true,
            "--no-cache" | "--fresh" => o.no_cache = true,
            "--device" | "-D" => o.device = it.next(),
            _ => o.pos.push(a),
        }
    }
    o
}

/// Entry point from `main`. `args` excludes the program name.
pub async fn run(args: Vec<String>) -> Result<()> {
    let o = parse_opts(args);
    let cmd = o.pos.first().cloned().unwrap_or_default();
    let a1 = o.pos.get(1).cloned();
    let a2 = o.pos.get(2).cloned();

    match cmd.as_str() {
        "status" | "state" => cmd_status(&o).await,
        "get" => cmd_get(&o, a1).await,
        "on" => cmd_power(&o, Some("on".into())).await,
        "off" => cmd_power(&o, Some("off".into())).await,
        "power" => cmd_power(&o, a1).await,
        "mode" => cmd_mode(&o, a1).await,
        "temp" | "temperature" => cmd_temp(&o, a1).await,
        "fan" => cmd_fan(&o, a1).await,
        "swing" => cmd_swing(&o, a1, false).await,
        "hswing" => cmd_swing(&o, a1, true).await,
        "vent" | "aim" => cmd_vent(&o, a1, a2).await,
        "feature" | "set" => cmd_feature(&o, a1, a2).await,
        "telemetry" | "telem" => cmd_telemetry(&o).await,
        "energy" | "power-usage" => cmd_energy(&o).await,
        "probe" => cmd_probe(&o).await,
        "list" => cmd_list(&o).await,
        "dump" => cmd_dump(&o).await,
        other => {
            bail_usage(&format!("unknown command: {other}"));
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// connection (with cache + one automatic retry on stale credentials)
// ---------------------------------------------------------------------------

async fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("building HTTP client")
}

/// Resolve a usable (session, device), reusing the cache when valid. The cache
/// is checked *before* loading credentials, so a valid session needs no config
/// (important for head-less / cached invocations).
async fn connect(o: &Opts) -> Result<(Session, Device)> {
    if !o.no_cache {
        if let Some((sess, dev)) = cache::load_valid() {
            if o.device.as_deref().is_none_or(|w| w == dev.device_id) {
                return Ok((sess, dev));
            }
        }
    }
    let cfg = Config::load()?;
    let want = o.device.clone().or(cfg.device_id.clone());
    login_and_select(&cfg, want.as_deref()).await
}

async fn login_and_select(cfg: &Config, want: Option<&str>) -> Result<(Session, Device)> {
    let client = http_client().await?;
    let session = auth::login(&client, &cfg.username, &cfg.password)
        .await
        .context("authentication failed")?;
    let devices = rest::get_things(&client, &session)
        .await
        .context("could not list devices")?;
    let device = match want {
        Some(id) => devices
            .iter()
            .find(|d| d.device_id == id)
            .cloned()
            .ok_or_else(|| anyhow!("device `{id}` not found on account"))?,
        None => devices
            .first()
            .cloned()
            .ok_or_else(|| anyhow!("no devices on account"))?,
    };
    cache::save(&session, &device);
    Ok((session, device))
}

/// Read the raw shadow document, transparently re-authenticating once if the
/// cached creds have gone stale.
async fn read_shadow(o: &Opts) -> Result<(Value, Device, Session)> {
    let (session, device) = connect(o).await?;
    let sc = ShadowClient::new(&session, &device.device_id);
    match sc.get_shadow().await {
        Ok(shadow) => Ok((shadow, device, session)),
        Err(_) => {
            // stale creds → drop cache, log in fresh, retry once
            cache::clear();
            let cfg = Config::load()?;
            let want = o.device.clone().or(cfg.device_id.clone());
            let (session, device) = login_and_select(&cfg, want.as_deref()).await?;
            let sc = ShadowClient::new(&session, &device.device_id);
            let shadow = sc.get_shadow().await?;
            Ok((shadow, device, session))
        }
    }
}

async fn read_state(o: &Opts) -> Result<(AcState, Device, Session)> {
    let (shadow, device, session) = read_shadow(o).await?;
    let mut st = AcState::from_shadow(&shadow);
    st.online = device.is_online;
    Ok((st, device, session))
}

/// Publish a desired-state patch, re-authenticating once on stale creds.
async fn publish(o: &Opts, desired: Value) -> Result<(Session, Device)> {
    let (session, device) = connect(o).await?;
    let sc = ShadowClient::new(&session, &device.device_id);
    if sc.set_desired(desired.clone()).await.is_ok() {
        return Ok((session, device));
    }
    cache::clear();
    let cfg = Config::load()?;
    let want = o.device.clone().or(cfg.device_id.clone());
    let (session, device) = login_and_select(&cfg, want.as_deref()).await?;
    let sc = ShadowClient::new(&session, &device.device_id);
    sc.set_desired(desired)
        .await
        .context("publishing command failed")?;
    Ok((session, device))
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

async fn cmd_status(o: &Opts) -> Result<()> {
    let (st, dev, _) = read_state(o).await?;
    if o.json {
        println!("{}", state_json(&st, &dev));
    } else {
        print_human(&st, &dev);
    }
    Ok(())
}

async fn cmd_get(o: &Opts, field: Option<String>) -> Result<()> {
    let field = field.ok_or_else(|| anyhow!("usage: tclac get <field>"))?;
    let (s, _, _) = read_state(o).await?;
    let v = match field.to_ascii_lowercase().as_str() {
        "power" => onoff(s.power).to_string(),
        "mode" => s
            .mode()
            .map(|m| m.key().to_string())
            .unwrap_or(s.mode_label()),
        "target" | "target_c" => fmt_num(s.target_temp),
        "target_f" => s.target_f.map(|v| v.to_string()).unwrap_or_else(dash),
        "current" | "current_c" | "temp" => fmt_num(s.current_temp),
        "outdoor" | "outdoor_c" => fmt_num(s.external_temp),
        "coil" | "coil_c" => fmt_num(s.coil_temp),
        "fan" => s.fan.key(),
        "fan_gear" => s.fan.gear_level().to_string(),
        "wind_pct" => s.wind_pct.map(|v| v.to_string()).unwrap_or_else(dash),
        "swing" | "vswing" | "v_swing" => onoff(s.v_swinging()).to_string(),
        "hswing" | "h_swing" => onoff(s.h_swinging()).to_string(),
        "online" => s.online.to_string(),
        "eco" => onoff(s.eco).to_string(),
        "sleep" => s.sleep.to_string(),
        "healthy" | "health" => onoff(s.healthy).to_string(),
        "screen" => onoff(s.screen).to_string(),
        "beep" => onoff(s.beep).to_string(),
        "self_clean" | "selfclean" => onoff(s.self_clean).to_string(),
        "anti_mold" | "antimold" => onoff(s.anti_mold).to_string(),
        "eight_heat" | "eightheat" => onoff(s.eight_heat).to_string(),
        other => {
            bail!("unknown field `{other}` (try: power mode target current fan swing online eco …)")
        }
    };
    println!("{v}");
    Ok(())
}

async fn cmd_power(o: &Opts, arg: Option<String>) -> Result<()> {
    let arg = arg.ok_or_else(|| anyhow!("usage: tclac power <on|off|toggle>"))?;
    let on = match arg.to_ascii_lowercase().as_str() {
        "on" | "1" | "true" => true,
        "off" | "0" | "false" => false,
        "toggle" => {
            let (s, _, _) = read_state(o).await?;
            !s.power
        }
        other => bail!("power expects on|off|toggle, got `{other}`"),
    };
    let desired = device::cmd_power(on);
    let (_, dev) = publish(o, desired.clone()).await?;
    done(o, &format!("power {}", onoff(on)), &desired, &dev).await
}

async fn cmd_mode(o: &Opts, arg: Option<String>) -> Result<()> {
    let arg = arg.ok_or_else(|| anyhow!("usage: tclac mode <auto|cool|dry|fan|heat>"))?;
    let mode = Mode::parse(&arg).ok_or_else(|| anyhow!("unknown mode `{arg}`"))?;
    let desired = device::cmd_mode(mode);
    let (_, dev) = publish(o, desired.clone()).await?;
    done(o, &format!("mode {}", mode.key()), &desired, &dev).await
}

async fn cmd_temp(o: &Opts, arg: Option<String>) -> Result<()> {
    let arg = arg.ok_or_else(|| anyhow!("usage: tclac temp <°C | +N | -N>"))?;
    // Need current limits (and current value for relative changes) → read state.
    let (s, dev, _) = read_state(o).await?;
    let target = if let Some(rest) = arg.strip_prefix('+') {
        let d: i64 = rest.parse().context("bad relative temp")?;
        s.target_temp.unwrap_or(24.0).round() as i64 + d
    } else if arg.starts_with('-') && arg.len() > 1 {
        let d: i64 = arg[1..].parse().context("bad relative temp")?;
        s.target_temp.unwrap_or(24.0).round() as i64 - d
    } else {
        arg.parse().context("temp must be a number, +N, or -N")?
    };
    let clamped = target.clamp(s.t_min, s.t_max);
    let desired = device::cmd_temp(clamped, s.t_min, s.t_max);
    publish(o, desired.clone()).await?;
    done(o, &format!("temp {clamped}C"), &desired, &dev).await
}

async fn cmd_fan(o: &Opts, arg: Option<String>) -> Result<()> {
    let arg = arg.ok_or_else(|| anyhow!("usage: tclac fan <auto|1..6|turbo>"))?;
    let fan = Fan::parse(&arg).ok_or_else(|| anyhow!("unknown fan speed `{arg}`"))?;
    // Fan dialect comes from the device shadow.
    let (s, dev, _) = read_state(o).await?;
    let desired = fan.desired(s.dialect);
    publish(o, desired.clone()).await?;
    done(o, &format!("fan {}", fan.key()), &desired, &dev).await
}

async fn cmd_swing(o: &Opts, arg: Option<String>, horizontal: bool) -> Result<()> {
    let name = if horizontal { "hswing" } else { "swing" };
    let arg = arg.ok_or_else(|| anyhow!("usage: tclac {name} <on|off>"))?;
    let on = parse_bool(&arg).ok_or_else(|| anyhow!("{name} expects on|off"))?;
    let desired = if horizontal {
        device::cmd_hswing(on)
    } else {
        device::cmd_vswing(on)
    };
    let (_, dev) = publish(o, desired.clone()).await?;
    done(o, &format!("{name} {}", onoff(on)), &desired, &dev).await
}

/// `tclac vent <h> <v>` — fix both vanes at one of the 5×5 aim points.
async fn cmd_vent(o: &Opts, h: Option<String>, v: Option<String>) -> Result<()> {
    const USAGE: &str = "usage: tclac vent <left|mid-left|center|mid-right|right|1-5> <top|upper|middle|lower|bottom|1-5>";
    let h = h.ok_or_else(|| anyhow!(USAGE))?;
    let v = v.ok_or_else(|| anyhow!(USAGE))?;
    let hi = parse_vent(&h, &device::VENT_H_LABELS)
        .ok_or_else(|| anyhow!("unknown horizontal position `{h}`\n{USAGE}"))?;
    let vi = parse_vent(&v, &device::VENT_V_LABELS)
        .ok_or_else(|| anyhow!("unknown vertical position `{v}`\n{USAGE}"))?;
    let desired = device::cmd_vent(hi as i64, vi as i64);
    let (_, dev) = publish(o, desired.clone()).await?;
    done(
        o,
        &format!(
            "vent {} · {}",
            device::VENT_H_LABELS[hi],
            device::VENT_V_LABELS[vi]
        ),
        &desired,
        &dev,
    )
    .await
}

/// Accept a label ("mid-left", "midleft"), or 1-based index "1".."5".
fn parse_vent(arg: &str, labels: &[&str; 5]) -> Option<usize> {
    let a = arg.to_ascii_lowercase().replace(['-', '_'], "");
    if let Ok(n) = a.parse::<usize>() {
        return (1..=5).contains(&n).then(|| n - 1);
    }
    labels.iter().position(|l| l.replace('-', "") == a)
}

async fn cmd_feature(o: &Opts, name: Option<String>, val: Option<String>) -> Result<()> {
    let name = name.ok_or_else(|| {
        anyhow!("usage: tclac feature <eco|sleep|health|screen|beep|selfclean|antimold|eightheat> <on|off>")
    })?;
    let val = val.ok_or_else(|| anyhow!("usage: tclac feature {name} <on|off>"))?;

    // sleep is multi-level (0..3); on → standard(1)
    if name.eq_ignore_ascii_case("sleep") {
        let level = match val.to_ascii_lowercase().as_str() {
            "on" => 1,
            "off" => 0,
            n => n
                .parse::<i64>()
                .ok()
                .filter(|l| (0..=3).contains(l))
                .ok_or_else(|| anyhow!("sleep expects on|off|0..3"))?,
        };
        let desired = device::cmd_sleep(level);
        let (_, dev) = publish(o, desired.clone()).await?;
        return done(o, &format!("sleep {level}"), &desired, &dev).await;
    }

    let on = parse_bool(&val).ok_or_else(|| anyhow!("{name} expects on|off"))?;
    let field = feature_field(&name).ok_or_else(|| anyhow!("unknown feature `{name}`"))?;
    let desired = device::cmd_flag(field, on);
    let (_, dev) = publish(o, desired.clone()).await?;
    done(o, &format!("{name} {}", onoff(on)), &desired, &dev).await
}

async fn cmd_list(o: &Opts) -> Result<()> {
    let cfg = Config::load()?;
    let client = http_client().await?;
    let session = auth::login(&client, &cfg.username, &cfg.password).await?;
    let devices = rest::get_things(&client, &session).await?;
    if let Some(d) = devices.first() {
        cache::save(&session, d);
    }
    if o.json {
        let arr: Vec<Value> = devices
            .iter()
            .map(|d| {
                json!({
                    "device_id": d.device_id,
                    "name": display_name(d),
                    "device_type": d.device_name,
                    "online": d.is_online,
                    "firmware": d.firmware_version,
                })
            })
            .collect();
        println!("{}", Value::Array(arr));
    } else if devices.is_empty() {
        println!("No devices found.");
    } else {
        for d in &devices {
            println!(
                "{}  [{}]  {}  {}",
                d.device_id,
                d.device_name,
                if d.is_online { "online" } else { "offline" },
                display_name(d)
            );
        }
    }
    Ok(())
}

async fn cmd_dump(o: &Opts) -> Result<()> {
    let (shadow, _, _) = read_shadow(o).await?;
    println!("{}", serde_json::to_string_pretty(&shadow)?);
    Ok(())
}

// ---------------------------------------------------------------------------
// telemetry (live readings straight from the shadow)
// ---------------------------------------------------------------------------

async fn cmd_telemetry(o: &Opts) -> Result<()> {
    let (shadow, dev, _) = read_shadow(o).await?;
    let st = AcState::from_shadow(&shadow);
    let r = shadow
        .pointer("/state/reported")
        .cloned()
        .unwrap_or(Value::Null);
    let f = |k: &str| {
        r.get(k)
            .and_then(|v| v.as_f64().or_else(|| v.as_i64().map(|i| i as f64)))
    };
    let i = |k: &str| r.get(k).and_then(|v| v.as_i64());
    let errors: Vec<i64> = r
        .get("errorCode")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_i64()).collect())
        .unwrap_or_default();

    if o.json {
        println!(
            "{}",
            json!({
                "device_id": dev.device_id,
                "indoor_c": f("currentTemperature"),
                "outdoor_c": f("externalUnitTemperature"),
                "coil_c": f("internalUnitCoilTemperature"),
                "compressor_hz": i("compressorFrequency"),
                "compressor_target_hz": i("OutDoorCompTarFreqSet"),
                "compressor_run_hz": i("OutDoorCompTarFreqRun"),
                "outdoor_fan_rpm": i("OutDoorFanTarSpeed"),
                "outdoor_fan_speed": i("externalUnitFanSpeed"),
                "eev_open_degree": i("OutDoorEEVTarOpenDegree"),
                "ptc_heater": i("PTCStatus"),
                "wind_pct": i("windSpeedPercentage"),
                "est_power_w": st.est_power_w(),
                "filter_block_status": i("filterBlockStatus"),
                "filter_clean_notify": i("filterBlockNotify"),
                "self_clean_status": i("selfCleanStatus"),
                "self_clean_pct": i("selfCleanPercentage"),
                "generator_mode": i("generatorMode"),
                "error_codes": errors,
            })
        );
    } else {
        println!("Telemetry — {} [{}]", display_name(&dev), dev.device_name);
        let t = |v: Option<f64>| v.map(|x| format!("{x:.1}")).unwrap_or_else(dash);
        let n = |v: Option<i64>| v.map(|x| x.to_string()).unwrap_or_else(dash);
        println!("  Temperatures");
        println!("    indoor        {} °C", t(f("currentTemperature")));
        println!("    outdoor       {} °C", t(f("externalUnitTemperature")));
        println!(
            "    indoor coil   {} °C",
            t(f("internalUnitCoilTemperature"))
        );
        println!("  Compressor / outdoor unit");
        println!(
            "    compressor    {} Hz  (target {} / run {})",
            n(i("compressorFrequency")),
            n(i("OutDoorCompTarFreqSet")),
            n(i("OutDoorCompTarFreqRun"))
        );
        println!(
            "    outdoor fan   speed {} / target {}",
            n(i("externalUnitFanSpeed")),
            n(i("OutDoorFanTarSpeed"))
        );
        println!("    EEV opening   {}", n(i("OutDoorEEVTarOpenDegree")));
        println!(
            "    PTC heater    {}",
            if i("PTCStatus").unwrap_or(0) != 0 {
                "on"
            } else {
                "off"
            }
        );
        println!(
            "    est. power    ~{} (estimated from compressor Hz — not a meter reading)",
            st.est_power_w().map(device::fmt_power).unwrap_or_else(dash)
        );
        println!("  Airflow / maintenance");
        println!("    wind output   {} %", n(i("windSpeedPercentage")));
        println!(
            "    filter        {}",
            if i("filterBlockStatus").unwrap_or(0) != 0 || i("filterBlockNotify").unwrap_or(0) != 0
            {
                "CLEAN NEEDED"
            } else {
                "ok"
            }
        );
        println!(
            "    self-clean    status {} · {}%",
            n(i("selfCleanStatus")),
            n(i("selfCleanPercentage"))
        );
        println!("  Diagnostics");
        println!(
            "    errors        {}",
            if errors.is_empty() {
                "none".to_string()
            } else {
                format!("{errors:?}")
            }
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// energy (historical consumption / runtime REST endpoints)
// ---------------------------------------------------------------------------

async fn fetch_json(client: &reqwest::Client, session: &Session, url: &str) -> Option<Value> {
    let (status, body) = rest::signed_get(client, session, url).await.ok()?;
    if status != 200 {
        return None;
    }
    serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v.get("data").cloned())
}

/// Format a work-time value (TCL reports minutes) as "Xh Ym".
fn fmt_worktime(minutes: i64) -> String {
    if minutes <= 0 {
        return "0".to_string();
    }
    let (h, m) = (minutes / 60, minutes % 60);
    if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

async fn cmd_energy(o: &Opts) -> Result<()> {
    let (session, device) = connect(o).await?;
    let client = http_client().await?;
    let base = &session.device_url;
    let id = &device.device_id;
    let now = Local::now();
    let (y, m) = (now.year(), now.month());

    let cons = fetch_json(
        &client,
        &session,
        &format!("{base}/v3/ac/{id}/power/consumption/info/{y}/{m:02}"),
    )
    .await;
    let work = fetch_json(
        &client,
        &session,
        &format!("{base}/v3/ac/{id}/work-time/info/{y}/{m:02}"),
    )
    .await;

    if o.json {
        println!(
            "{}",
            json!({ "device_id": id, "period": format!("{y}-{m:02}"),
                    "consumption": cons, "work_time": work })
        );
        return Ok(());
    }

    let ef = |sect: Option<&Value>, k: &str| {
        sect.and_then(|x| x.get(k))
            .and_then(|x| x.as_f64())
            .unwrap_or(0.0)
    };
    let wi = |sect: Option<&Value>, k: &str| {
        sect.and_then(|x| x.get(k))
            .and_then(|x| x.as_i64())
            .unwrap_or(0)
    };

    println!("Energy & runtime — {} ({y}-{m:02})", display_name(&device));

    match &cons {
        Some(c) => {
            let cur = c.get("currStatisticsRes");
            let prev = c.get("beforeStatisticsRes");
            println!("  Consumption (kWh)");
            println!(
                "    this month  {:.2}   (online {:.2} · offline {:.2} · AI {:.2})",
                ef(cur, "totalElectricity"),
                ef(cur, "onlineElectricity"),
                ef(cur, "offlineElectricity"),
                ef(cur, "aiElectricity"),
            );
            println!("    last month  {:.2}", ef(prev, "totalElectricity"));
            if let Some(days) = c.get("consumptionDetails").and_then(|d| d.as_array()) {
                if !days.is_empty() {
                    println!("    by day:");
                    for d in days {
                        let date = d.get("date").and_then(|x| x.as_str()).unwrap_or("?");
                        println!("      {date}  {:.2}", ef(Some(d), "totalElectricity"));
                    }
                }
            }
        }
        None => println!("  Consumption: (unavailable)"),
    }

    match &work {
        Some(w) => {
            let cur = w.get("currentTotalWorkTime");
            let prev = w.get("beforeTotalWorkTime");
            println!("  Runtime");
            println!(
                "    this month  {}   (AI {})",
                fmt_worktime(wi(cur, "workTime")),
                fmt_worktime(wi(cur, "aiWorkTime"))
            );
            println!("    last month  {}", fmt_worktime(wi(prev, "workTime")));
        }
        None => println!("  Runtime: (unavailable)"),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// probe — discover which auxiliary endpoints this account/model exposes
// ---------------------------------------------------------------------------

async fn cmd_probe(o: &Opts) -> Result<()> {
    let (session, device) = connect(o).await?;
    let client = http_client().await?;
    let base = &session.device_url;
    let id = &device.device_id;

    let now = Local::now();
    let (y, m, d) = (now.year(), now.month(), now.day());
    let weekday = now.weekday().num_days_from_monday() as i64;
    let monday = now.date_naive() - Duration::days(weekday);
    let sunday = monday + Duration::days(6);
    let week = format!("{}-{}", monday.format("%Y%m%d"), sunday.format("%Y%m%d"));

    let urls = [
        format!("{base}/v3/ac/{id}/power/consumption/info/{y}/{m:02}"),
        format!("{base}/v3/ac/{id}/power/consumption/info/{y}"),
        format!("{base}/v3/ac/{id}/power/consumption/info"),
        format!("{base}/v3/ac/{id}/power/consumption/info/{y}/{m:02}/{d:02}"),
        format!("{base}/v3/ac/{id}/work-time/info"),
        format!("{base}/v3/ac/{id}/work-time/info?week={week}"),
        format!("{base}/v3/ac/{id}/work-time/info/{y}/{m:02}"),
        format!("{base}/v3/ac/{id}/energy/info/{y}/{m:02}"),
        // candidate real-time / instantaneous power endpoints
        format!("{base}/v3/ac/{id}/power/consumption/info/realtime"),
        format!("{base}/v3/ac/{id}/power/realtime"),
        format!("{base}/v3/ac/{id}/power/realtime/info"),
        format!("{base}/v3/ac/{id}/realtime/power"),
        format!("{base}/v3/ac/{id}/power/current"),
        format!("{base}/v3/ac/{id}/electricity/realtime"),
    ];

    for url in urls {
        match rest::signed_get(&client, &session, &url).await {
            Ok((status, body)) => {
                let pretty = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|v| serde_json::to_string_pretty(&v).ok())
                    .unwrap_or(body);
                let shown = if pretty.chars().count() > 1500 {
                    let s: String = pretty.chars().take(1500).collect();
                    format!("{s}…")
                } else {
                    pretty
                };
                println!("── {status}  {url}\n{shown}\n");
            }
            Err(e) => println!("── ERR  {url}\n{e}\n"),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// output helpers
// ---------------------------------------------------------------------------

/// Emit the result of a set command; optionally read back the new state.
async fn done(o: &Opts, action: &str, desired: &Value, dev: &Device) -> Result<()> {
    if o.wait {
        tokio::time::sleep(std::time::Duration::from_millis(900)).await;
        let (st, dev2, _) = read_state(o).await?;
        if o.json {
            println!(
                "{}",
                json!({ "ok": true, "action": action, "sent": desired, "state": state_json(&st, &dev2) })
            );
        } else {
            println!("ok: {action}");
            print_human(&st, &dev2);
        }
    } else if o.json {
        println!(
            "{}",
            json!({ "ok": true, "action": action, "sent": desired, "device_id": dev.device_id })
        );
    } else {
        println!("ok: {action}");
    }
    Ok(())
}

fn state_json(s: &AcState, dev: &Device) -> Value {
    json!({
        "device_id": dev.device_id,
        "name": display_name(dev),
        "device_type": dev.device_name,
        "online": dev.is_online,
        "power": s.power,
        "mode": s.mode().map(|m| m.key()),
        "work_mode": s.work_mode,
        "target_c": s.target_temp,
        "target_f": s.target_f,
        "current_c": s.current_temp,
        "outdoor_c": s.external_temp,
        "coil_c": s.coil_temp,
        "compressor_hz": s.compressor_hz,
        "fan": s.fan.key(),
        "fan_gear": s.fan.gear_level(),
        "wind_pct": s.wind_pct,
        "v_swing": s.v_swinging(),
        "h_swing": s.h_swinging(),
        "v_dir": s.v_dir,
        "h_dir": s.h_dir,
        "eco": s.eco,
        "sleep": s.sleep,
        "healthy": s.healthy,
        "screen": s.screen,
        "beep": s.beep,
        "self_clean": s.self_clean,
        "anti_mold": s.anti_mold,
        "eight_heat": s.eight_heat,
        "temp_min": s.t_min,
        "temp_max": s.t_max,
        "shadow_version": s.shadow_version,
    })
}

fn print_human(s: &AcState, dev: &Device) {
    let online = if dev.is_online { "online" } else { "offline" };
    println!("{} [{}]  {}", display_name(dev), dev.device_name, online);
    println!("  power    {}", onoff(s.power));
    println!("  mode     {} (workMode={})", s.mode_label(), s.work_mode);
    println!(
        "  target   {}C{}",
        fmt_num(s.target_temp),
        s.target_f.map(|f| format!(" / {f}F")).unwrap_or_default()
    );
    println!("  current  {}C", fmt_num(s.current_temp));
    println!("  fan      {} (gear {})", s.fan.label(), s.fan.gear_level());
    println!(
        "  swing    V:{}  H:{}",
        onoff(s.v_swinging()),
        onoff(s.h_swinging())
    );
    println!(
        "  sensors  outdoor {}C · coil {}C · compressor {} Hz · wind {}%",
        fmt_num(s.external_temp),
        fmt_num(s.coil_temp),
        s.compressor_hz.unwrap_or(0),
        s.wind_pct.unwrap_or(0)
    );
    let flags = [
        ("eco", s.eco),
        ("health", s.healthy),
        ("screen", s.screen),
        ("beep", s.beep),
        ("self-clean", s.self_clean),
        ("anti-mold", s.anti_mold),
        ("8C-heat", s.eight_heat),
    ];
    let on: Vec<&str> = flags.iter().filter(|(_, v)| *v).map(|(k, _)| *k).collect();
    println!(
        "  features {}  (sleep {})",
        if on.is_empty() {
            "none".to_string()
        } else {
            on.join(", ")
        },
        s.sleep
    );
}

fn feature_field(name: &str) -> Option<&'static str> {
    match name.to_ascii_lowercase().as_str() {
        "eco" => Some("ECO"),
        "health" | "healthy" => Some("healthy"),
        "screen" | "display" => Some("screen"),
        "beep" => Some("beepSwitch"),
        "selfclean" | "self-clean" | "self_clean" => Some("selfClean"),
        "antimold" | "anti-mold" | "anti_mold" => Some("antiMoldew"),
        "eightheat" | "8heat" | "8c-heat" | "eight_heat" => Some("eightAddHot"),
        _ => None,
    }
}

fn display_name(d: &Device) -> &str {
    if d.nick_name.is_empty() {
        &d.device_name
    } else {
        &d.nick_name
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.to_ascii_lowercase().as_str() {
        "on" | "1" | "true" | "yes" => Some(true),
        "off" | "0" | "false" | "no" => Some(false),
        _ => None,
    }
}

fn onoff(b: bool) -> &'static str {
    if b {
        "on"
    } else {
        "off"
    }
}

fn fmt_num(v: Option<f64>) -> String {
    v.map(|x| {
        if (x.round() - x).abs() < 0.05 {
            format!("{:.0}", x)
        } else {
            format!("{:.1}", x)
        }
    })
    .unwrap_or_else(dash)
}

fn dash() -> String {
    "-".to_string()
}

fn bail_usage(msg: &str) {
    eprintln!("{msg}\n\nRun `tclac help` for usage.");
    std::process::exit(2);
}
