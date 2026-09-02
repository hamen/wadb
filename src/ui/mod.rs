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
        let mut lines: Vec<JournalLine> = Vec::new();
        for (unit, count) in [
            (crate::service::CONNECT_UNIT_NAME, "40"),
            // adb emits over a thousand consecutive info lines under load, so a warning followed
            // by a flood would fall outside a small window — and warnings are the only reason this
            // unit is read at all. The measured cost is per query, not per line.
            (crate::service::UNIT_NAME, "4000"),
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
                    // Precise: two units log within the same second often enough that
                    // second resolution leaves their order down to which query ran first.
                    "short-iso-precise",
                ])
                .output();
            if let Ok(out) = out {
                lines.extend(
                    String::from_utf8_lossy(&out.stdout)
                        .lines()
                        .filter_map(parse_journal_line)
                        .filter(worth_showing),
                );
            }
        }
        self.logs = render_log_lines(lines);
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

/// One journal line: when, which process wrote it, and what it said.
///
/// `journalctl -o short-iso` gives `<iso-ts> <host> <process>[<pid>]: <message>`. The process name
/// is the authoritative signal for whose line this is — keying off the message text instead means
/// adb's own sub-tags (`D mdns : …`, from the same server) slip through, and any other line that
/// happens to contain the word gets judged as if adb had written it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalLine {
    pub timestamp: String,
    pub process: String,
    pub message: String,
}

pub fn parse_journal_line(line: &str) -> Option<JournalLine> {
    let (timestamp, rest) = line.split_once(' ')?;
    // A timestamp, not a continuation line of a multi-line entry and not journalctl's own
    // `-- Boot … --` banners.
    if !timestamp.starts_with(|c: char| c.is_ascii_digit()) || !timestamp.contains('T') {
        return None;
    }
    let (head, message) = rest.split_once("]: ")?;
    let process = head
        .rsplit_once('[')
        .map(|(name, _)| name)
        .unwrap_or(head)
        .rsplit(' ')
        .next()
        .unwrap_or(head);
    Some(JournalLine {
        timestamp: timestamp.to_string(),
        process: process.to_string(),
        message: message.trim().to_string(),
    })
}

/// Days since an arbitrary fixed date, exactly.
///
/// Howard Hinnant's civil-date algorithm. An approximation with 31-day months is not good enough:
/// across a short month boundary with an offset change it inverts real order —
/// `2026-03-01T00:15:00+01:00` is 23:15 UTC on the 28th of February, and so precedes
/// `2026-02-28T23:30:00+00:00`, which a month-times-31 key gets backwards.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = (month + 9) % 12;
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// A key that orders timestamps by real time, in microseconds.
///
/// `journalctl` stamps each entry with its own UTC offset, so a plain string sort puts `+01:00`
/// lines before `+02:00` lines during the repeated hour of a DST fall-back. Microsecond resolution
/// matters because two units routinely log within the same second, and a coarser key leaves their
/// order decided by which query ran first rather than by the journal.
pub fn sort_key(timestamp: &str) -> i64 {
    let Some((date, rest)) = timestamp.split_once('T') else {
        return 0;
    };
    let (time, offset_minutes) = match rest.rfind(['+', '-']) {
        Some(i) => {
            let sign = if rest.as_bytes()[i] == b'-' { -1 } else { 1 };
            // `+HH:MM` and `+HHMM` are both emitted, depending on journalctl and locale.
            let zone = rest[i + 1..].replace(':', "");
            let hours: i64 = zone.get(..2).and_then(|v| v.parse().ok()).unwrap_or(0);
            let minutes: i64 = zone.get(2..4).and_then(|v| v.parse().ok()).unwrap_or(0);
            (&rest[..i], sign * (hours * 60 + minutes))
        }
        None => (rest.trim_end_matches('Z'), 0),
    };

    let number = |part: Option<&str>| -> i64 { part.and_then(|p| p.parse().ok()).unwrap_or(0) };
    let mut date = date.split('-');
    let days = days_from_civil(
        number(date.next()),
        number(date.next()),
        number(date.next()),
    );

    let mut time = time.split(':');
    let hours = number(time.next());
    let minutes = number(time.next());
    let (seconds, fraction) = match time.next() {
        Some(s) => match s.split_once('.') {
            // Pad or trim to microseconds, whatever precision the journal used.
            Some((whole, frac)) => (
                whole.parse().unwrap_or(0),
                format!("{frac:0<6}")[..6].parse().unwrap_or(0),
            ),
            None => (s.parse().unwrap_or(0), 0),
        },
        None => (0, 0),
    };

    ((((days * 1440 + hours * 60 + minutes - offset_minutes) * 60) + seconds) * 1_000_000)
        + fraction
}

