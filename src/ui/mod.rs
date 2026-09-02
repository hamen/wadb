// SPDX-License-Identifier: Apache-2.0

//! The terminal UI.

pub mod devices;
pub mod pair;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

use crate::adb::{Device, SmartSocket};
use crate::qr::Matrix;
use crate::service::{self, PortOwner};

/// The unit's state right now, re-read after an action changes it.
pub fn current_unit_state() -> UnitState {
    if crate::service::installed_unit().is_none() {
        UnitState::NotInstalled
    } else if crate::service::is_active() {
        UnitState::Active
    } else {
        UnitState::Inactive
    }
}

/// What the supervising unit is doing. `NotInstalled` is a real state, not an error:
/// before the first `wadb install` there is nothing to report on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnitState {
    NotInstalled,
    Active,
    Inactive,
}

pub struct App {
    pub port: u16,
    pub adb: Option<PathBuf>,
    pub unit: UnitState,
    pub owner: PortOwner,
    pub devices: Vec<Device>,
    pub server_up: bool,
    pub pairing: Option<Pairing>,
    pub message: String,
    pub tick: u64,
    pub should_quit: bool,
    pair_rx: Option<std::sync::mpsc::Receiver<crate::pairing::PairEvent>>,
    pair_cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    pub logs: Vec<String>,
}

pub struct Pairing {
    pub qr: Matrix,
    pub phase: pair::Phase,
    pub started: Instant,
    pub timeout: u64,
}

impl App {
    pub fn new(port: u16, adb: Option<PathBuf>, unit: UnitState) -> Self {
        Self {
            port,
            adb,
            unit,
            owner: PortOwner::Nobody,
            devices: Vec::new(),
            server_up: false,
            pairing: None,
            message: String::new(),
            tick: 0,
            should_quit: false,
            pair_rx: None,
            pair_cancel: None,
            logs: Vec::new(),
        }
    }

