//! Split-AC state model and command (desired-state) builders.
//!
//! Field mappings verified live against the user's 18T3-PRO shadow (2026-07-16):
//! - Fan is the **7-gear dialect**: `windSpeed7Gear` (0..7) + `windSpeedAutoSwitch`,
//!   no `windSpeed`/`turbo`/`silenceSwitch` keys.
//! - Temperature limits are device-reported (`lower/upperTemperatureLimit`, 16–31),
//!   and `targetFahrenheitTemp` is sent alongside `targetTemperature`.
//! - `workMode: 1` observed while cooling → standard mapping holds
//!   (Auto=0, Cool=1, Dry=2, Fan=3, Heat=4). Raw value still surfaced in UI.

use serde_json::{Map, Value};

pub const FALLBACK_TEMP_MIN: i64 = 16;
pub const FALLBACK_TEMP_MAX: i64 = 31;

pub const SLEEP_LABELS: [&str; 4] = ["Off", "Standard", "Elderly", "Child"];

// ---------------------------------------------------------------------------
// Mode
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Auto,
    Cool,
    Dry,
    Fan,
    Heat,
}

impl Mode {
    pub const CYCLE: [Mode; 5] = [Mode::Auto, Mode::Cool, Mode::Dry, Mode::Fan, Mode::Heat];

    pub fn work_mode(self) -> i64 {
        match self {
            Mode::Auto => 0,
            Mode::Cool => 1,
            Mode::Dry => 2,
            Mode::Fan => 3,
            Mode::Heat => 4,
        }
    }

    pub fn from_work_mode(n: i64) -> Option<Mode> {
        Mode::CYCLE.into_iter().find(|m| m.work_mode() == n)
    }

    pub fn label(self) -> &'static str {
        match self {
            Mode::Auto => "Auto",
            Mode::Cool => "Cool",
            Mode::Dry => "Dry",
            Mode::Fan => "Fan",
            Mode::Heat => "Heat",
        }
    }

    /// Lowercase key for CLI / JSON.
    pub fn key(self) -> &'static str {
        match self {
            Mode::Auto => "auto",
            Mode::Cool => "cool",
            Mode::Dry => "dry",
            Mode::Fan => "fan",
            Mode::Heat => "heat",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Some(Mode::Auto),
            "cool" => Some(Mode::Cool),
            "dry" | "dehumidify" | "dehumidification" => Some(Mode::Dry),
            "fan" | "fan_only" | "fanonly" => Some(Mode::Fan),
            "heat" => Some(Mode::Heat),
            _ => None,
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Mode::Auto => "◎",
            Mode::Cool => "❆",
            Mode::Dry => "≈",
            Mode::Fan => "≋",
            Mode::Heat => "☀",
        }
    }

    pub fn next(self) -> Mode {
        let idx = Mode::CYCLE.iter().position(|&m| m == self).unwrap_or(0);
        Mode::CYCLE[(idx + 1) % Mode::CYCLE.len()]
    }
}

// ---------------------------------------------------------------------------
// Fan (7-gear dialect, with classic fallback)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FanDialect {
    /// `windSpeed7Gear` + `windSpeedAutoSwitch` (this device).
    SevenGear,
    /// Legacy `windSpeed`/`turbo`/`silenceSwitch` scheme.
    Classic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fan {
    Auto,
    Gear(u8), // 1..=6
    Turbo,
}

impl Fan {
    pub const CYCLE: [Fan; 8] = [
        Fan::Auto,
        Fan::Gear(1),
        Fan::Gear(2),
        Fan::Gear(3),
        Fan::Gear(4),
        Fan::Gear(5),
        Fan::Gear(6),
        Fan::Turbo,
    ];

    pub fn label(self) -> String {
        match self {
            Fan::Auto => "Auto".to_string(),
            Fan::Gear(g) => format!("Gear {g}"),
            Fan::Turbo => "Turbo".to_string(),
        }
    }

    /// Lowercase key for CLI / JSON: "auto", "gear3", "turbo".
    pub fn key(self) -> String {
        match self {
            Fan::Auto => "auto".to_string(),
            Fan::Gear(g) => format!("gear{g}"),
            Fan::Turbo => "turbo".to_string(),
        }
    }

