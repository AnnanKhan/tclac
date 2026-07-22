# tclac — agent & scripting guide

`tclac` controls a **TCL Home** smart air conditioner over TCL's cloud (works from
anywhere, no LAN access needed). This document is the contract for using it
non-interactively from scripts or agents.

## Install & prerequisites

```bash
cargo install --path .    # installs `tclac` to ~/.cargo/bin (must be on PATH)
```

Credentials are read, in order, from:
1. env vars `TCL_USERNAME` / `TCL_PASSWORD` (and optional `TCL_DEVICE_ID`)
2. `~/.config/tclac/config.toml`
3. interactive prompt (not usable head-less — set env vars or the config file)

For agents, **set the env vars** before invoking.

## Model

- Commands are one-shot: authenticate → act → print → exit.
- A session (temporary AWS credentials, ~1h) is cached at `~/.cache/tclac/session.json`,
  so back-to-back commands are fast (~1s) instead of re-running the full login (~6s).
  Stale credentials are detected and refreshed automatically.
- Sending a command **publishes** a desired-state change to the device shadow. It is
  fire-and-forget: success means "accepted by the cloud", not "device confirmed".
  Use `--wait` (reads state back after ~1s) or a follow-up `tclac status` to verify.
- State reflects the cloud's last-known shadow. If the AC is **offline**, reads return
  the last values and writes queue until it reconnects.

## Exit codes

| code | meaning        |
|------|----------------|
| 0    | success        |
| 1    | runtime error (auth, network, unknown field/value) — message on stderr |
| 2    | usage error (unknown command) |

## Commands

```
tclac status [--json]                 current state
tclac get <field>                     one field, raw (for shell)
tclac on | off                        power shortcuts
tclac power <on|off|toggle>
tclac mode <auto|cool|dry|fan|heat>
tclac temp <°C | +N | -N>             clamped to the device's min/max
tclac fan <auto|1..6|turbo>
tclac swing <on|off>                  vertical
tclac hswing <on|off>                 horizontal
tclac vent <h> <v>                    fix both vanes at an aim point:
                                      h = left|mid-left|center|mid-right|right or 1-5
                                      v = top|upper|middle|lower|bottom or 1-5
                                      (implicitly stops swing; `swing`/`hswing on` resumes it)
tclac feature <name> <on|off>         eco | sleep | health | screen | beep | selfclean | antimold | eightheat
tclac telemetry [--json]              live sensor readout (compressor/coil/outdoor/filter/faults)
tclac energy [--json]                 monthly consumption (kWh) + runtime
tclac list [--json]
tclac dump                            raw shadow JSON (debugging)
```

Global flags: `--json`, `--wait`, `--device <id>`, `--no-cache`.

`get <field>` accepts: `power mode target current outdoor coil fan fan_gear wind_pct
swing hswing online eco sleep healthy screen beep self_clean anti_mold eight_heat`.
It prints a bare token (`on`, `cool`, `26`, `24.9`, `gear6`, `true`) — ideal for
`$(...)` in shell.

## JSON: `tclac status --json`

```json
{
  "device_id": "DiysFCvgAAE",
  "name": "Split AC",
  "device_type": "Split AC",
  "online": true,
  "power": true,
  "mode": "cool",
  "work_mode": 1,
  "target_c": 26.0,
  "target_f": 79,
  "current_c": 24.9,
  "outdoor_c": 35.0,
  "coil_c": 26.0,
  "compressor_hz": 0,
  "fan": "gear6",
  "fan_gear": 6,
  "wind_pct": 85,
  "v_swing": false,
  "h_swing": false,
  "v_dir": 13,
  "h_dir": 8,
  "eco": false,
  "sleep": 0,
  "healthy": false,
  "screen": true,
  "beep": false,
  "self_clean": false,
  "anti_mold": false,
  "eight_heat": false,
  "temp_min": 16,
  "temp_max": 31,
  "shadow_version": 396
}
```

Any reading may be `null` if the device didn't report it. `mode` is `null` if the raw
`work_mode` integer doesn't map to a known mode (fall back to `work_mode`).

A set command with `--json` prints `{"ok":true,"action":"...","sent":{...},"device_id":"..."}`
(or, with `--wait`, an additional `"state"` object).

## JSON: `tclac telemetry --json`

Live values from the shadow (any may be `null`):

```json
{
  "device_id": "…", "indoor_c": 27.8, "outdoor_c": 39.0, "coil_c": 18.0,
  "compressor_hz": 60, "compressor_target_hz": 0, "compressor_run_hz": 36,
  "outdoor_fan_rpm": 0, "outdoor_fan_speed": 920, "eev_open_degree": 0,
  "ptc_heater": 0, "wind_pct": 0, "est_power_w": 1245,
  "filter_block_status": 0, "filter_clean_notify": 0,
  "self_clean_status": 6, "self_clean_pct": 0, "generator_mode": 0, "error_codes": []
}
```

**No live wattage exists in TCL's API** — there is no power/current/voltage field and no
real-time power endpoint (verified: all candidates 404). `est_power_w` is a **heuristic**
(fans + ~20 W/Hz × compressor frequency), not a meter reading. The only *measured* energy
data is cumulative kWh from `tclac energy` (hourly granularity at best).

## JSON: `tclac energy --json`

Two REST endpoints, current month. `consumption` electricity fields are **kWh**;
`work_time` values are **minutes** (both as TCL reports them — verify once real usage
accrues). `consumptionDetails` / `workTimeDetails` are per-day breakdowns (empty until
there's history).

```json
{
  "device_id": "…", "period": "2026-07",
  "consumption": { "currStatisticsRes": {"totalElectricity":0,"onlineElectricity":0,
                   "offlineElectricity":0,"aiElectricity":0,"date":"2026-07"},
                   "beforeStatisticsRes": {…}, "consumptionDetails": [], "timeZone":"Asia/Karachi" },
  "work_time":   { "currentTotalWorkTime": {"workTime":0,"aiWorkTime":0,"date":"2026-07"},
                   "beforeTotalWorkTime": {…}, "workTimeDetails": [] }
}
```

## Recipes

```bash
export TCL_USERNAME="you@example.com" TCL_PASSWORD="…"

# read a value
tclac get current                      # -> 24.9

# cool the room to 22 on high fan
tclac on && tclac mode cool && tclac temp 22 && tclac fan turbo

# conditional logic
[ "$(tclac get power)" = on ] && tclac off

# structured read for an agent
tclac status --json | jq '.current_c, .target_c, .mode'

# confirm a change landed
tclac temp 24 --wait --json | jq '.state.target_c'
```

## Guidance for agents

- **Don't spam commands.** The cloud rate-limits; space rapid changes out, and prefer a
  single `temp 22` over many `+1` steps. On a `Throttling` error, wait ~15s and retry.
- Prefer `status --json` for reads and parse fields you need; use `get <field>` for a
  single value in shell conditionals.
- Treat a set as accepted-by-cloud; if you need confirmation, use `--wait` or re-read.
- `temp` is clamped to `temp_min`/`temp_max` from the device — read those from `status`
  if you need to validate before sending.
- Target one device explicitly with `--device <id>` (from `tclac list --json`) on
  multi-AC accounts.