/// Is this line worth a user's attention?
///
/// adb logs its own internals at info — transport lifecycle, libusb threads, key loading — dozens
/// of lines per device per restart, enough that our own reconnect messages fell outside a
/// thousand-line window entirely. Warnings and errors from adb are kept; those are worth reading.
/// Only lines adb itself wrote are judged this way.
pub fn worth_showing(line: &JournalLine) -> bool {
    if line.message.trim().is_empty() {
        return false;
    }
    if line.process != "adb" {
        return true;
    }
    !matches!(adb_level(&line.message), Some("I") | Some("D") | Some("V"))
}

/// adb's log level, located by shape rather than by position.
///
/// The format is `<date> <time> <pid> <tid> <LEVEL> <tag> : file.cc:NN message`. A fixed field
/// index gets this wrong — index 2 is the pid, not the level — and a filter that reads the pid
/// keeps every line it was meant to drop. The level is instead found as the single letter directly
/// before a tag and its colon, which holds for adb's sub-tags (`D mdns : …`) as well.
pub fn adb_level(message: &str) -> Option<&str> {
    let fields: Vec<&str> = message.split_whitespace().collect();
    fields.windows(3).find_map(|w| {
        let level = w[0];
        (w[2] == ":" && matches!(level, "I" | "D" | "V" | "W" | "E" | "F")).then_some(level)
    })
}

/// Trim a message to what is readable in a narrow pane.
///
/// Drops adb's `file.cpp:NN` source location, of no use outside adb's own tree, and systemd's
/// restatement of the unit description, which is the same forty characters on every line.
pub fn shorten_log(process: &str, message: &str) -> String {
    // Only adb writes source locations. Applying this to every process would truncate a wadb or
    // systemd message that merely mentions a file.
    let source_location = (process == "adb")
        .then(|| {
            message
                .split_once(".cpp:")
                .or_else(|| message.split_once(".cc:"))
        })
        .flatten();
    // A warning must not render like an info line once its level is stripped.
    let marker = match adb_level(message) {
        Some(level @ ("W" | "E" | "F")) if process == "adb" => format!("{level}: "),
        _ => String::new(),
    };
    let message = match source_location {
        // Whatever follows the line number, even if there is nothing after it. Returning the
        // original here would keep the one thing this function exists to remove.
        Some((_, rest)) => rest.split_once(' ').map(|(_, m)| m).unwrap_or(""),
        None => message,
    };
    let body = match message.split_once(" - ") {
        Some((head, _)) if head.ends_with(".service") || head.contains(".service:") => head,
        _ => message.trim(),
    };
    format!("{marker}{body}")
}

/// Sort, trim and format the merged lines for the pane.
pub fn render_log_lines(mut lines: Vec<JournalLine>) -> Vec<String> {
    lines.sort_by_key(|l| sort_key(&l.timestamp));
    lines
        .into_iter()
        .map(|l| {
            let clock = l.timestamp.split('T').nth(1).unwrap_or(&l.timestamp);
            format!(
                "{}  {}",
                &clock[..clock.len().min(8)],
                shorten_log(&l.process, &l.message)
            )
        })
        .collect()
}