    /// Show a QR and hand the credential to a worker thread. The payload moves into the
    /// worker, so the password never lives in UI state that gets rendered or logged.
    pub fn start_pairing(&mut self, timeout_secs: u64) {
        if self.unit == UnitState::NotInstalled {
            self.message = "install the service first (press i)".into();
            return;
        }
        let Some(adb) = self.adb.clone() else {
            self.message = "no adb binary found".into();
            return;
        };
        if !self.server_up {
            self.message = "adb server is down; run `wadb install` first".into();
            return;
        }
        let payload = match crate::pairing::Payload::random() {
            Ok(p) => p,
            Err(e) => {
                self.message = format!("could not generate a pairing code: {e}");
                return;
            }
        };
        let qr = match crate::qr::encode(payload.qr_text().as_bytes()) {
            Ok(m) => m,
            Err(e) => {
                self.message = format!("could not render the QR: {e}");
                return;
            }
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let port = self.port;
        std::thread::spawn(move || {
            crate::pairing::run_pairing(
                adb,
                port,
                payload,
                Duration::from_secs(timeout_secs),
                worker_cancel,
                tx,
            );
        });
        self.pair_rx = Some(rx);
        self.pair_cancel = Some(cancel);
        self.message.clear();
        self.pairing = Some(Pairing {
            qr,
            phase: pair::Phase::Waiting,
            started: Instant::now(),
            timeout: timeout_secs,
        });
    }

    /// Stop a pairing in flight. Signals the worker rather than only dropping the
    /// channel, or it would keep browsing and could still pair after a cancel.
    pub fn cancel_pairing(&mut self) {
        if let Some(flag) = self.pair_cancel.take() {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.pairing = None;
        self.pair_rx = None;
    }

    /// Both units' journals, so the pane shows history from before the TUI started.
    ///
    /// Reading only the server unit meant the pane was entirely adb's internal C++ logging, while
    /// the lines a user actually wants — the watcher reporting a reconnect — live in the other
    /// unit and never appeared at all.
    pub fn refresh_logs(&mut self) {
        // Queried per unit rather than as one merged stream. adb's volume is such that a merged
        // `-n` window is entirely its own chatter, and a `--since` window wide enough to reach
        // past it took nearly two seconds — unusable in a render loop. Two narrow queries take
        // about nine milliseconds each.
        let mut lines: Vec<(String, String)> = Vec::new();
        for (unit, count) in [
            (crate::service::CONNECT_UNIT_NAME, "40"),
            (crate::service::UNIT_NAME, "200"),
        ] {
            let out = std::process::Command::new("journalctl")
                .args([
                    "--user",
                    "-u",
                    unit,
                    "-n",
                    count,
                    "--no-pager",
                    "-o",
                    "short-iso",
                ])
                .output();
            if let Ok(out) = out {
                lines.extend(
                    String::from_utf8_lossy(&out.stdout)
                        .lines()
                        .filter_map(parse_journal_line)
                        .filter(|(_, message)| worth_showing(message)),
                );
            }
        }
        // Same timezone from one journal, so the timestamps sort lexicographically.
        lines.sort_by(|a, b| a.0.cmp(&b.0));
        self.logs = lines
            .into_iter()
            .map(|(timestamp, message)| {
                let clock = timestamp.split('T').nth(1).unwrap_or(&timestamp);
                format!(
                    "{}  {}",
                    &clock[..clock.len().min(8)],
                    shorten_log(&message)
                )
            })
            .collect();
    }

    /// Drain whatever the pairing worker has reported.
    pub fn poll_pairing(&mut self) {
        let Some(rx) = &self.pair_rx else { return };
        let events: Vec<_> = rx.try_iter().collect();
        for event in events {
            let phase = match event {
                crate::pairing::PairEvent::Found(host) => pair::Phase::Pairing(host),
                crate::pairing::PairEvent::Connected(msg) => {
                    self.refresh();
                    pair::Phase::Connected(msg)
                }
                crate::pairing::PairEvent::Failed(why) => pair::Phase::Failed(why),
            };
            if let Some(p) = &mut self.pairing {
                p.phase = phase;
            }
        }
        let timed_out = self.pairing.as_ref().is_some_and(|p| {
            matches!(p.phase, pair::Phase::Waiting)
                && p.started.elapsed() > Duration::from_secs(p.timeout)
        });
        if timed_out {
            self.cancel_pairing();
            self.message = "pairing timed out".into();
        }
    }

    /// Refresh from the adb server over the smart socket. This never runs `adb`, so it
    /// cannot start the unsupervised server it is meant to report.
    pub fn refresh(&mut self) {
        // With no unit of ours there is nothing to report on. Reading the default port
        // here would list a stranger's devices as though wadb were supervising them.
        if self.unit == UnitState::NotInstalled {
            self.server_up = false;
            self.devices.clear();
            self.owner = PortOwner::Nobody;
            return;
        }
        let sock = SmartSocket::new(self.port);
        self.server_up = sock.is_up();
        self.devices = if self.server_up {
            sock.devices()
                .map(|all| crate::adb::wireless_devices(&all))
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        self.owner = service::port_owner(self.port);
    }
}

/// One journal line, split so it can be sorted, filtered and rendered.
///
/// `journalctl -o short-iso` gives `<iso-ts> <host> <unit>[<pid>]: <message>`.
pub fn parse_journal_line(line: &str) -> Option<(String, String)> {
    let (timestamp, rest) = line.split_once(' ')?;
    if !timestamp.starts_with("20") {
        return None;
    }
    // Strip `<host> <process>[<pid>]: `.
    let message = rest.split_once("]: ").map(|(_, m)| m).unwrap_or(rest);
    Some((timestamp.to_string(), message.trim().to_string()))
}

/// Is this journal message worth a user's attention?
///
/// adb logs its own internals at info — transport lifecycle, libusb threads, key loading — dozens
/// of lines per device per restart, enough that our own reconnect messages fell outside a
/// thousand-line window entirely. Warnings and errors from adb are kept; those are worth reading.
pub fn worth_showing(message: &str) -> bool {
    if message.trim().is_empty() {
        return false;
    }
    // `<pid> <tid> <level> adb : file.cpp:NN message`
    let level = message
        .split(" adb ")
        .next()
        .and_then(|head| head.rsplit(' ').find(|token| token.len() == 1));
    !matches!(level, Some("I") | Some("D") | Some("V"))
}

/// Trim a message to what is readable in a narrow pane.
///
/// Drops adb's `file.cpp:NN` source location, of no use outside adb's own tree, and systemd's
/// restatement of the unit description, which is the same forty characters on every line.
pub fn shorten_log(message: &str) -> String {
    let message = match message.split_once(".cpp:") {
        Some((_, rest)) => rest.split_once(' ').map(|(_, m)| m).unwrap_or(message),
        None => message,
    };
    match message.split_once(" - ") {
        Some((head, _)) if head.ends_with(".service") || head.contains(".service:") => {
            head.to_string()
        }
        _ => message.trim().to_string(),
    }
}

/// The smallest terminal the layout fits in. Below this the panes would overlap into
/// nonsense, so we say so instead of drawing it.
pub const MIN_WIDTH: u16 = 78;
/// Header (3) + the pairing pane + footer (1). The QR is the tallest thing drawn and a
/// clipped QR does not scan, so this is derived from the pane rather than guessed: an
/// earlier value of 26 clipped four rows off the code while the guard reported the
/// terminal was big enough.
pub const MIN_HEIGHT: u16 = 30;

pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        let msg = Paragraph::new(vec![
            Line::from("terminal too small"),
            Line::from(format!(
                "need {MIN_WIDTH}x{MIN_HEIGHT}, have {}x{}",
                area.width, area.height
            )),
        ])
        .wrap(Wrap { trim: true });
        frame.render_widget(msg, area);
        return;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        // The journal folds away while pairing: the QR needs the height more than the
        // log does, and a clipped QR does not scan.
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(if app.pairing.is_some() { 0 } else { 7 }),
            Constraint::Length(1),
        ])
        .split(area);

