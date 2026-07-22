//! ratatui rendering: themed, animated AC dashboard.
//!
//! Layout (≥ ~100×30 recommended, degrades by clipping):
//!   header
//!   [ big temp ][ mode & power ][ fan ][ swing ]
//!   [ toggles / extras ]
//!   [ sensors | indoor-temp sparkline ]
//!   [ activity log | key reference ]
//!   footer

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Sparkline, Wrap};
use ratatui::Frame;

use crate::app::App;
use crate::device::{self, AcState, Fan, Mode, SLEEP_LABELS};

// ---------------------------------------------------------------------------
// palette / theme
// ---------------------------------------------------------------------------

const COL_TEXT: Color = Color::Rgb(232, 236, 243); // primary text (near-white)
const COL_DIM: Color = Color::Rgb(176, 184, 200); // secondary text — legible on dark
const COL_FAINT: Color = Color::Rgb(126, 134, 152); // borders / inactive decoration
const COL_TRACK: Color = Color::Rgb(88, 95, 112); // empty bar track (non-text only)
const COL_OK: Color = Color::Rgb(112, 214, 140);
const COL_ERR: Color = Color::Rgb(255, 110, 110);

/// Accent color follows the active mode (grey when off / unknown).
fn accent(app: &App) -> Color {
    match app.state.as_ref() {
        Some(s) if s.power => match s.mode() {
            Some(Mode::Cool) => Color::Rgb(90, 175, 255),
            Some(Mode::Heat) => Color::Rgb(255, 125, 95),
            Some(Mode::Dry) => Color::Rgb(235, 200, 105),
            Some(Mode::Fan) => Color::Rgb(120, 220, 150),
            Some(Mode::Auto) => Color::Rgb(195, 150, 255),
            None => Color::Rgb(150, 160, 180),
        },
        _ => Color::Rgb(120, 128, 145),
    }
}

fn panel<'a>(title: &'a str, ac: Color) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(COL_FAINT))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(ac).add_modifier(Modifier::BOLD),
        ))
}

fn chip(text: String, on: bool, on_bg: Color) -> Span<'static> {
    if on {
        Span::styled(
            format!(" {text} "),
            Style::default()
                .fg(Color::Rgb(15, 18, 24))
                .bg(on_bg)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(format!(" {text} "), Style::default().fg(COL_DIM))
    }
}

fn key_hint(k: &str) -> Span<'static> {
    Span::styled(format!("[{k}]"), Style::default().fg(COL_FAINT))
}

// ---------------------------------------------------------------------------
// root
// ---------------------------------------------------------------------------

pub const MIN_W: u16 = 106;
pub const MIN_H: u16 = 32;

pub fn render(f: &mut Frame, app: &App) {
    let area = f.area();
    if area.width < MIN_W || area.height < MIN_H {
        render_too_small(f, area);
        return;
    }

    let rows = Layout::vertical([
        Constraint::Length(1),  // header
        Constraint::Length(14), // main panels (fits the Braille fan)
        Constraint::Length(4),  // toggles
        Constraint::Length(7),  // sensors + outdoor unit
        Constraint::Min(5),     // log + keys
        Constraint::Length(1),  // footer
    ])
    .split(area);

    render_header(f, rows[0], app);
    render_main(f, rows[1], app);
    render_toggles(f, rows[2], app);
    render_sensors(f, rows[3], app);
    render_bottom(f, rows[4], app);
    render_footer(f, rows[5], app);

    if app.aim_open {
        render_aim(f, area, app);
    }
}

// ---------------------------------------------------------------------------
// aim-airflow overlay: a square "room" seen from the AC; pick a point and the
// vanes are fixed to the matching 5×5 protocol position (dirs 9..13 × 9..13).
// ---------------------------------------------------------------------------

const AIM_CELL_W: u16 = 7; // grid cell footprint (chars)
const AIM_CELL_H: u16 = 2;
const AIM_GRID_W: u16 = 5 * AIM_CELL_W; // 35
const AIM_GRID_H: u16 = 5 * AIM_CELL_H - 1; // 9: dot rows at 0,2,4,6,8
const AIM_POP_W: u16 = AIM_GRID_W + 4; // borders + 1 pad each side
const AIM_POP_H: u16 = AIM_GRID_H + 7; // borders + AC line + gaps + label + hint

/// Centered popup rect for the aim overlay.
pub fn aim_popup_rect(term: Rect) -> Rect {
    let w = AIM_POP_W.min(term.width);
    let h = AIM_POP_H.min(term.height);
    Rect::new(
        term.x + (term.width.saturating_sub(w)) / 2,
        term.y + (term.height.saturating_sub(h)) / 2,
        w,
        h,
    )
}

/// Top-left terminal cell of the 5×5 grid inside the popup.
fn aim_grid_origin(term: Rect) -> (u16, u16) {
    let p = aim_popup_rect(term);
    (p.x + 2, p.y + 3) // border+pad, border + AC line + gap
}

