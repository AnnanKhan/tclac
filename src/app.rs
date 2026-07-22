//! TUI application state and event loop.
//!
//! All cloud I/O (shadow reads, command publishes, re-auth) runs in background
//! tasks and reports back over an mpsc channel, so keypresses and animations are
//! never blocked by a network round-trip.
//!
//! Anti-throttling:
//!  - Rapid temp/fan/mode keypresses are **debounced** (450 ms): the UI shows the
//!    pending value immediately, but only one command is published.
//!  - Shadow reads never overlap (in-flight guard + coalescing).
//!  - A `Throttling` error triggers a 15 s cool-down, not a re-login. Re-auth is
//!    reserved for auth-shaped errors and rate-limited to once per 45 s.

use anyhow::Result;
use chrono::Local;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use serde_json::Value;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::auth::{self, Session};
use crate::config::Config;
use crate::device::{self, AcState, Fan, FanDialect, Mode, SLEEP_LABELS};
use crate::iot::ShadowClient;
use crate::rest::Device;
use crate::ui;

const DEBOUNCE: Duration = Duration::from_millis(450);
const THROTTLE_BACKOFF: Duration = Duration::from_secs(15);
const REAUTH_COOLDOWN: Duration = Duration::from_secs(45);

/// Result of a background cloud task.
enum Msg {
    Shadow(Result<Value>),
    Cmd {
        desc: String,
        payload: String,
        result: Result<()>,
    },
}

/// A debounced, not-yet-sent user intent.
#[derive(Debug, Clone, Copy)]
pub enum PendingKind {
    Temp(i64),
    Fan(Fan),
    Mode(Mode),
}

struct PendingCmd {
    kind: PendingKind,
    due: Instant,
}

pub struct App {
    cfg: Config,
    client: reqwest::Client,
    session: Session,
    shadow: ShadowClient,
    pub device: Device,

    msg_tx: UnboundedSender<Msg>,
    msg_rx: Option<UnboundedReceiver<Msg>>,

    pub state: Option<AcState>,
    pub status: String,
    pub last_refresh: Option<Instant>,
    pub busy: bool,
    pub log: Vec<String>,
    pub should_quit: bool,

    pending: Option<PendingCmd>,
    refresh_inflight: bool,
    refresh_queued: bool,
    backoff_until: Option<Instant>,
    last_reauth: Option<Instant>,

    pub anim: bool,
    pub frame: u64,
    pub temp_hist: Vec<f64>,

    /// Aim-airflow overlay: open flag + crosshair grid position (0..=4 each).
    pub aim_open: bool,
    pub aim_h: usize,
    pub aim_v: usize,
}

impl App {
    pub fn new(cfg: Config, client: reqwest::Client, session: Session, device: Device) -> Self {
        let shadow = ShadowClient::new(&session, &device.device_id);
        let (msg_tx, msg_rx) = mpsc::unbounded_channel();
        App {
            cfg,
            client,
            session,
            shadow,
            device,
            msg_tx,
            msg_rx: Some(msg_rx),
            state: None,
            status: "Starting…".to_string(),
            last_refresh: None,
            busy: false,
            log: Vec::new(),
            should_quit: false,
            pending: None,
            refresh_inflight: false,
            refresh_queued: false,
            backoff_until: None,
            last_reauth: None,
            anim: true,
            frame: 0,
            temp_hist: Vec::new(),
            aim_open: false,
            aim_h: 2,
            aim_v: 2,
        }
    }

    pub fn refresh_age(&self) -> String {
        match self.last_refresh {
            Some(t) => format!("{}s ago", t.elapsed().as_secs()),
            None => "never".to_string(),
        }
    }

    pub fn backoff_active(&self) -> bool {
        self.backoff_until
            .map(|t| Instant::now() < t)
            .unwrap_or(false)
    }

    // Pending accessors for the UI (instant feedback before the publish).
    pub fn pending_temp(&self) -> Option<i64> {
        match self.pending.as_ref()?.kind {
            PendingKind::Temp(v) => Some(v),
            _ => None,
        }
    }
    pub fn pending_fan(&self) -> Option<Fan> {
        match self.pending.as_ref()?.kind {
            PendingKind::Fan(f) => Some(f),
            _ => None,
        }
    }
    pub fn pending_mode(&self) -> Option<Mode> {
        match self.pending.as_ref()?.kind {
            PendingKind::Mode(m) => Some(m),
            _ => None,
        }
    }