    render_header(frame, rows[0], app);

    let pane = match &app.pairing {
        // Width comes from the QR itself, so a different payload size cannot clip it.
        Some(p) => pair::min_size(&p.qr).0,
        None => 0,
    };
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(30), Constraint::Length(pane)])
        .split(rows[1]);

    devices::render(
        frame,
        cols[0],
        &app.devices,
        app.server_up,
        app.unit != UnitState::NotInstalled,
    );
    if let Some(p) = &app.pairing {
        let view = pair::PairView {
            qr: &p.qr,
            phase: &p.phase,
            elapsed: p.started.elapsed().as_secs(),
            timeout: p.timeout,
        };
        pair::render(frame, cols[1], &view, app.tick);
    }

    if app.pairing.is_none() {
        render_logs(frame, rows[2], app);
    }
    render_footer(frame, rows[3], app);
}

fn render_header(frame: &mut Frame, area: Rect, app: &App) {
    let (dot, text, colour) = match (&app.unit, app.server_up, &app.owner) {
        (UnitState::NotInstalled, _, _) => (
            "○",
            "not installed - run `wadb install`".to_string(),
            Color::Yellow,
        ),
        (_, false, _) => ("○", "adb server down".to_string(), Color::Red),
        (_, true, PortOwner::HeldUnknown) => (
            "▲",
            format!("port {} is held, owner unknown", app.port),
            Color::Yellow,
        ),
        (_, true, PortOwner::Foreign) => (
            "▲",
            format!(
                "port {} held by another adb server - `wadb takeover`",
                app.port
            ),
            Color::Yellow,
        ),
        (UnitState::Active, true, PortOwner::Ours(pid)) => {
            ("●", format!("supervised, pid {pid}"), Color::Green)
        }
        (_, true, _) => ("●", "server up, unit inactive".to_string(), Color::Yellow),
    };

    let mut spans = vec![
        Span::styled(
            " wadb ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(dot, Style::default().fg(colour)),
        Span::raw(" "),
        Span::styled(text, Style::default().fg(colour)),
    ];
    if !app.message.is_empty() {
        spans.push(Span::raw("   "));
        spans.push(Span::styled(
            app.message.clone(),
            Style::default().fg(Color::DarkGray),
        ));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn render_logs(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        " journal ",
        Style::default().add_modifier(Modifier::BOLD),
    ));
    let rows = area.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = if app.logs.is_empty() {
        vec![Line::from(Span::styled(
            "nothing from the unit yet (a volatile journal keeps no history across reboots)",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        app.logs
            .iter()
            .rev()
            .take(rows)
            .rev()
            .map(|l| {
                Line::from(Span::styled(
                    l.clone(),
                    Style::default().fg(Color::DarkGray),
                ))
            })
            .collect()
    };
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_footer(frame: &mut Frame, area: Rect, app: &App) {
    let keys: &[(&str, &str)] = if app.pairing.is_some() {
        &[("esc", "cancel pairing"), ("q", "quit")]
    } else if app.unit == UnitState::NotInstalled {
        &[
            ("i", "install"),
            ("p", "pair"),
            ("r", "refresh"),
            ("q", "quit"),
        ]
    } else if app.unit != UnitState::Active {
        &[
            ("s", "start"),
            ("p", "pair"),
            ("r", "refresh"),
            ("q", "quit"),
        ]
    } else {
        &[("p", "pair"), ("r", "refresh"), ("q", "quit")]
    };
    let mut spans = Vec::new();
    for (key, label) in keys {
        spans.push(Span::styled(
            format!(" {key} "),
            Style::default().fg(Color::Black).bg(Color::DarkGray),
        ));
        spans.push(Span::styled(
            format!(" {label}   "),
            Style::default().fg(Color::DarkGray),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// One key press. Returns true when the UI should redraw immediately.
pub fn handle_key(app: &mut App, code: KeyCode) -> bool {
    match code {
        KeyCode::Char('q') | KeyCode::Char('Q') => {
            // Quitting must stop the worker too, or it keeps browsing and can pair and
            // connect after the TUI is gone.
            app.cancel_pairing();
            app.should_quit = true;
            true
        }
        KeyCode::Esc if app.pairing.is_some() => {
            app.cancel_pairing();
            app.message = "pairing cancelled".into();
            true
        }
        KeyCode::Char('r') => {
            app.refresh();
            true
        }
        KeyCode::Char('p') if app.pairing.is_none() => {
            app.start_pairing(120);
            true
        }
        KeyCode::Char('i') if app.unit == UnitState::NotInstalled => {
            app.message = match crate::service::install() {
                // "enable --now returned 0" is not "the unit owns the port".
                Ok(r) => match crate::service::wait_for_ownership(r.port, Duration::from_secs(5)) {
                    PortOwner::Ours(pid) => format!("supervising {} (pid {pid})", r.adb.display()),
                    PortOwner::Foreign | PortOwner::HeldUnknown => {
                        "installed, but another adb server holds the port - press t".into()
                    }
                    PortOwner::Nobody => "installed, but the unit is not listening yet".into(),
                },
                Err(e) => format!("install failed: {e}"),
            };
            app.unit = crate::ui::current_unit_state();
            app.refresh();
            true
        }
        KeyCode::Char('s') if app.unit == UnitState::Inactive => {
            app.message = match crate::service::start() {
                Ok(()) => "unit started".into(),
                Err(e) => format!("could not start the unit: {e}"),
            };
            app.unit = crate::ui::current_unit_state();
            app.refresh();
            true
        }
        _ => false,
    }
}

pub fn run(app: &mut App) -> Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, app);
    ratatui::restore();
    result
}

fn event_loop<B>(terminal: &mut Terminal<B>, app: &mut App) -> Result<()>
where
    B: ratatui::backend::Backend,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let mut last_refresh = Instant::now() - Duration::from_secs(10);
    loop {
        if last_refresh.elapsed() >= Duration::from_secs(2) {
            app.refresh();
            app.refresh_logs();
            last_refresh = Instant::now();
        }
        app.poll_pairing();
        terminal.draw(|frame| render(frame, app))?;

        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    handle_key(app, key.code);
                }
            }
        }
        app.tick = app.tick.wrapping_add(1);
        if app.should_quit {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adb::parse_devices;
    use ratatui::backend::TestBackend;

    fn draw(app: &App, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| render(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn app_with(devices: &str) -> App {
        let mut app = App::new(5037, None, UnitState::Active);
        app.devices = crate::adb::wireless_devices(&parse_devices(devices));
        app.server_up = true;
        app.owner = PortOwner::Ours(4242);
        app
    }

    #[test]
    fn lists_a_wireless_device_with_its_state() {
        let app = app_with("192.168.1.42:37219 device model:Pixel_9\n");
        let out = draw(&app, 90, 30);
        assert!(out.contains("Pixel_9"));
        assert!(out.contains("192.168.1.42:37219"));
        assert!(out.contains("supervised, pid 4242"));
    }

    #[test]
    fn mdns_attached_device_is_shown_shortened() {
        let app =
            app_with("adb-39061FDJH00KZR-vWTMTB._adb-tls-connect._tcp device model:Pixel_9\n");
        let out = draw(&app, 90, 30);
        assert!(out.contains("adb-39061FDJH00KZR-vWTMTB"));
        assert!(
            !out.contains("_adb-tls-connect._tcp"),
            "suffix is noise in a column"
        );
        assert!(out.contains("mdns"));
    }

    #[test]
    fn empty_state_points_at_pairing_not_at_nothing() {
        let app = app_with("1BC4F9AK99001 device model:Pixel_6\n");
        let out = draw(&app, 90, 30);
        assert!(
            out.contains("no wireless devices"),
            "USB devices are not listed"
        );
        assert!(out.contains("press p to pair"));
    }

    #[test]
    fn server_down_is_stated_plainly() {
        let mut app = App::new(5037, None, UnitState::Active);
        app.server_up = false;
        let out = draw(&app, 90, 30);
        assert!(out.contains("adb server down"));
    }

    #[test]
    fn first_run_says_it_is_not_installed() {
        // Before any install there is no unit to report on, and describing some other
        // server on the default port would be a lie.
        let app = App::new(5037, None, UnitState::NotInstalled);
        let out = draw(&app, 90, 30);
        assert!(out.contains("not installed"));
        assert!(out.contains("wadb install"));
    }

    #[test]
    fn foreign_server_names_the_way_out() {
        let mut app = App::new(5037, None, UnitState::Inactive);
        app.server_up = true;
        app.owner = PortOwner::Foreign;
        let out = draw(&app, 90, 30);
        assert!(out.contains("held by another adb server"));
        assert!(
            out.contains("takeover"),
            "a warning with no remedy is a dead end"
        );
    }

    #[test]
    fn the_declared_minimum_size_actually_fits_the_qr() {
        // MIN_HEIGHT is the number the guard trusts; if the QR is clipped at exactly that
        // size the guard is lying and the code will not scan.
        let mut app = app_with("");
        app.pairing = Some(Pairing {
            qr: crate::qr::encode(b"WIFI:T:ADB;S:studio-Ab3xK9pQr2;P:7fRt2LmNz4;;").unwrap(),
            phase: pair::Phase::Waiting,
            started: Instant::now(),
            timeout: 120,
        });
        let out = draw(&app, MIN_WIDTH, MIN_HEIGHT);
        assert!(!out.contains("terminal too small"));
        // 19 half-block rows are drawn, but the outermost two rows at each end are the
        // quiet zone and carry no glyphs, so 15 rows of the code proper must be present.
        let glyph_rows = out.lines().filter(|l| l.contains(['█', '▀', '▄'])).count();
        assert_eq!(glyph_rows, 15, "the code is clipped");
        // The lines below the code are what an undersized MIN_HEIGHT eats first.
        assert!(out.contains("waiting for a scan"));
        assert!(
            out.contains("Wireless debugging"),
            "instructions were clipped"
        );
    }

    #[test]
    fn first_run_reads_nothing_from_a_server_that_is_not_ours() {
        // A foreign server on the default port must not have its devices listed as though
        // wadb were supervising them.
        let mut app = App::new(5037, None, UnitState::NotInstalled);
        app.devices = crate::adb::parse_devices("192.168.1.42:37219 device model:Pixel_9\n");
        app.server_up = true;
        app.refresh();
        assert!(app.devices.is_empty());
        assert!(!app.server_up);
        assert_eq!(app.owner, PortOwner::Nobody);
    }

    #[test]
    fn pairing_is_refused_before_anything_is_installed() {
        let mut app = App::new(5037, None, UnitState::NotInstalled);
        app.start_pairing(120);
        assert!(app.pairing.is_none());
        assert!(app.message.contains("install"));
    }

    #[test]
    fn too_small_terminal_says_so_instead_of_drawing_rubbish() {
        let app = app_with("192.168.1.42:37219 device model:Pixel_9\n");
        let out = draw(&app, 40, 12);
        assert!(out.contains("terminal too small"));
        assert!(out.contains("40x12"));
    }

    #[test]
    fn pairing_pane_shows_the_qr_and_the_instructions() {
        let mut app = app_with("");
        app.pairing = Some(Pairing {
            qr: crate::qr::encode(b"WIFI:T:ADB;S:studio-Ab3xK9pQr2;P:7fRt2LmNz4;;").unwrap(),
            phase: pair::Phase::Waiting,
            started: Instant::now(),
            timeout: 120,
        });
        let out = draw(&app, 100, 34);
        assert!(out.contains("waiting for a scan"));
        assert!(out.contains("Wireless debugging"));
        assert!(
            out.contains('█') || out.contains('▀'),
            "the QR must actually be drawn"
        );
    }

    #[test]
    fn advertised_keys_are_the_keys_that_work() {
        // The empty state used to tell users to press i and s while neither was handled.
        let mut app = App::new(5037, None, UnitState::NotInstalled);
        let out = draw(&app, 90, 30);
        assert!(
            out.contains(" i "),
            "install key must be offered when not installed"
        );
        app.unit = UnitState::Inactive;
        let out = draw(&app, 90, 30);
        assert!(
            out.contains(" s "),
            "start key must be offered when inactive"
        );
        app.unit = UnitState::Active;
        let out = draw(&app, 90, 30);
        assert!(!out.contains(" i "), "nothing to install once it is active");
    }

    #[test]
    fn journal_lines_split_into_timestamp_and_message() {
        let line = "2026-09-02T16:26:53+02:00 money-maker wadb[1455422]: wadb: connected to 192.168.86.45:42595";
        let (ts, msg) = parse_journal_line(line).expect("a short-iso line parses");
        assert_eq!(ts, "2026-09-02T16:26:53+02:00");
        assert_eq!(msg, "wadb: connected to 192.168.86.45:42595");
        // Anything that is not a timestamped line is ignored rather than shown raw.
        assert!(parse_journal_line("-- Boot 1a2b --").is_none());
        assert!(parse_journal_line("").is_none());
    }

    #[test]
    fn systemd_unit_descriptions_are_trimmed() {
        // Every systemd line restates the same forty-character description.
        assert_eq!(
            shorten_log("Started wadb-connect.service - Reconnect wireless ADB devices that adb's own mDNS cannot find (wadb)."),
            "Started wadb-connect.service"
        );
        assert_eq!(
            shorten_log("wadb: connected to 192.168.86.45:42595"),
            "wadb: connected to 192.168.86.45:42595"
        );
    }

    #[test]
    fn adb_internal_chatter_is_filtered_out() {
        // Real lines captured from the pane, which was five-sixths this.
        let noise = [
            "09-02 18:30:19.663 1455377 1455377 I adb     : transport.cpp:404 BlockingConnectionAdapter(<unknown>): not started",
            "09-02 18:30:19.675 1455377 1455377 I adb     : transport.cpp:302 BlockingConnectionAdapter(<unknown>): destructing",
            "09-01 12:53:28.419 2726685 2726774 I adb     : usb_libusb.cpp:119 35191FDHS0003Q: write thread spawning",
            "09-01 12:53:28.599 2726685 2726685 I adb     : transport.cpp:1720 fetching keys for transport 3C231JEKB44234",
            "",
        ];
        for line in noise {
            assert!(!worth_showing(line), "should be filtered: {line}");
        }
    }

    #[test]
    fn the_lines_a_user_wants_survive() {
        // Our own watcher output, and systemd's unit lifecycle.
        for line in [
            "wadb: connected to 192.168.86.45:42595",
            "wadb: watching _adb-tls-connect._tcp.local. for devices to reconnect on port 5037",
            "Started wadb.service - Keep the ADB server running for wireless debugging (wadb).",
            "Stopping wadb-connect.service - Reconnect wireless ADB devices…",
            "wadb.service: Consumed 18.196s CPU time, 6.3M memory peak.",
        ] {
            assert!(worth_showing(line), "should be kept: {line}");
        }
    }

    #[test]
    fn adb_warnings_and_errors_are_kept() {
        // The whole point of filtering by level rather than by the word "adb".
        for line in [
            "09-02 18:30:19.663 1455377 1455377 W adb     : adb.cpp:100 failed to bind socket",
            "09-02 18:30:19.663 1455377 1455377 E adb     : adb.cpp:100 could not read key",
            "09-02 18:30:19.663 1455377 1455377 F adb     : main.cpp:167 could not install listener",
        ] {
            assert!(worth_showing(line), "should be kept: {line}");
        }
    }

    #[test]
    fn adb_source_locations_are_stripped() {
        assert_eq!(
            shorten_log("09-02 18:30 1 1 W adb     : adb.cpp:100 failed to bind socket"),
            "failed to bind socket"
        );
    }

    #[test]
    fn journal_pane_is_shown_and_folds_away_while_pairing() {
        let mut app = app_with("");
        app.logs = vec!["started adb server".into()];
        let out = draw(&app, 90, 30);
        assert!(out.contains("journal"));
        assert!(out.contains("started adb server"));

        app.pairing = Some(Pairing {
            qr: crate::qr::encode(b"WIFI:T:ADB;S:studio-a;P:b;;").unwrap(),
            phase: pair::Phase::Waiting,
            started: Instant::now(),
            timeout: 120,
        });
        let out = draw(&app, 90, 30);
        assert!(
            !out.contains("journal"),
            "the QR needs the height more than the log does"
        );
    }

    #[test]
    fn cancelling_signals_the_worker_not_just_the_channel() {
        let mut app = app_with("");
        app.pairing = Some(Pairing {
            qr: crate::qr::encode(b"WIFI:T:ADB;S:studio-a;P:b;;").unwrap(),
            phase: pair::Phase::Waiting,
            started: Instant::now(),
            timeout: 120,
        });
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        app.pair_cancel = Some(flag.clone());
        app.cancel_pairing();
        // A worker that never sees this keeps browsing and can pair after the cancel.
        assert!(flag.load(std::sync::atomic::Ordering::Relaxed));
        assert!(app.pairing.is_none());
    }

    #[test]
    fn quitting_also_stops_the_pairing_worker() {
        // Esc used to be the only path that signalled the worker, so quitting mid-pair
        // left it browsing and able to pair after the TUI was gone.
        let mut app = app_with("");
        app.pairing = Some(Pairing {
            qr: crate::qr::encode(b"WIFI:T:ADB;S:studio-a;P:b;;").unwrap(),
            phase: pair::Phase::Waiting,
            started: Instant::now(),
            timeout: 120,
        });
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        app.pair_cancel = Some(flag.clone());
        handle_key(&mut app, KeyCode::Char('q'));
        assert!(app.should_quit);
        assert!(flag.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn start_key_is_offered_only_when_there_is_a_unit_to_start() {
        let mut app = App::new(5037, None, UnitState::NotInstalled);
        // `s` would have run `systemctl start` on a unit that does not exist.
        handle_key(&mut app, KeyCode::Char('s'));
        assert!(app.message.is_empty());
    }

    #[test]
    fn q_quits_and_esc_cancels_pairing() {
        let mut app = app_with("");
        app.pairing = Some(Pairing {
            qr: crate::qr::encode(b"WIFI:T:ADB;S:studio-a;P:b;;").unwrap(),
            phase: pair::Phase::Waiting,
            started: Instant::now(),
            timeout: 120,
        });
        handle_key(&mut app, KeyCode::Esc);
        assert!(app.pairing.is_none());
        assert!(!app.should_quit);
        handle_key(&mut app, KeyCode::Char('q'));
        assert!(app.should_quit);
    }
}