    pub fn parse(s: &str) -> Option<Fan> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Some(Fan::Auto),
            "turbo" | "strong" | "max" => Some(Fan::Turbo),
            other => other
                .trim_start_matches("gear")
                .parse::<u8>()
                .ok()
                .filter(|g| (1..=6).contains(g))
                .map(Fan::Gear),
        }
    }

    /// 0 = auto, 1..=6 gears, 7 = turbo. Used for bars/animation speed.
    pub fn gear_level(self) -> u8 {
        match self {
            Fan::Auto => 0,
            Fan::Gear(g) => g,
            Fan::Turbo => 7,
        }
    }

    pub fn next(self) -> Fan {
        let idx = Fan::CYCLE.iter().position(|&f| f == self).unwrap_or(0);
        Fan::CYCLE[(idx + 1) % Fan::CYCLE.len()]
    }

    pub fn prev(self) -> Fan {
        let idx = Fan::CYCLE.iter().position(|&f| f == self).unwrap_or(0);
        Fan::CYCLE[(idx + Fan::CYCLE.len() - 1) % Fan::CYCLE.len()]
    }

    pub fn desired(self, dialect: FanDialect) -> Value {
        match dialect {
            FanDialect::SevenGear => {
                let (auto, gear) = match self {
                    Fan::Auto => (1, 0),
                    Fan::Gear(g) => (0, g as i64),
                    Fan::Turbo => (0, 7),
                };
                obj(&[("windSpeedAutoSwitch", auto), ("windSpeed7Gear", gear)])
            }
            FanDialect::Classic => {
                let (turbo, silence, ws) = match self {
                    Fan::Auto => (0, 0, 0),
                    Fan::Gear(1) => (0, 1, 2), // mute-ish
                    Fan::Gear(2) => (0, 0, 2),
                    Fan::Gear(3) => (0, 0, 3),
                    Fan::Gear(4) => (0, 0, 4),
                    Fan::Gear(5) => (0, 0, 5),
                    Fan::Gear(_) => (0, 0, 6),
                    Fan::Turbo => (1, 0, 6),
                };
                obj(&[
                    ("highTemperatureWind", 0),
                    ("turbo", turbo),
                    ("silenceSwitch", silence),
                    ("windSpeed", ws),
                ])
            }
        }
    }

    fn from_7gear(auto_switch: i64, gear: i64) -> Fan {
        if auto_switch != 0 {
            Fan::Auto
        } else {
            match gear {
                1..=6 => Fan::Gear(gear as u8),
                7 => Fan::Turbo,
                _ => Fan::Auto,
            }
        }
    }

    fn from_classic(ws: i64, turbo: bool, silence: bool) -> Fan {
        if turbo {
            Fan::Turbo
        } else if silence {
            Fan::Gear(1)
        } else {
            match ws {
                0 => Fan::Auto,
                2 => Fan::Gear(2),
                3 => Fan::Gear(3),
                4 => Fan::Gear(4),
                5 => Fan::Gear(5),
                6 => Fan::Gear(6),
                _ => Fan::Auto,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Parsed state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AcState {
    pub power: bool,
    pub work_mode: i64,
    pub target_temp: Option<f64>,
    pub target_f: Option<i64>,
    pub current_temp: Option<f64>,
    pub external_temp: Option<f64>,
    pub coil_temp: Option<f64>,
    pub compressor_hz: Option<i64>,
    pub comp_target_hz: Option<i64>,
    pub comp_run_hz: Option<i64>,
    pub outdoor_fan: Option<i64>,
    pub eev: Option<i64>,
    pub ptc: bool,
    pub filter_alert: bool,
    pub self_clean_pct: Option<i64>,

    pub fan: Fan,
    pub dialect: FanDialect,
    pub wind_pct: Option<i64>,

    /// verticalDirection: 1=swing 2=up-swing 3=down-swing 8=not set 9..13=fix top..bottom
    pub v_dir: i64,
    /// horizontalDirection: 1=swing 2=left 3=middle 4=right 8=not set 9..13=fix L..R
    pub h_dir: i64,

    pub eco: bool,
    pub sleep: i64,
    pub healthy: bool,
    pub screen: bool,
    pub beep: bool,
    pub self_clean: bool,
    pub anti_mold: bool,
    pub eight_heat: bool,

    pub temp_is_f: bool,
    pub t_min: i64,
    pub t_max: i64,

    pub errors: Vec<i64>,
    pub capabilities: Vec<i64>,
    pub shadow_version: i64,
    pub online: bool,
}

impl AcState {
    /// Parse a shadow document. Controllable fields prefer `desired` (pending
    /// command) then fall back to `reported`; sensors read from `reported`.
    pub fn from_shadow(shadow: &Value) -> AcState {
        let reported = shadow
            .pointer("/state/reported")
            .cloned()
            .unwrap_or(Value::Null);
        let desired = shadow
            .pointer("/state/desired")
            .cloned()
            .unwrap_or(Value::Null);

        let pick = |key: &str| -> Option<Value> {
            desired
                .get(key)
                .filter(|v| !v.is_null())
                .or_else(|| reported.get(key))
                .cloned()
        };
        let pi = |key: &str| pick(key).as_ref().and_then(num_i);
        let pf = |key: &str| pick(key).as_ref().and_then(num_f);
        let ri = |key: &str| reported.get(key).and_then(num_i);
        let rf = |key: &str| reported.get(key).and_then(num_f);

        let dialect = if reported.get("windSpeed7Gear").is_some()
            || desired.get("windSpeed7Gear").is_some()
        {
            FanDialect::SevenGear
        } else {
            FanDialect::Classic
        };

        let fan = match dialect {
            FanDialect::SevenGear => Fan::from_7gear(
                pi("windSpeedAutoSwitch").unwrap_or(0),
                pi("windSpeed7Gear").unwrap_or(0),
            ),
            FanDialect::Classic => Fan::from_classic(
                pi("windSpeed").unwrap_or(0),
                pi("turbo").unwrap_or(0) != 0,
                pi("silenceSwitch").unwrap_or(0) != 0,
            ),
        };

        let capabilities = reported
            .get("capabilities")
            .and_then(|c| c.as_array())
            .map(|a| a.iter().filter_map(num_i).collect())
            .unwrap_or_default();
        let errors = reported
            .get("errorCode")
            .and_then(|c| c.as_array())
            .map(|a| a.iter().filter_map(num_i).collect())
            .unwrap_or_default();

        AcState {
            power: pi("powerSwitch").unwrap_or(0) != 0,
            work_mode: pi("workMode").unwrap_or(0),
            target_temp: pf("targetTemperature"),
            target_f: pi("targetFahrenheitTemp"),
            current_temp: rf("currentTemperature"),
            external_temp: rf("externalUnitTemperature"),
            coil_temp: rf("internalUnitCoilTemperature"),
            compressor_hz: ri("compressorFrequency"),
            comp_target_hz: ri("OutDoorCompTarFreqSet"),
            comp_run_hz: ri("OutDoorCompTarFreqRun"),
            outdoor_fan: ri("externalUnitFanSpeed"),
            eev: ri("OutDoorEEVTarOpenDegree"),
            ptc: ri("PTCStatus").unwrap_or(0) != 0,
            filter_alert: ri("filterBlockStatus").unwrap_or(0) != 0
                || ri("filterBlockNotify").unwrap_or(0) != 0,
            self_clean_pct: ri("selfCleanPercentage"),
            fan,
            dialect,
            wind_pct: ri("windSpeedPercentage"),
            v_dir: pi("verticalDirection").unwrap_or(8),
            h_dir: pi("horizontalDirection").unwrap_or(8),
            eco: pi("ECO").unwrap_or(0) != 0,
            sleep: pi("sleep").unwrap_or(0),
            healthy: pi("healthy").unwrap_or(0) != 0,
            screen: pi("screen").unwrap_or(0) != 0,
            beep: pi("beepSwitch").unwrap_or(0) != 0,
            self_clean: pi("selfClean").unwrap_or(0) != 0,
            anti_mold: pi("antiMoldew").unwrap_or(0) != 0,
            eight_heat: pi("eightAddHot").unwrap_or(0) != 0,
            temp_is_f: pi("temperatureType").unwrap_or(0) != 0,
            t_min: ri("lowerTemperatureLimit").unwrap_or(FALLBACK_TEMP_MIN),
            t_max: ri("upperTemperatureLimit").unwrap_or(FALLBACK_TEMP_MAX),
            errors,
            capabilities,
            shadow_version: shadow.get("version").and_then(num_i).unwrap_or(0),
            online: true,
        }
    }

    pub fn mode(&self) -> Option<Mode> {
        Mode::from_work_mode(self.work_mode)
    }

    /// Rough **estimated** live power draw in watts. TCL exposes no measured
    /// wattage; an inverter AC's draw is dominated by compressor frequency, so
    /// this is `fans + ~20 W/Hz × compressor`. A heuristic, not a meter reading.
    pub fn est_power_w(&self) -> Option<i64> {
        let hz = self.compressor_hz? as f64;
        if !self.power {
            return Some(0);
        }
        let fans = 45.0; // indoor + outdoor fans when running
        let compressor = 20.0 * hz.max(0.0);
        Some((fans + compressor).round() as i64)
    }

    pub fn mode_label(&self) -> String {
        match self.mode() {
            Some(m) => m.label().to_string(),
            None => format!("mode#{}", self.work_mode),
        }
    }

    /// Vertical vane fixed at a position? → grid index 0..=4 (top→bottom).
    pub fn v_fix(&self) -> Option<usize> {
        matches!(self.v_dir, 9..=13).then(|| (self.v_dir - 9) as usize)
    }

    /// Horizontal vane fixed at a position? → grid index 0..=4 (left→right).
    pub fn h_fix(&self) -> Option<usize> {
        matches!(self.h_dir, 9..=13).then(|| (self.h_dir - 9) as usize)
    }

    /// Vertical swing active (any sweeping variant)?
    pub fn v_swinging(&self) -> bool {
        matches!(self.v_dir, 1..=3)
    }

    pub fn h_swinging(&self) -> bool {
        matches!(self.h_dir, 1..=4)
    }
}

// ---------------------------------------------------------------------------
// Command builders (return the `desired` object to send)
// ---------------------------------------------------------------------------

pub fn cmd_power(on: bool) -> Value {
    obj(&[("powerSwitch", on as i64)])
}

pub fn cmd_mode(mode: Mode) -> Value {
    obj(&[("workMode", mode.work_mode())])
}

/// Target temperature in °C; device also expects the Fahrenheit mirror.
pub fn cmd_temp(celsius: i64, t_min: i64, t_max: i64) -> Value {
    let c = celsius.clamp(t_min, t_max);
    let f = (c as f64 * 9.0 / 5.0 + 32.0).round() as i64;
    obj(&[("targetTemperature", c), ("targetFahrenheitTemp", f)])
}

pub fn cmd_vswing(on: bool) -> Value {
    obj(&[("verticalDirection", if on { 1 } else { 8 })])
}

/// Vane aim-point labels, index 0..4 (top→bottom / left→right).
pub const VENT_V_LABELS: [&str; 5] = ["top", "upper", "middle", "lower", "bottom"];
pub const VENT_H_LABELS: [&str; 5] = ["left", "mid-left", "center", "mid-right", "right"];

/// Fix both vanes at one aim point. `h`/`v` are 0..=4 grid indices
/// (left→right / top→bottom), mapped to direction values 9..13.
/// Sending a fixed direction stops any active swing on that axis.
pub fn cmd_vent(h: i64, v: i64) -> Value {
    obj(&[
        ("horizontalDirection", 9 + h.clamp(0, 4)),
        ("verticalDirection", 9 + v.clamp(0, 4)),
    ])
}

pub fn cmd_hswing(on: bool) -> Value {
    obj(&[("horizontalDirection", if on { 1 } else { 8 })])
}

/// Generic single-field 0/1 toggle (ECO, screen, beepSwitch, healthy, …).
pub fn cmd_flag(field: &str, on: bool) -> Value {
    obj(&[(field, on as i64)])
}

pub fn cmd_sleep(level: i64) -> Value {
    obj(&[("sleep", level.clamp(0, 3))])
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Format watts as "770 W" or "1.1 kW".
pub fn fmt_power(w: i64) -> String {
    if w >= 1000 {
        format!("{:.1} kW", w as f64 / 1000.0)
    } else {
        format!("{w} W")
    }
}

fn obj(fields: &[(&str, i64)]) -> Value {
    let mut m = Map::new();
    for (k, v) in fields {
        m.insert((*k).to_string(), Value::from(*v));
    }
    Value::Object(m)
}

fn num_i(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_f64().map(|f| f as i64))
        .or_else(|| v.as_bool().map(|b| b as i64))
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

fn num_f(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_i64().map(|i| i as f64))
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}