    fn push_log(&mut self, line: String) {
        let line = truncate(&line, 220);
        let stamped = format!("{} {line}", Local::now().format("%H:%M:%S"));
        self.log.push(stamped);
        let len = self.log.len();
        if len > 200 {
            self.log.drain(0..len - 200);
        }
    }

    // ---- background task spawners --------------------------------------

    /// Kick off a shadow read (optionally after a settle delay). Reads never
    /// overlap: a second request while one is in flight is coalesced.
    fn spawn_refresh(&mut self, delay_ms: u64) {
        if self.refresh_inflight {
            self.refresh_queued = true;
            return;
        }
        self.refresh_inflight = true;
        let sc = self.shadow.clone();
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            if delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
            let _ = tx.send(Msg::Shadow(sc.get_shadow().await));
        });
    }

    /// Kick off a command publish.
    fn spawn_cmd(&mut self, desc: String, desired: Value) {
        self.busy = true;
        self.status = format!("Sending: {desc}");
        let payload = desired.to_string();
        let sc = self.shadow.clone();
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            let result = sc.set_desired(desired).await;
            let _ = tx.send(Msg::Cmd {
                desc,
                payload,
                result,
            });
        });
    }

    // ---- debounced commands ---------------------------------------------

    fn set_pending(&mut self, kind: PendingKind) {
        // A different kind already queued? Send it right away so it isn't lost.
        if let Some(prev) = self.pending.take() {
            if std::mem::discriminant(&prev.kind) != std::mem::discriminant(&kind) {
                self.dispatch(prev.kind);
            }
        }
        self.pending = Some(PendingCmd {
            kind,
            due: Instant::now() + DEBOUNCE,
        });
    }

    /// Called on every UI tick: publish the pending intent once it settles.
    pub fn flush_pending(&mut self) {
        let due = self
            .pending
            .as_ref()
            .map(|p| p.due <= Instant::now())
            .unwrap_or(false);
        if due {
            if let Some(p) = self.pending.take() {
                self.dispatch(p.kind);
            }
        }
    }

    fn dispatch(&mut self, kind: PendingKind) {
        match kind {
            PendingKind::Temp(c) => {
                let (t_min, t_max) = self
                    .state
                    .as_ref()
                    .map(|s| (s.t_min, s.t_max))
                    .unwrap_or((device::FALLBACK_TEMP_MIN, device::FALLBACK_TEMP_MAX));
                self.spawn_cmd(format!("temp {c}°C"), device::cmd_temp(c, t_min, t_max));
            }
            PendingKind::Fan(f) => {
                let dialect = self
                    .state
                    .as_ref()
                    .map(|s| s.dialect)
                    .unwrap_or(FanDialect::SevenGear);
                self.spawn_cmd(format!("fan {}", f.label()), f.desired(dialect));
            }
            PendingKind::Mode(m) => {
                self.spawn_cmd(format!("mode {}", m.label()), device::cmd_mode(m));
            }
        }
    }

    // ---- message handling ----------------------------------------------

    async fn handle_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Shadow(result) => {
                self.refresh_inflight = false;
                match result {
                    Ok(shadow) => {
                        self.apply_shadow(shadow);
                        if self.refresh_queued {
                            self.refresh_queued = false;
                            self.spawn_refresh(300);
                        }
                    }
                    Err(e) => {
                        self.refresh_queued = false;
                        self.status = format!("Refresh error: {e}");
                        self.push_log(format!("✗ refresh: {e}"));
                        self.handle_cloud_error(&e.to_string()).await;
                    }
                }
            }
            Msg::Cmd {
                desc,
                payload,
                result,
            } => {
                self.busy = false;
                match result {
                    Ok(()) => {
                        self.backoff_until = None;
                        self.push_log(format!("→ {desc}  {payload}"));
                        self.status = format!("Sent: {desc}");
                        // read back after the device settles
                        self.spawn_refresh(800);
                    }
                    Err(e) => {
                        self.status = format!("Error: {e}");
                        self.push_log(format!("✗ {desc}: {e}"));
                        self.handle_cloud_error(&e.to_string()).await;
                    }
                }
            }
        }
    }

    /// Classify a cloud failure: throttling → back off; auth-shaped → re-login.
    async fn handle_cloud_error(&mut self, msg: &str) {
        let lower = msg.to_lowercase();
        if lower.contains("throttl") || lower.contains("too many") || lower.contains("rate") {
            self.backoff_until = Some(Instant::now() + THROTTLE_BACKOFF);
            self.status = "Cloud rate-limit — cooling down 15s".to_string();
            self.push_log("· backing off 15s (rate-limited)".to_string());
        } else if lower.contains("forbidden")
            || lower.contains("unauthorized")
            || lower.contains("expired")
            || lower.contains("security token")
            || lower.contains("credential")
            || lower.contains("403")
        {
            self.maybe_reauth().await;
        }
    }

    fn apply_shadow(&mut self, shadow: Value) {
        let mut st = AcState::from_shadow(&shadow);
        st.online = self.device.is_online;

        // Verbose diff log: announce externally-observed changes (app, remote,
        // another tclac) — not just our own commands.
        if let Some(old) = &self.state {
            let mut diffs: Vec<String> = Vec::new();
            if old.power != st.power {
                diffs.push(format!("power {}→{}", onoff(old.power), onoff(st.power)));
            }
            if old.work_mode != st.work_mode {
                diffs.push(format!("mode {}→{}", old.mode_label(), st.mode_label()));
            }
            if old.target_temp != st.target_temp {
                diffs.push(format!(
                    "target {}→{}",
                    fmt_temp(old.target_temp),
                    fmt_temp(st.target_temp)
                ));
            }
            if old.fan != st.fan {
                diffs.push(format!("fan {}→{}", old.fan.label(), st.fan.label()));
            }
            if old.v_dir != st.v_dir {
                diffs.push(format!("v-swing {}→{}", old.v_dir, st.v_dir));
            }
            if old.h_dir != st.h_dir {
                diffs.push(format!("h-swing {}→{}", old.h_dir, st.h_dir));
            }
            if !diffs.is_empty() {
                self.push_log(format!("◆ state: {}", diffs.join(", ")));
            }
        }

        if let Some(t) = st.current_temp {
            self.temp_hist.push(t);
            let len = self.temp_hist.len();
            if len > 600 {
                self.temp_hist.drain(0..len - 600);
            }
        }

        self.state = Some(st);
        self.last_refresh = Some(Instant::now());
        if self.status.is_empty()
            || self.status.contains("error")
            || self.status.starts_with("Refreshing")
            || self.status.starts_with("Connecting")
            || self.status == "Starting…"
        {
            self.status = "Connected".to_string();
        }
    }

    /// Re-run the auth chain (rare: only on credential expiry) with a cooldown so
    /// a persistent permission error can't trigger a re-login storm.
    async fn maybe_reauth(&mut self) {
        if let Some(t) = self.last_reauth {
            if t.elapsed() < REAUTH_COOLDOWN {
                return;
            }
        }
        self.last_reauth = Some(Instant::now());
        match auth::login(&self.client, &self.cfg.username, &self.cfg.password).await {
            Ok(sess) => {
                self.session = sess;
                self.shadow = ShadowClient::new(&self.session, &self.device.device_id);
                self.push_log("↻ re-authenticated".to_string());
                self.spawn_refresh(0);
            }
            Err(e) => {
                self.push_log(format!("✗ re-auth: {e}"));
            }
        }
    }

    // ---- key handling (all non-blocking) --------------------------------

    fn handle_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if self.aim_open {
            return self.handle_aim_key(key, ctrl);
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('c') if ctrl => self.should_quit = true,

            KeyCode::Char('v') => self.open_aim(),
            KeyCode::Char('p') => self.toggle_power(),
            KeyCode::Char('+') | KeyCode::Char('=') | KeyCode::Up => self.adjust_temp(1),
            KeyCode::Char('-') | KeyCode::Char('_') | KeyCode::Down => self.adjust_temp(-1),

            KeyCode::Char('m') => self.cycle_mode(),
            KeyCode::Char(c @ '1'..='5') => {
                let mode = Mode::CYCLE[(c as usize) - ('1' as usize)];
                self.status = format!("queued: mode {}", mode.label());
                self.set_pending(PendingKind::Mode(mode));
            }

            KeyCode::Char('f') => self.step_fan(true),
            KeyCode::Char('F') => self.step_fan(false),

            KeyCode::Char('s') => self.toggle_vswing(),
            KeyCode::Char('h') => self.toggle_hswing(),

            KeyCode::Char('e') => self.toggle_flag("ECO", "eco", |s| s.eco),
            KeyCode::Char('z') => self.cycle_sleep(),
            KeyCode::Char('d') => self.toggle_flag("screen", "display", |s| s.screen),
            KeyCode::Char('b') => self.toggle_flag("beepSwitch", "beep", |s| s.beep),
            KeyCode::Char('g') => self.toggle_flag("healthy", "health", |s| s.healthy),
            KeyCode::Char('c') => self.toggle_flag("selfClean", "self-clean", |s| s.self_clean),
            KeyCode::Char('n') => self.toggle_flag("antiMoldew", "anti-mold", |s| s.anti_mold),
            KeyCode::Char('8') => self.toggle_flag("eightAddHot", "8°C-heat", |s| s.eight_heat),

            KeyCode::Char('a') => {
                self.anim = !self.anim;
                let msg = if self.anim {
                    "animations on"
                } else {
                    "animations off"
                };
                self.push_log(format!("· {msg}"));
                self.status = msg.to_string();
            }
            KeyCode::Char('r') => {
                self.status = "Refreshing…".to_string();
                self.spawn_refresh(0);
            }
            _ => {}
        }
    }

    /// Open the aim overlay, seeding the crosshair from the current fixed
    /// vane position (or center if swinging / unset).
    fn open_aim(&mut self) {
        if let Some(s) = &self.state {
            self.aim_h = s.h_fix().unwrap_or(2);
            self.aim_v = s.v_fix().unwrap_or(2);
        }
        self.aim_open = true;
        self.status = "aim: pick a point, Enter to apply".to_string();
    }

    fn handle_aim_key(&mut self, key: KeyEvent, ctrl: bool) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('v') => {
                self.aim_open = false;
                self.status = "aim cancelled".to_string();
            }
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('c') if ctrl => self.should_quit = true,
            KeyCode::Left | KeyCode::Char('a') => self.aim_h = self.aim_h.saturating_sub(1),
            KeyCode::Right | KeyCode::Char('d') => self.aim_h = (self.aim_h + 1).min(4),
            KeyCode::Up | KeyCode::Char('w') => self.aim_v = self.aim_v.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('s') => self.aim_v = (self.aim_v + 1).min(4),
            KeyCode::Enter | KeyCode::Char(' ') => self.apply_aim(),
            _ => {}
        }
    }

    /// Send the crosshair position as fixed vane directions.
    fn apply_aim(&mut self) {
        let (h, v) = (self.aim_h, self.aim_v);
        let was_swinging = self
            .state
            .as_ref()
            .map(|s| s.v_swinging() || s.h_swinging())
            .unwrap_or(false);
        let mut desc = format!(
            "aim {} · {}",
            device::VENT_V_LABELS[v],
            device::VENT_H_LABELS[h]
        );
        if was_swinging {
            desc.push_str(" (stops swing)");
        }
        self.spawn_cmd(desc, device::cmd_vent(h as i64, v as i64));
        self.aim_open = false;
    }

    /// Mouse click at terminal cell (x, y): inside the aim grid → aim there
    /// and apply; anywhere else while the overlay is open → close it.
    pub fn handle_click(&mut self, x: u16, y: u16, term: ratatui::layout::Rect) {
        if !self.aim_open {
            return;
        }
        match crate::ui::aim_cell_at(term, x, y) {
            Some((h, v)) => {
                self.aim_h = h;
                self.aim_v = v;
                self.apply_aim();
            }
            None => {
                self.aim_open = false;
                self.status = "aim cancelled".to_string();
            }
        }
    }

    fn toggle_power(&mut self) {
        let on = self.state.as_ref().map(|s| !s.power).unwrap_or(true);
        let desc = if on { "power on" } else { "power off" };
        self.spawn_cmd(desc.to_string(), device::cmd_power(on));
    }

    fn adjust_temp(&mut self, delta: i64) {
        let (cur, t_min, t_max) = match self.state.as_ref() {
            Some(s) => (
                s.target_temp.unwrap_or(24.0).round() as i64,
                s.t_min,
                s.t_max,
            ),
            None => (24, device::FALLBACK_TEMP_MIN, device::FALLBACK_TEMP_MAX),
        };
        let base = self.pending_temp().unwrap_or(cur);
        let next = (base + delta).clamp(t_min, t_max);
        self.status = format!("queued: temp {next}°C");
        self.set_pending(PendingKind::Temp(next));
    }

    fn cycle_mode(&mut self) {
        let cur = self
            .pending_mode()
            .or_else(|| self.state.as_ref().and_then(|s| s.mode()))
            .unwrap_or(Mode::Cool);
        let next = cur.next();
        self.status = format!("queued: mode {}", next.label());
        self.set_pending(PendingKind::Mode(next));
    }

    fn step_fan(&mut self, forward: bool) {
        let cur = self
            .pending_fan()
            .or_else(|| self.state.as_ref().map(|s| s.fan))
            .unwrap_or(Fan::Auto);
        let next = if forward { cur.next() } else { cur.prev() };
        self.status = format!("queued: fan {}", next.label());
        self.set_pending(PendingKind::Fan(next));
    }

    fn toggle_vswing(&mut self) {
        let on = self.state.as_ref().map(|s| !s.v_swinging()).unwrap_or(true);
        let desc = if on { "v-swing on" } else { "v-swing off" };
        self.spawn_cmd(desc.to_string(), device::cmd_vswing(on));
    }

    fn toggle_hswing(&mut self) {
        let on = self.state.as_ref().map(|s| !s.h_swinging()).unwrap_or(true);
        let desc = if on { "h-swing on" } else { "h-swing off" };
        self.spawn_cmd(desc.to_string(), device::cmd_hswing(on));
    }

    fn toggle_flag(&mut self, field: &'static str, label: &str, get: fn(&AcState) -> bool) {
        let on = self.state.as_ref().map(|s| !get(s)).unwrap_or(true);
        let desc = format!("{label} {}", onoff(on));
        self.spawn_cmd(desc, device::cmd_flag(field, on));
    }

    fn cycle_sleep(&mut self) {
        let cur = self.state.as_ref().map(|s| s.sleep).unwrap_or(0);
        let next = (cur + 1) % 4;
        self.spawn_cmd(
            format!("sleep {}", SLEEP_LABELS[next as usize]),
            device::cmd_sleep(next),
        );
    }
}