/// Which grid cell (h, v) a terminal click at (x, y) lands in, if any.
pub fn aim_cell_at(term: Rect, x: u16, y: u16) -> Option<(usize, usize)> {
    let (ox, oy) = aim_grid_origin(term);
    if x < ox || y < oy || x >= ox + AIM_GRID_W || y >= oy + AIM_GRID_H {
        return None;
    }
    let h = ((x - ox) / AIM_CELL_W) as usize;
    let v = (((y - oy) + 1) / AIM_CELL_H) as usize; // round to nearest dot row
    Some((h.min(4), v.min(4)))
}

fn render_aim(f: &mut Frame, term: Rect, app: &App) {
    let ac = accent(app);
    let pop = aim_popup_rect(term);
    f.render_widget(Clear, pop);

    let (fix_h, fix_v) = match &app.state {
        Some(s) => (s.h_fix(), s.v_fix()),
        None => (None, None),
    };
    let swinging = app
        .state
        .as_ref()
        .map(|s| s.v_swinging() || s.h_swinging())
        .unwrap_or(false);

    let mut lines: Vec<Line> = vec![];

    // the AC unit on the wall at the top (air source)
    let left = ((AIM_GRID_W as usize) - 8) / 2;
    let right = AIM_GRID_W as usize - 8 - left;
    lines.push(Line::from(vec![
        Span::styled("▬".repeat(left), Style::default().fg(COL_TRACK)),
        Span::styled(
            "[ ❆ AC ]",
            Style::default().fg(ac).add_modifier(Modifier::BOLD),
        ),
        Span::styled("▬".repeat(right), Style::default().fg(COL_TRACK)),
    ]));
    lines.push(Line::from(""));

    // 5×5 grid: dot rows at even lines, air-stream guide down the cursor column
    let cur_line = (app.aim_v as u16) * AIM_CELL_H; // grid line of the crosshair
    for gy in 0..AIM_GRID_H {
        let mut spans: Vec<Span> = vec![];
        if gy % AIM_CELL_H == 0 {
            let row = (gy / AIM_CELL_H) as usize;
            for col in 0..5usize {
                let cursor = col == app.aim_h && row == app.aim_v;
                let applied = fix_h == Some(col) && fix_v == Some(row);
                let (ch, st) = match (cursor, applied) {
                    (true, true) => ("◉", Style::default().fg(ac).add_modifier(Modifier::BOLD)),
                    (true, false) => ("◎", Style::default().fg(ac).add_modifier(Modifier::BOLD)),
                    (false, true) => ("●", Style::default().fg(COL_OK)),
                    (false, false) => ("·", Style::default().fg(COL_FAINT)),
                };
                spans.push(Span::raw("   "));
                spans.push(Span::styled(ch, st));
                spans.push(Span::raw("   "));
            }
        } else {
            // spacer line: faint stream from the AC down to the crosshair
            for col in 0..5u16 {
                let on_stream = col as usize == app.aim_h && gy < cur_line;
                spans.push(Span::raw("   "));
                spans.push(if on_stream {
                    Span::styled("┊", Style::default().fg(ac))
                } else {
                    Span::raw(" ")
                });
                spans.push(Span::raw("   "));
            }
        }
        lines.push(Line::from(spans));
    }

    lines.push(Line::from(""));
    // label: where the crosshair points + swing warning
    let mut label = vec![
        Span::styled(" aim ", Style::default().fg(COL_DIM)),
        Span::styled(
            format!(
                "{} · {}",
                device::VENT_V_LABELS[app.aim_v],
                device::VENT_H_LABELS[app.aim_h]
            ),
            Style::default().fg(ac).add_modifier(Modifier::BOLD),
        ),
    ];
    if swinging {
        label.push(Span::styled(
            "   swing on → will stop",
            Style::default().fg(COL_DIM),
        ));
    }
    lines.push(Line::from(label));
    lines.push(Line::from(Span::styled(
        " click · ←↑↓→ · Enter apply · Esc",
        Style::default().fg(COL_FAINT),
    )));

    let block = panel("Aim airflow", ac);
    f.render_widget(
        Paragraph::new(lines).block(block.padding(ratatui::widgets::Padding::horizontal(1))),
        pop,
    );
}

fn render_too_small(f: &mut Frame, area: Rect) {
    let msg = vec![
        Line::from(Span::styled(
            "❆ tclac",
            Style::default()
                .fg(Color::Rgb(90, 175, 255))
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Terminal too small for the dashboard.",
            Style::default().fg(COL_TEXT),
        )),
        Line::from(Span::styled(
            format!(
                "Resize to at least {MIN_W}×{MIN_H}  (now {}×{}).",
                area.width, area.height
            ),
            Style::default().fg(COL_DIM),
        )),
    ];
    let v = Layout::vertical([
        Constraint::Percentage(40),
        Constraint::Length(4),
        Constraint::Min(0),
    ])
    .split(area);
    f.render_widget(Paragraph::new(msg).alignment(Alignment::Center), v[1]);
}

// ---------------------------------------------------------------------------
// header / footer
// ---------------------------------------------------------------------------