/// The smallest terminal the layout fits in. Below this the panes would overlap into nonsense,
/// so the guard says so instead of drawing it.
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

    fn line(process: &str, message: &str) -> JournalLine {
        JournalLine {
            timestamp: "2026-09-02T16:26:53+02:00".into(),
            process: process.into(),
            message: message.into(),
        }
    }

    #[test]
    fn journal_lines_split_into_timestamp_process_and_message() {
        let parsed = parse_journal_line(
            "2026-09-02T16:26:53+02:00 money-maker wadb[1455422]: wadb: connected to 192.168.86.45:42595",
        )
        .expect("a short-iso line parses");
        assert_eq!(parsed.timestamp, "2026-09-02T16:26:53+02:00");
        assert_eq!(parsed.process, "wadb", "the process is who wrote it");
        assert_eq!(parsed.message, "wadb: connected to 192.168.86.45:42595");

        // Banners and continuation lines carry no timestamp and are skipped.
        assert!(parse_journal_line("-- Boot 1a2b --").is_none());
        assert!(parse_journal_line("    continuation of a multi-line entry").is_none());
        assert!(parse_journal_line("").is_none());
    }

    #[test]
    fn adb_internal_chatter_is_filtered_out() {
        // Real messages captured from the pane, which was five-sixths this.
        // Copied verbatim from the live journal, adb's own date and time prefix included. The
        // previous fixtures omitted that prefix, which is why they passed against a filter that
        // read the pid as the level and kept every line it was meant to drop.
        for message in [
            "09-02 19:18:23.376 1455377 1455377 I adb     : transport.cpp:404 BlockingConnectionAdapter(<unknown>): not started",
            "09-02 18:30:19.675 1455377 1455377 I adb     : transport.cpp:302 BlockingConnectionAdapter(<unknown>): destructing",
            "09-01 12:53:28.419 2726685 2726774 I adb     : usb_libusb.cpp:119 35191FDHS0003Q: write thread spawning",
            // adb's own sub-tags: same process, different tag. Keying on the word "adb" let these
            // through, and this tool's whole purpose makes mDNS chatter plentiful.
            "09-02 19:18:23.376 1455377 1455377 D mdns    : mdns.cpp:120 request for service",
            "09-02 19:18:23.376 1455377 1455377 V openscreen : discovery.cc:44 querying",
        ] {
            assert!(!worth_showing(&line("adb", message)), "should be filtered: {message}");
        }
    }

    #[test]
    fn the_lines_a_user_wants_survive() {
        for (process, message) in [
            ("wadb", "wadb: connected to 192.168.86.45:42595"),
            (
                "wadb",
                "wadb: watching _adb-tls-connect._tcp.local. for devices to reconnect",
            ),
            (
                "systemd",
                "Started wadb.service - Keep the ADB server running (wadb).",
            ),
            (
                "systemd",
                "wadb.service: Consumed 18.196s CPU time, 6.3M memory peak.",
            ),
            // Not adb's line, so adb's level rules must not be applied to it: a lone I, D or V
            // in someone else's message used to hide it.
            ("wadb", "pairing failed: I D V adb tokens in the text"),
        ] {
            assert!(
                worth_showing(&line(process, message)),
                "should be kept: {message}"
            );
        }
    }

    #[test]
    fn adb_warnings_and_errors_are_kept() {
        for message in [
            "09-02 19:18:23.376 1 1 W adb     : adb.cpp:100 failed to bind socket",
            "09-02 19:18:23.376 1 1 E adb     : adb.cpp:100 could not read key",
            "09-02 19:18:23.376 1 1 F adb     : main.cpp:167 could not install listener",
        ] {
            assert!(
                worth_showing(&line("adb", message)),
                "should be kept: {message}"
            );
        }
    }

    #[test]
    fn adb_source_locations_are_stripped() {
        assert_eq!(
            shorten_log(
                "adb",
                "09-02 19:18 1 1 W adb     : adb.cpp:100 failed to bind socket"
            ),
            "W: failed to bind socket",
            "a warning must not read like an info line once its level is stripped"
        );
        // Nothing after the line number: the source location must still go, since removing it is
        // the entire point of the function.
        assert_eq!(
            shorten_log("adb", "09-02 19:18 1 1 W adb : adb.cpp:100"),
            "W: "
        );
        // openscreen logs from .cc files, and its warnings would have kept the path.
        assert_eq!(
            shorten_log(
                "adb",
                "09-02 19:18 1 1 W openscreen : discovery.cc:44 mdns socket unavailable"
            ),
            "W: mdns socket unavailable"
        );
        // Another process mentioning a source file keeps its whole message.
        assert_eq!(
            shorten_log("wadb", "wadb: parsed foo.cc:12 from the manifest"),
            "wadb: parsed foo.cc:12 from the manifest"
        );
    }

    #[test]
    fn systemd_unit_descriptions_are_trimmed() {
        assert_eq!(
            shorten_log("systemd", "Started wadb-connect.service - Reconnect wireless ADB devices that adb's own mDNS cannot find (wadb)."),
            "Started wadb-connect.service"
        );
        assert_eq!(
            shorten_log("wadb", "wadb: connected to 192.168.86.45:42595"),
            "wadb: connected to 192.168.86.45:42595"
        );
    }

    #[test]
    fn the_pane_merges_both_units_in_time_order() {
        // Two units, interleaved, arriving in the wrong order as they do from two queries.
        let lines = vec![
            JournalLine {
                timestamp: "2026-09-02T16:26:53.500000+02:00".into(),
                process: "wadb".into(),
                message: "wadb: connected to 192.168.86.45:42595".into(),
            },
            JournalLine {
                timestamp: "2026-09-02T16:26:40.100000+02:00".into(),
                process: "systemd".into(),
                message: "Started wadb-connect.service - Reconnect wireless ADB devices.".into(),
            },
        ];
        assert_eq!(
            render_log_lines(lines),
            vec![
                "16:26:40  Started wadb-connect.service".to_string(),
                "16:26:53  wadb: connected to 192.168.86.45:42595".to_string(),
            ]
        );
    }

    #[test]
    fn same_second_lines_keep_journal_order() {
        // Two units routinely log within the same second. At second resolution their keys were
        // equal and the order came from whichever query ran first, not from the journal.
        let first = "2026-09-02T16:26:53.101000+02:00";
        let second = "2026-09-02T16:26:53.987000+02:00";
        assert!(sort_key(first) < sort_key(second));

        let lines = vec![
            JournalLine {
                timestamp: second.into(),
                process: "wadb".into(),
                message: "wadb: connected to 192.168.86.45:42595".into(),
            },
            JournalLine {
                timestamp: first.into(),
                process: "systemd".into(),
                message: "Started wadb-connect.service - Reconnect wireless ADB devices.".into(),
            },
        ];
        let rendered = render_log_lines(lines);
        assert!(rendered[0].contains("Started"), "got {rendered:?}");
        assert!(rendered[1].contains("connected"), "got {rendered:?}");
    }

    /// Runs the real pane against this machine's journal.
    ///
    /// The unit tests are only as good as their fixtures, and this filter shipped a version that
    /// did nothing because the fixtures omitted adb's own date and time prefix. This one cannot be
    /// fooled that way: whatever the journal actually holds, no adb info line may reach the pane.
    #[test]
    #[ignore = "reads the live journal; run with --ignored"]
    fn the_live_pane_contains_no_adb_chatter() {
        let mut app = App::new(5037, None, UnitState::Active);
        app.refresh_logs();
        eprintln!("pane has {} lines", app.logs.len());
        for line in app.logs.iter().rev().take(5) {
            eprintln!("  {line}");
        }
        for line in &app.logs {
            assert!(
                !line.contains("BlockingConnectionAdapter"),
                "adb chatter reached the pane: {line}"
            );
            assert!(
                !line.contains(" I adb "),
                "an adb info line reached the pane: {line}"
            );
        }
    }

    #[test]
    fn the_level_is_found_by_shape_not_position() {
        // Field 2 is the pid. A fixed index read it as the level and kept every info line.
        let live = "09-02 19:18:23.376 1455377 1455377 I adb     : transport.cpp:404 not started";
        assert_eq!(
            live.split_whitespace().nth(2),
            Some("1455377"),
            "field 2 is the pid"
        );
        assert_eq!(adb_level(live), Some("I"));
        assert_eq!(adb_level("09-02 19:18 1 1 W adb : adb.cpp:1 x"), Some("W"));
        assert_eq!(adb_level("wadb: connected to 192.168.86.45:42595"), None);
    }

    #[test]
    fn offsets_parse_with_or_without_a_colon() {
        // journalctl emits either shape depending on version and locale.
        assert_eq!(
            sort_key("2026-09-02T16:26:53.000000+05:30"),
            sort_key("2026-09-02T16:26:53.000000+0530")
        );
        // A half-hour zone really is half an hour off a whole one.
        assert_eq!(
            sort_key("2026-09-02T16:26:53.000000+05:00")
                - sort_key("2026-09-02T16:26:53.000000+05:30"),
            30 * 60 * 1_000_000
        );
    }

    #[test]
    fn negative_offsets_and_zulu_are_handled() {
        // Most of North America emits -HH:MM, and the sign branch is one character of logic.
        assert_eq!(
            sort_key("2026-09-02T12:00:00.000000-05:00"),
            sort_key("2026-09-02T17:00:00.000000+00:00"),
            "noon in New York is 17:00 UTC"
        );
        assert_eq!(
            sort_key("2026-09-02T17:00:00.000000Z"),
            sort_key("2026-09-02T17:00:00.000000+00:00"),
            "a Z suffix is UTC"
        );
    }

    #[test]
    fn dates_are_exact_across_a_short_month_boundary() {
        // 00:15 on 1 March at +01:00 is 23:15 UTC on 28 February, so it precedes 23:30 UTC that
        // day. A key giving every month 31 days puts them the other way round.
        let earlier = "2026-03-01T00:15:00.000000+01:00";
        let later = "2026-02-28T23:30:00.000000+00:00";
        assert!(
            sort_key(earlier) < sort_key(later),
            "March 1st 00:15+01:00 is earlier in real time than February 28th 23:30 UTC"
        );
        // Consecutive days across the boundary stay in order.
        assert!(
            sort_key("2026-02-28T12:00:00.000000+00:00")
                < sort_key("2026-03-01T12:00:00.000000+00:00")
        );
        // And a leap day is a real day.
        assert_eq!(
            sort_key("2028-03-01T00:00:00.000000+00:00")
                - sort_key("2028-02-29T00:00:00.000000+00:00"),
            86_400_000_000
        );
    }

    #[test]
    fn timestamps_order_correctly_across_a_utc_offset_change() {
        // A DST fall-back repeats an hour with a different offset. 02:30+02:00 is 00:30 UTC and
        // happened BEFORE 02:10+01:00, which is 01:10 UTC — the reverse of a string sort.
        let earlier = "2026-10-25T02:30:00.000000+02:00"; // 00:30 UTC
        let later = "2026-10-25T02:10:00.000000+01:00"; // 01:10 UTC
        assert!(
            later < earlier,
            "a plain string sort would put the later entry first"
        );
        assert!(
            sort_key(earlier) < sort_key(later),
            "the sort key must reflect real time, not the printed offset"
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