fn onoff(b: bool) -> &'static str {
    if b {
        "ON"
    } else {
        "OFF"
    }
}

fn fmt_temp(t: Option<f64>) -> String {
    t.map(|v| format!("{v:.0}°")).unwrap_or_else(|| "—".into())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n).collect();
        out.push('…');
        out
    }
}

/// Set up the terminal, run the event loop, and restore the terminal afterwards.
pub async fn run(mut app: App) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = event_loop(&mut terminal, &mut app).await;

    disable_raw_mode().ok();
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )
    .ok();
    terminal.show_cursor().ok();
    res
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    // Read terminal events on a dedicated thread; forward over a channel.
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<Event>();
    std::thread::spawn(move || {
        while let Ok(ev) = crossterm::event::read() {
            if ev_tx.send(ev).is_err() {
                break;
            }
        }
    });

    let mut msg_rx = app.msg_rx.take().expect("msg_rx present");

    app.status = "Connecting…".to_string();
    app.spawn_refresh(0);

    let mut refresh_tick = tokio::time::interval(Duration::from_secs(10));
    refresh_tick.tick().await; // consume the immediate first tick
    let mut ui_tick = tokio::time::interval(Duration::from_millis(120));

    loop {
        terminal.draw(|f| ui::render(f, app))?;

        tokio::select! {
            Some(ev) = ev_rx.recv() => {
                match ev {
                    Event::Key(key) if key.kind == KeyEventKind::Press => app.handle_key(key),
                    Event::Mouse(m)
                        if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) =>
                    {
                        let term = terminal.size().map(|s| ratatui::layout::Rect::new(0, 0, s.width, s.height)).unwrap_or_default();
                        app.handle_click(m.column, m.row, term);
                    }
                    _ => {}
                }
            }
            Some(msg) = msg_rx.recv() => app.handle_msg(msg).await,
            _ = refresh_tick.tick() => {
                if !app.busy && !app.refresh_inflight && !app.backoff_active() {
                    app.spawn_refresh(0);
                }
            }
            _ = ui_tick.tick() => {
                if app.anim {
                    app.frame = app.frame.wrapping_add(1);
                }
                app.flush_pending();
            }
        }

        if app.should_quit {
            break;
        }
    }
    Ok(())
}