fn render_header(f: &mut Frame, area: Rect, app: &App) {
    let ac = accent(app);
    let name = if app.device.nick_name.is_empty() {
        app.device.device_name.clone()
    } else {
        app.device.nick_name.clone()
    };
    let online = if app.device.is_online {
        Span::styled(" ● online", Style::default().fg(COL_OK))
    } else {
        Span::styled(" ● offline", Style::default().fg(COL_ERR))
    };
    let fw = if app.device.firmware_version.is_empty() {
        String::new()
    } else {
        format!("  fw {}", app.device.firmware_version)
    };
    let line = Line::from(vec![
        Span::styled(
            " ❆ tclac ",
            Style::default()
                .fg(Color::Rgb(15, 18, 24))
                .bg(ac)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {name} "),
            Style::default().fg(COL_TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("({})", app.device.device_id),
            Style::default().fg(COL_DIM),
        ),
        online,
        Span::styled(fw, Style::default().fg(COL_DIM)),
        Span::styled(
            format!("   {}", chrono::Local::now().format("%H:%M:%S")),
            Style::default().fg(COL_DIM),
        ),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn render_footer(f: &mut Frame, area: Rect, app: &App) {
    let busy = if app.busy { "⟳ " } else { "" };
    let style = if app.status.starts_with("Error") || app.status.contains("error") {
        Style::default().fg(COL_ERR)
    } else {
        Style::default().fg(accent(app))
    };
    let ver = app
        .state
        .as_ref()
        .map(|s| {
            format!(
                "shadow v{} · {} capabilities",
                s.shadow_version,
                s.capabilities.len()
            )
        })
        .unwrap_or_default();
    let line = Line::from(vec![
        Span::styled(format!(" {busy}{}", app.status), style),
        Span::styled(
            format!(
                "  ·  updated {}  ·  anim {}  ·  {}",
                app.refresh_age(),
                if app.anim { "on" } else { "off" },
                ver
            ),
            Style::default().fg(COL_DIM),
        ),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

// ---------------------------------------------------------------------------
// main row: temp / mode / fan / swing
// ---------------------------------------------------------------------------

fn render_main(f: &mut Frame, area: Rect, app: &App) {
    let cols = Layout::horizontal([
        Constraint::Length(30),
        Constraint::Length(22),
        Constraint::Min(28),
        Constraint::Length(26),
    ])
    .split(area);

    render_temp(f, cols[0], app);
    render_mode(f, cols[1], app);
    render_fan(f, cols[2], app);
    render_swing(f, cols[3], app);
}

// --- big-digit font ---------------------------------------------------------

const DIGITS: [[&str; 5]; 10] = [
    ["████", "█  █", "█  █", "█  █", "████"],
    ["  █ ", " ██ ", "  █ ", "  █ ", " ███"],
    ["████", "   █", "████", "█   ", "████"],
    ["████", "   █", " ███", "   █", "████"],
    ["█  █", "█  █", "████", "   █", "   █"],
    ["████", "█   ", "████", "   █", "████"],
    ["████", "█   ", "████", "█  █", "████"],
    ["████", "   █", "  █ ", " █  ", " █  "],
    ["████", "█  █", "████", "█  █", "████"],
    ["████", "█  █", "████", "   █", "████"],
];
const GLYPH_DEG: [&str; 5] = ["██", "██", "  ", "  ", "  "];
const GLYPH_C: [&str; 5] = ["████", "█   ", "█   ", "█   ", "████"];
const GLYPH_F: [&str; 5] = ["████", "█   ", "███ ", "█   ", "█   "];
const GLYPH_DASH: [&str; 5] = ["    ", "    ", "████", "    ", "    "];

fn big_temp_rows(value: Option<i64>, fahrenheit: bool) -> [String; 5] {
    let mut glyphs: Vec<[&str; 5]> = Vec::new();
    match value {
        Some(v) => {
            let digits: Vec<usize> = v
                .max(0)
                .to_string()
                .chars()
                .filter_map(|c| c.to_digit(10).map(|d| d as usize))
                .collect();
            for d in digits {
                glyphs.push(DIGITS[d]);
            }
        }
        None => {
            glyphs.push(GLYPH_DASH);
            glyphs.push(GLYPH_DASH);
        }
    }
    glyphs.push(GLYPH_DEG);
    glyphs.push(if fahrenheit { GLYPH_F } else { GLYPH_C });

    let mut rows: [String; 5] = Default::default();
    for (i, row) in rows.iter_mut().enumerate() {
        let mut s = String::new();
        for (gi, g) in glyphs.iter().enumerate() {
            if gi > 0 {
                s.push(' ');
            }
            s.push_str(g[i]);
        }
        *row = s;
    }
    rows
}

fn render_temp(f: &mut Frame, area: Rect, app: &App) {
    let ac = accent(app);
    let block = panel("Target", ac);

    let mut lines: Vec<Line> = vec![];
    match &app.state {
        None => lines.push(Line::from(Span::styled(
            "loading…",
            Style::default().fg(COL_DIM),
        ))),
        Some(s) => {
            // Pending (debounced, not yet published) target wins for display.
            let pending_c = app.pending_temp();
            let (val, is_f) = if s.temp_is_f {
                let v = pending_c
                    .map(|c| (c as f64 * 9.0 / 5.0 + 32.0).round() as i64)
                    .or(s.target_f);
                (v, true)
            } else {
                (pending_c.or(s.target_temp.map(|t| t.round() as i64)), false)
            };
            let style = if s.power {
                Style::default().fg(ac).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(COL_DIM)
            };
            for row in big_temp_rows(val, is_f) {
                lines.push(Line::from(Span::styled(row, style)));
            }
            lines.push(Line::from(""));

            // trend arrow from the last two history points
            let trend = {
                let h = &app.temp_hist;
                if h.len() >= 2 {
                    let d = h[h.len() - 1] - h[h.len() - 2];
                    if d > 0.05 {
                        "↗"
                    } else if d < -0.05 {
                        "↘"
                    } else {
                        "→"
                    }
                } else {
                    " "
                }
            };
            let now = s
                .current_temp
                .map(|t| format!("{t:.1}°C"))
                .unwrap_or_else(|| "—".into());
            let alt_f = s.target_f.map(|v| format!("= {v}°F")).unwrap_or_default();
            let mut info = vec![
                Span::styled(format!("now {now} {trend}"), Style::default().fg(COL_TEXT)),
                Span::styled(
                    format!("   range {}–{}°C  {alt_f}", s.t_min, s.t_max),
                    Style::default().fg(COL_DIM),
                ),
            ];
            if pending_c.is_some() {
                info.push(Span::styled(
                    "  ⏳",
                    Style::default().fg(ac).add_modifier(Modifier::BOLD),
                ));
            }
            lines.push(Line::from(info));
        }
    }

    f.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .block(block),
        area,
    );
}

fn render_mode(f: &mut Frame, area: Rect, app: &App) {
    let ac = accent(app);
    let block = panel("Mode & Power", ac);

    let mut lines: Vec<Line> = vec![];
    let (power, active_mode, raw_mode) = match &app.state {
        Some(s) => (s.power, s.mode(), s.work_mode),
        None => (false, None, -1),
    };

    let power_chip = if power {
        chip("◉ POWER ON ".into(), true, COL_OK)
    } else {
        chip("○ POWER OFF".into(), true, Color::Rgb(200, 80, 80))
    };
    lines.push(Line::from(vec![
        Span::raw(" "),
        power_chip,
        Span::raw(" "),
        key_hint("p"),
    ]));
    lines.push(Line::from(""));

    let pending = app.pending_mode();
    for (i, m) in Mode::CYCLE.iter().enumerate() {
        let active = active_mode == Some(*m);
        let queued = pending == Some(*m) && !active;
        let marker = if active {
            "▶"
        } else if queued {
            "◌"
        } else {
            " "
        };
        let style = if active {
            Style::default().fg(ac).add_modifier(Modifier::BOLD)
        } else if queued {
            Style::default().fg(ac)
        } else {
            Style::default().fg(COL_DIM)
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {marker} "), style),
            Span::styled(format!("{} {:<5}", m.icon(), m.label()), style),
            key_hint(&(i + 1).to_string()),
        ]));
    }
    if active_mode.is_none() && raw_mode >= 0 {
        lines.push(Line::from(Span::styled(
            format!(" raw workMode={raw_mode}"),
            Style::default().fg(COL_ERR),
        )));
    }

    f.render_widget(Paragraph::new(lines).block(block), area);
}

// --- fan --------------------------------------------------------------------

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

// A real fan, drawn on a Braille sub-cell canvas (2×4 dots per character) so the
// blades are smooth curves inside a circular guard rather than ASCII slashes.
const FAN_CW: usize = 18; // canvas width  in characters → 36 dot columns
const FAN_CH: usize = 9; //  canvas height in characters → 36 dot rows  (square)
const FAN_BLADES: f64 = 3.0;
const FAN_SPIRAL: f64 = 1.25; // blade curvature (angle skew per unit radius)

/// dot-bit for (row 0..4, col 0..2) within a Braille cell (base U+2800).
const BRAILLE_BITS: [[u8; 2]; 4] = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];

/// A Braille pixel canvas. `layer` records what drew each cell (1 guard, 2 blade,
/// 3 hub) so the cell can be colored by the topmost element.
struct FanCanvas {
    bits: [u8; FAN_CW * FAN_CH],
    layer: [u8; FAN_CW * FAN_CH],
}

impl FanCanvas {
    fn new() -> Self {
        FanCanvas {
            bits: [0; FAN_CW * FAN_CH],
            layer: [0; FAN_CW * FAN_CH],
        }
    }
    fn plot(&mut self, px: i32, py: i32, layer: u8) {
        if px < 0 || py < 0 {
            return;
        }
        let (px, py) = (px as usize, py as usize);
        let (cx, cy) = (px / 2, py / 4);
        if cx >= FAN_CW || cy >= FAN_CH {
            return;
        }
        let idx = cy * FAN_CW + cx;
        self.bits[idx] |= BRAILLE_BITS[py % 4][px % 2];
        if layer > self.layer[idx] {
            self.layer[idx] = layer;
        }
    }
}

/// Rasterise the fan for the current frame.
fn draw_fan(app: &App, fan: Fan, power: bool) -> FanCanvas {
    use std::f64::consts::TAU;
    let mut cv = FanCanvas::new();
    let pw = (FAN_CW * 2) as f64;
    let ph = (FAN_CH * 4) as f64;
    let cx = (pw - 1.0) / 2.0;
    let cy = (ph - 1.0) / 2.0;
    let radius = pw / 2.0 - 1.0;

    // rotation: advances with the frame counter (which only ticks when animating)
    let theta = if power && app.anim {
        let per = match fan {
            Fan::Turbo => 0.55,
            Fan::Gear(g) => 0.09 + 0.05 * g as f64,
            Fan::Auto => 0.14,
        };
        app.frame as f64 * per
    } else {
        0.7 // parked angle
    };
    let sector = TAU / FAN_BLADES;

    let mut py = 0;
    while (py as f64) < ph {
        let mut px = 0;
        while (px as f64) < pw {
            let dx = px as f64 - cx;
            let dy = py as f64 - cy;
            let r = (dx * dx + dy * dy).sqrt() / radius;
            if r <= 1.03 {
                if r >= 0.90 {
                    cv.plot(px, py, 1); // circular guard rim
                } else if r <= 0.17 {
                    cv.plot(px, py, 3); // hub
                } else if (0.20..=0.86).contains(&r) {
                    // curved blade: skew angle by radius for a paddle/spiral shape
                    let a = dy.atan2(dx) - theta + FAN_SPIRAL * r;
                    let m = a.rem_euclid(sector);
                    let d = m.min(sector - m); // angular distance to blade centerline
                    let half = 0.14 + 0.42 * r; // blades widen toward the rim
                    if d < half {
                        cv.plot(px, py, 2);
                    }
                }
            }
            px += 1;
        }
        py += 1;
    }
    cv
}

fn fan_lines(cv: &FanCanvas, pad: usize, ac: Color, power: bool) -> Vec<Line<'static>> {
    let blade = if power {
        Style::default().fg(ac).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(COL_TRACK)
    };
    let guard = Style::default().fg(if power { COL_DIM } else { COL_TRACK });
    let hub = Style::default()
        .fg(if power { COL_TEXT } else { COL_FAINT })
        .add_modifier(Modifier::BOLD);

    (0..FAN_CH)
        .map(|cy| {
            let mut spans: Vec<Span> = vec![Span::raw(" ".repeat(pad))];
            for cx in 0..FAN_CW {
                let idx = cy * FAN_CW + cx;
                let bits = cv.bits[idx];
                if bits == 0 {
                    spans.push(Span::raw(" "));
                    continue;
                }
                let ch = char::from_u32(0x2800 + bits as u32).unwrap_or(' ');
                let style = match cv.layer[idx] {
                    3 => hub,
                    2 => blade,
                    _ => guard,
                };
                spans.push(Span::styled(ch.to_string(), style));
            }
            Line::from(spans)
        })
        .collect()
}

fn render_fan(f: &mut Frame, area: Rect, app: &App) {
    let ac = accent(app);
    let block = panel("Fan", ac);

    let mut lines: Vec<Line> = vec![];
    let (fan, pct, power) = match &app.state {
        Some(s) => (s.fan, s.wind_pct, s.power),
        None => (Fan::Auto, None, false),
    };
    let shown_fan = app.pending_fan().unwrap_or(fan);
    let level = shown_fan.gear_level();
    let spinning = power && app.anim;
    let speed: u64 = match shown_fan {
        Fan::Turbo => 1,
        Fan::Gear(g) => (9 - g as u64).max(2),
        Fan::Auto => 4,
    };

    let iw = area.width.saturating_sub(2) as usize;
    let pad = iw.saturating_sub(FAN_CW) / 2;
    lines.extend(fan_lines(&draw_fan(app, shown_fan, power), pad, ac, power));

    // gear bar ▁▂▃▄▅▆█ (2 cells per gear)
    let bars = ["▁", "▂", "▃", "▄", "▅", "▆", "█"];
    let mut bar_spans: Vec<Span> = vec![Span::raw(" ")];
    if shown_fan == Fan::Auto {
        // Auto: an animated accent wave across the whole bar, so it reads as
        // "adaptive / auto" rather than an empty speed-0 bar.
        for i in 0..bars.len() {
            let phase = if spinning {
                app.frame as f64 * 0.6
            } else {
                0.0
            };
            let wave = ((i as f64 * 0.9 + phase).sin() * 0.5 + 0.5) * (bars.len() as f64 - 1.0);
            let ch = bars[(wave.round() as usize).min(bars.len() - 1)];
            bar_spans.push(Span::styled(
                format!("{ch}{ch}"),
                Style::default().fg(ac).add_modifier(Modifier::BOLD),
            ));
        }
    } else {
        for (i, b) in bars.iter().enumerate() {
            let lit = level as usize > i;
            let mut st = if lit {
                Style::default().fg(ac)
            } else {
                Style::default().fg(COL_TRACK)
            };
            // turbo flash
            if shown_fan == Fan::Turbo && spinning && (app.frame / 3).is_multiple_of(2) {
                st = st.add_modifier(Modifier::BOLD);
            }
            bar_spans.push(Span::styled(format!("{b}{b}"), st));
        }
    }
    let spin_char = if spinning {
        SPINNER[((app.frame / speed.max(2)) % SPINNER.len() as u64) as usize]
    } else {
        " "
    };
    let label = if app.pending_fan().is_some() {
        format!("  {} {} ⏳", spin_char, shown_fan.label())
    } else {
        format!("  {} {}", spin_char, shown_fan.label())
    };
    bar_spans.push(Span::styled(
        label,
        Style::default().fg(COL_TEXT).add_modifier(Modifier::BOLD),
    ));
    bar_spans.push(Span::raw("  "));
    bar_spans.push(key_hint("f/F"));
    lines.push(Line::from(bar_spans));

    // wind output percentage bar
    if let Some(p) = pct {
        let w: usize = 14;
        let filled = ((p.clamp(0, 100) as usize) * w) / 100;
        lines.push(Line::from(vec![
            Span::styled(" output ", Style::default().fg(COL_DIM)),
            Span::styled("█".repeat(filled), Style::default().fg(ac)),
            Span::styled("░".repeat(w - filled), Style::default().fg(COL_TRACK)),
            Span::styled(format!(" {p:>3}%"), Style::default().fg(COL_TEXT)),
        ]));
    }

    // badges
    lines.push(Line::from(vec![
        Span::raw(" "),
        chip("AUTO".into(), shown_fan == Fan::Auto, ac),
        Span::raw(" "),
        chip(
            "TURBO".into(),
            shown_fan == Fan::Turbo,
            Color::Rgb(255, 140, 90),
        ),
    ]));

    f.render_widget(Paragraph::new(lines).block(block), area);
}

// --- swing -------------------------------------------------------------------

/// Ping-pong 0..n-1..0 over time t.
fn ping_pong(t: u64, n: u64) -> u64 {
    if n <= 1 {
        return 0;
    }
    let m = 2 * (n - 1);
    let p = t % m;
    if p < n {
        p
    } else {
        m - p
    }
}

/// Sweep position within an inclusive row range.
fn sweep(frame: u64, lo: u64, hi: u64) -> u64 {
    lo + ping_pong(frame / 3, hi - lo + 1)
}

fn render_swing(f: &mut Frame, area: Rect, app: &App) {
    let ac = accent(app);
    let block = panel("Swing", ac);

    let mut lines: Vec<Line> = vec![];
    let (v_dir, h_dir, power) = match &app.state {
        Some(s) => (s.v_dir, s.h_dir, s.power),
        None => (8, 8, false),
    };
    let animate = power && app.anim;

    // Vertical: 5 louver rows; marker sweeps when swinging, parks when fixed.
    // dir: 1 full sweep, 2 upper sweep, 3 lower sweep, 9..13 fixed, 8 none
    let v_active: Option<u64> = match v_dir {
        1 if animate => Some(sweep(app.frame, 0, 4)),
        2 if animate => Some(sweep(app.frame, 0, 2)),
        3 if animate => Some(sweep(app.frame, 2, 4)),
        1..=3 => Some(2), // swinging but animations off → park mid
        9..=13 => Some((v_dir - 9) as u64),
        _ => None,
    };
    let v_label = match v_dir {
        1..=3 => "sweep",
        9..=13 => "fixed",
        _ => "off",
    };
    for row in 0..5u64 {
        let active = v_active == Some(row);
        let (marker, vane, style) = if active {
            (
                "▶",
                "━━━━━━━━",
                Style::default().fg(ac).add_modifier(Modifier::BOLD),
            )
        } else {
            (" ", "────────", Style::default().fg(COL_TRACK))
        };
        let mut spans = vec![Span::styled(format!("  {marker} {vane}"), style)];
        if row == 0 {
            spans.push(Span::styled(
                format!("  V:{v_label}"),
                Style::default().fg(COL_DIM),
            ));
            spans.push(Span::raw(" "));
            spans.push(key_hint("s"));
        }
        lines.push(Line::from(spans));
    }

    lines.push(Line::from(""));

    // Horizontal: 5 cells, sweep or fixed. dir: 1 full, 2 left, 3 mid, 4 right
    let h_active: Option<u64> = match h_dir {
        1 if animate => Some(sweep(app.frame, 0, 4)),
        2 if animate => Some(sweep(app.frame, 0, 2)),
        3 if animate => Some(sweep(app.frame, 1, 3)),
        4 if animate => Some(sweep(app.frame, 2, 4)),
        1..=4 => Some(2),
        9..=13 => Some((h_dir - 9) as u64),
        _ => None,
    };
    let h_label = match h_dir {
        1..=4 => "sweep",
        9..=13 => "fixed",
        _ => "off",
    };
    let mut h_spans: Vec<Span> = vec![Span::raw("    ")];
    for cell in 0..5u64 {
        if h_active == Some(cell) {
            h_spans.push(Span::styled("██", Style::default().fg(ac)));
        } else {
            h_spans.push(Span::styled("░░", Style::default().fg(COL_TRACK)));
        }
    }
    h_spans.push(Span::styled(
        format!("  H:{h_label}"),
        Style::default().fg(COL_DIM),
    ));
    h_spans.push(Span::raw(" "));
    h_spans.push(key_hint("h"));
    h_spans.push(Span::styled("  aim", Style::default().fg(COL_DIM)));
    h_spans.push(Span::raw(" "));
    h_spans.push(key_hint("v"));
    lines.push(Line::from(h_spans));

    f.render_widget(Paragraph::new(lines).block(block), area);
}

// ---------------------------------------------------------------------------
// toggles
// ---------------------------------------------------------------------------

fn render_toggles(f: &mut Frame, area: Rect, app: &App) {
    let ac = accent(app);
    let block = panel("Comfort & Extras", ac);

    let s = app.state.as_ref();
    let get = |f: fn(&AcState) -> bool| s.map(f).unwrap_or(false);

    let sleep = s.map(|x| x.sleep).unwrap_or(0);
    let sleep_on = sleep > 0;
    let sleep_txt = format!("SLEEP·{}", SLEEP_LABELS[(sleep as usize).min(3)]);

    let row1: Vec<(String, bool, &str)> = vec![
        ("ECO".into(), get(|x| x.eco), "e"),
        (sleep_txt, sleep_on, "z"),
        ("HEALTH".into(), get(|x| x.healthy), "g"),
        ("SELF-CLEAN".into(), get(|x| x.self_clean), "c"),
    ];
    let row2: Vec<(String, bool, &str)> = vec![
        ("ANTI-MOLD".into(), get(|x| x.anti_mold), "n"),
        ("8°C-HEAT".into(), get(|x| x.eight_heat), "8"),
        ("SCREEN".into(), get(|x| x.screen), "d"),
        ("BEEP".into(), get(|x| x.beep), "b"),
    ];

    let mk = |items: Vec<(String, bool, &str)>, extra: Option<Span<'static>>| -> Line<'static> {
        let mut spans: Vec<Span> = vec![Span::raw(" ")];
        for (label, on, key) in items {
            spans.push(chip(label, on, COL_OK));
            spans.push(key_hint(key));
            spans.push(Span::raw("  "));
        }
        if let Some(e) = extra {
            spans.push(e);
        }
        Line::from(spans)
    };

    // error status chip on row 2
    let err_span = match s {
        Some(st) if !st.errors.is_empty() => Span::styled(
            format!(" ⚠ errors {:?} ", st.errors),
            Style::default()
                .fg(Color::Rgb(15, 18, 24))
                .bg(COL_ERR)
                .add_modifier(Modifier::BOLD),
        ),
        Some(_) => Span::styled(" ✔ no errors ", Style::default().fg(COL_OK)),
        None => Span::raw(""),
    };

    let lines = vec![mk(row1, None), mk(row2, Some(err_span))];
    f.render_widget(Paragraph::new(lines).block(block), area);
}

// ---------------------------------------------------------------------------
// sensors + sparkline
// ---------------------------------------------------------------------------

fn render_sensors(f: &mut Frame, area: Rect, app: &App) {
    let ac = accent(app);
    let cols = Layout::horizontal([
        Constraint::Length(26),
        Constraint::Length(34),
        Constraint::Min(16),
    ])
    .split(area);

    let t = |v: Option<f64>| v.map(|x| format!("{x:.1}°C")).unwrap_or_else(|| "—".into());
    let n = |v: Option<i64>| v.map(|x| x.to_string()).unwrap_or_else(|| "—".into());

    // --- column 1: temperatures + estimated power ---
    let mut temps: Vec<Line> = vec![];
    if let Some(s) = &app.state {
        temps.push(sensor_line("Indoor", t(s.current_temp), COL_TEXT));
        temps.push(sensor_line("Outdoor", t(s.external_temp), COL_TEXT));
        temps.push(sensor_line("Coil", t(s.coil_temp), COL_DIM));
        let est = s.est_power_w();
        temps.push(Line::from(vec![
            Span::styled(" Power ~    ", Style::default().fg(COL_DIM)),
            Span::styled(
                est.map(device::fmt_power).unwrap_or_else(|| "—".into()),
                Style::default().fg(ac).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" est", Style::default().fg(COL_TRACK)),
        ]));
    } else {
        temps.push(Line::from(Span::styled(
            " loading…",
            Style::default().fg(COL_DIM),
        )));
    }
    f.render_widget(Paragraph::new(temps).block(panel("Temps", ac)), cols[0]);

    // --- column 2: outdoor unit + maintenance ---
    let mut ou: Vec<Line> = vec![];
    if let Some(s) = &app.state {
        let hz = s.compressor_hz.unwrap_or(0);
        let w: usize = 8;
        let filled = (((hz as f64 / 120.0).clamp(0.0, 1.0)) * w as f64) as usize;
        ou.push(Line::from(vec![
            Span::styled(" Comp ", Style::default().fg(COL_DIM)),
            Span::styled("█".repeat(filled), Style::default().fg(ac)),
            Span::styled("░".repeat(w - filled), Style::default().fg(COL_TRACK)),
            Span::styled(
                format!(
                    " {hz}Hz (run {} / tgt {})",
                    n(s.comp_run_hz),
                    n(s.comp_target_hz)
                ),
                Style::default().fg(COL_TEXT),
            ),
        ]));
        ou.push(kv2("Outdoor fan", n(s.outdoor_fan), "EEV", n(s.eev)));
        ou.push(kv2(
            "PTC heat",
            if s.ptc { "on".into() } else { "off".into() },
            "Wind",
            s.wind_pct
                .map(|p| format!("{p}%"))
                .unwrap_or_else(|| "—".into()),
        ));
        let (filt, filt_c) = if s.filter_alert {
            ("CLEAN NEEDED".to_string(), COL_ERR)
        } else {
            ("ok".to_string(), COL_OK)
        };
        ou.push(Line::from(vec![
            Span::styled(" Filter      ", Style::default().fg(COL_DIM)),
            Span::styled(
                filt,
                Style::default().fg(filt_c).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("   Self-clean {}%", n(s.self_clean_pct)),
                Style::default().fg(COL_DIM),
            ),
        ]));
    } else {
        ou.push(Line::from(Span::styled(" …", Style::default().fg(COL_DIM))));
    }
    f.render_widget(Paragraph::new(ou).block(panel("Outdoor unit", ac)), cols[1]);

    // --- column 3: indoor-temp sparkline ---
    let hist = &app.temp_hist;
    let title = match (
        hist.iter().cloned().reduce(f64::min),
        hist.iter().cloned().reduce(f64::max),
    ) {
        (Some(lo), Some(hi)) if hist.len() > 1 => format!("Trend {lo:.1}–{hi:.1}°"),
        _ => "Trend".to_string(),
    };
    let inner_w = cols[2].width.saturating_sub(2) as usize;
    let take = hist.len().saturating_sub(inner_w.max(1));
    let lo = hist.iter().cloned().reduce(f64::min).unwrap_or(0.0);
    let data: Vec<u64> = hist[take..]
        .iter()
        .map(|t| (((t - lo) * 10.0).round() as u64) + 1)
        .collect();
    let spark = Sparkline::default()
        .block(panel(&title, ac))
        .style(Style::default().fg(ac))
        .data(&data);
    f.render_widget(spark, cols[2]);
}

fn sensor_line(label: &str, value: String, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!(" {label:<10} "), Style::default().fg(COL_DIM)),
        Span::styled(
            value,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ])
}

