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
        }
    }

    /// Show a QR and hand the credential to a worker thread. The payload moves into the
    /// worker, so the password never lives in UI state that gets rendered or logged.
    pub fn start_pairing(&mut self, timeout_secs: u64) {
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
        let port = self.port;
        std::thread::spawn(move || {
            crate::pairing::run_pairing(adb, port, payload, Duration::from_secs(timeout_secs), tx);
        });
        self.pair_rx = Some(rx);
        self.message.clear();
        self.pairing = Some(Pairing {
            qr,
            phase: pair::Phase::Waiting,
            started: Instant::now(),
            timeout: timeout_secs,
        });
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
        if let Some(p) = &self.pairing {
            if matches!(p.phase, pair::Phase::Waiting)
                && p.started.elapsed() > Duration::from_secs(p.timeout)
            {
                self.message = "pairing timed out".into();
                self.pairing = None;
                self.pair_rx = None;
            }
        }
    }

    /// Refresh from the adb server over the smart socket. This never runs `adb`, so it
    /// cannot start the unsupervised server it is meant to report.
    pub fn refresh(&mut self) {
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

/// The smallest terminal the layout fits in. Below this the panes would overlap into
/// nonsense, so we say so instead of drawing it.
pub const MIN_WIDTH: u16 = 78;
pub const MIN_HEIGHT: u16 = 26;

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
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
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

    devices::render(frame, cols[0], &app.devices, app.server_up);
    if let Some(p) = &app.pairing {
        let view = pair::PairView {
            qr: &p.qr,
            phase: &p.phase,
            elapsed: p.started.elapsed().as_secs(),
            timeout: p.timeout,
        };
        pair::render(frame, cols[1], &view, app.tick);
    }

    render_footer(frame, rows[2], app);
}

fn render_header(frame: &mut Frame, area: Rect, app: &App) {
    let (dot, text, colour) = match (&app.unit, app.server_up, &app.owner) {
        (UnitState::NotInstalled, _, _) => (
            "○",
            "not installed - run `wadb install`".to_string(),
            Color::Yellow,
        ),
        (_, false, _) => ("○", "adb server down".to_string(), Color::Red),
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

fn render_footer(frame: &mut Frame, area: Rect, app: &App) {
    let keys: &[(&str, &str)] = if app.pairing.is_some() {
        &[("esc", "cancel pairing"), ("q", "quit")]
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
            app.should_quit = true;
            true
        }
        KeyCode::Esc if app.pairing.is_some() => {
            app.pairing = None;
            // Dropping the receiver lets the worker finish and shut its browser down.
            app.pair_rx = None;
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