/// Two label:value pairs on one line.
fn kv2(l1: &str, v1: String, l2: &str, v2: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!(" {l1:<11} "), Style::default().fg(COL_DIM)),
        Span::styled(format!("{v1:<7}"), Style::default().fg(COL_TEXT)),
        Span::styled(format!("{l2} "), Style::default().fg(COL_DIM)),
        Span::styled(v2, Style::default().fg(COL_TEXT)),
    ])
}

// ---------------------------------------------------------------------------
// log + keys
// ---------------------------------------------------------------------------

fn render_bottom(f: &mut Frame, area: Rect, app: &App) {
    let ac = accent(app);
    let cols = Layout::horizontal([Constraint::Min(40), Constraint::Length(42)]).split(area);

    // activity log
    let block = panel("Activity", ac);
    let inner_h = cols[0].height.saturating_sub(2) as usize;
    let start = app.log.len().saturating_sub(inner_h);
    let lines: Vec<Line> = app.log[start..]
        .iter()
        .map(|l| {
            let color = if l.contains(" ✗") {
                COL_ERR
            } else if l.contains(" →") {
                COL_OK
            } else if l.contains(" ◆") {
                Color::Rgb(235, 200, 105)
            } else {
                COL_DIM
            };
            Line::from(Span::styled(format!(" {l}"), Style::default().fg(color)))
        })
        .collect();
    f.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
        cols[0],
    );

    // key reference
    let keys_block = panel("Keys", ac);
    let key_rows = [
        ("p", "power", "m 1-5", "mode"),
        ("+/-", "temp", "f/F", "fan ±"),
        ("s", "v-swing", "h", "h-swing"),
        ("v", "aim vents", "e", "eco"),
        ("z", "sleep", "d", "screen"),
        ("b", "beep", "g", "health"),
        ("c", "self-clean", "n", "anti-mold"),
        ("8", "8°C-heat", "a", "animations"),
        ("r", "refresh", "q", "quit"),
    ];
    let key_lines: Vec<Line> = key_rows
        .iter()
        .map(|(k1, v1, k2, v2)| {
            Line::from(vec![
                Span::styled(format!(" {k1:>5} "), Style::default().fg(ac)),
                Span::styled(format!("{v1:<11}"), Style::default().fg(COL_DIM)),
                Span::styled(format!("{k2:>5} "), Style::default().fg(ac)),
                Span::styled((*v2).to_string(), Style::default().fg(COL_DIM)),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(key_lines).block(keys_block), cols[1]);
}
