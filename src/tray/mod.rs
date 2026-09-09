// SPDX-License-Identifier: Apache-2.0

//! A panel icon, for people who do not keep a terminal open.
//!
//! This speaks StatusNotifierItem over DBus, which is what XFCE's tray plugin, KDE and the GNOME
//! AppIndicator extension all consume. It is the same shape as the macOS menu-bar app this project
//! is modelled on: an icon that answers "is a phone attached?", and a menu that lists the wireless
//! devices, opens pairing, and reconnects.
//!
//! Nothing slow runs on the DBus thread. A menu click sends an [`Action`] to the run loop, which
//! performs it on a thread of its own and writes the outcome back into the tray afterwards.
//! Opening the TUI is on that list too: choosing a terminal walks several directories and then
//! waits on the child, which is far too much for a DBus handler.

pub mod icon;
pub mod terminal;

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Duration;

use anyhow::{anyhow, Result};
use ksni::blocking::{Handle, TrayMethods};
use ksni::{
    menu::{MenuItem, StandardItem},
    Category, Icon, Status, Tray,
};

use crate::adb::{Device, SmartSocket};
use crate::service::{self, PortOwner};
use crate::ui::UnitState;

/// How often the icon and menu are refreshed from the adb server.
pub const REFRESH: Duration = Duration::from_secs(5);
/// The outcome line is one menu entry; adb's stderr is not.
pub const OUTCOME_MAX: usize = 120;

/// Is a phone usable right now? Only a wireless row in state `device` counts: `offline` is what
/// adb leaves behind after a drop, and `unauthorized` needs the user, not a reconnect.
pub fn attached(devices: &[Device]) -> bool {
    devices
        .iter()
        .any(|d| d.transport.is_wireless() && d.state == "device")
}

/// The first menu line, from the unit and the port owner directly, so it cannot say "not held
/// by wadb" when it is.
pub fn header(unit: &UnitState, owner: &PortOwner, port: u16) -> String {
    match (unit, owner) {
        (UnitState::NotInstalled, _) => "wadb is not installed".to_string(),
        (UnitState::Active, PortOwner::Ours(_)) => "Supervised".to_string(),
        (UnitState::Inactive, PortOwner::Ours(_)) => "Unit inactive, port ours".to_string(),
        (_, PortOwner::Foreign) => format!("Port {port} held by another adb"),
        (_, PortOwner::HeldUnknown) => format!("Port {port} held, owner unknown"),
        (UnitState::Active, PortOwner::Nobody) => {
            "adb server down (unit active, nothing listening)".to_string()
        }
        (UnitState::Inactive, PortOwner::Nobody) => "adb server down (unit inactive)".to_string(),
    }
}

fn device_count(devices: &[Device]) -> String {
    match devices.len() {
        0 => "no wireless devices".to_string(),
        1 => "1 wireless device".to_string(),
        n => format!("{n} wireless devices"),
    }
}

/// The text under the icon on hover.
/// Takes the header rather than deriving it, so that the hover says the same thing the menu does.
/// It used to call `header()` itself, which meant an action in flight showed "working…" in the
/// menu and the idle state on hover - two answers to one question.
pub fn tooltip(header: &str, devices: &[Device], outcome: Option<&str>) -> String {
    let mut text = format!("{header}\n{}", device_count(devices));
    if let Some(outcome) = outcome {
        text.push('\n');
        text.push_str(outcome);
    }
    text
}

/// One menu line per device: model, then how it is attached.
pub fn device_label(device: &Device) -> String {
    let serial = crate::ui::devices::short_serial(&device.serial);
    let head = match &device.model {
        Some(model) => format!("{model}   {serial}"),
        None => serial,
    };
    if device.state == "device" {
        head
    } else {
        format!("{head}  ({})", device.state)
    }
}

/// One line, whitespace collapsed, short enough for a menu. adb's stderr and chained errors are
/// neither.
pub fn outcome_line(text: &str) -> String {
    let first = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or_default();
    let collapsed = first.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > OUTCOME_MAX {
        let mut cut: String = collapsed.chars().take(OUTCOME_MAX).collect();
        cut.push('…');
        cut
    } else {
        collapsed
    }
}

/// Open the TUI, which is where pairing lives: the QR needs a text grid, and a menu cannot draw
/// one. Returns a note when there is something the user should be told about how it opened.
fn open_tui() -> Result<Option<String>> {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("wadb"));
    terminal::open(&exe)
}

/// One `daemon::tick` in a line a menu can show.
///
/// 0/0 says no more than "nothing to reconnect" on purpose: `tick` returns it both when every
/// advertised phone is already attached and when the mDNS browse found nothing at all because the
/// phone is off, and `Outcome` cannot tell the two apart.
pub fn reconnect_outcome(result: &Result<crate::daemon::Outcome>) -> String {
    let outcome = match result {
        Err(e) => return format!("reconnect failed: {e:#}"),
        Ok(outcome) => outcome,
    };
    let (connected, failed) = (outcome.connected.len(), outcome.failed.len());
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    match (connected, failed) {
        (0, 0) => "nothing to reconnect".to_string(),
        (n, 0) => format!("reconnected {n} device{}", plural(n)),
        // `failed` holds "{endpoint}: {error}", not serials.
        (0, m) => format!(
            "could not connect {m} device{}: {}",
            plural(m),
            outcome
                .failed
                .first()
                .map(String::as_str)
                .unwrap_or_default()
        ),
        (n, m) => format!("reconnected {n}, could not connect {m}"),
    }
}

/// What a menu click asks the run loop to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Reconnect,
    Takeover,
    /// Open the TUI. Not an adb action: it does not take `busy`, and two are allowed at once.
    Pair,
    Quit,
}

impl Action {
    /// Reconnect and Take over touch the adb server, so they run one at a time. Pair opens a
    /// terminal, which is neither exclusive nor slow enough to be worth blocking, and Quit must
    /// be honoured even mid-action.
    fn guarded(self) -> bool {
        matches!(self, Action::Reconnect | Action::Takeover)
    }
}

/// One refresh's worth of reading, gathered outside the tray lock so a slow server cannot
/// freeze the menu.
pub struct Snapshot {
    pub port: u16,
    pub adb: Option<PathBuf>,
    pub unit: UnitState,
    pub owner: PortOwner,
    /// `None` keeps the last known list: a device read that fails while the server is up is
    /// usually the server restarting, not an empty phone list.
    pub devices: Option<Vec<Device>>,
}

/// Read everything the icon and menu show. Same port and adb resolution as the CLI, re-read each
/// time, so an install from the tray-launched TUI is picked up.
pub fn snapshot() -> Snapshot {
    let port = crate::port();
    let adb = crate::adb_for_commands().ok();
    let unit = crate::ui::current_unit_state();
    // With no unit of ours there is nothing to report on: reading the port would list a
    // stranger's devices as though wadb were supervising them (the TUI does the same).
    if unit == UnitState::NotInstalled {
        return Snapshot {
            port,
            adb,
            unit,
            owner: PortOwner::Nobody,
            devices: Some(Vec::new()),
        };
    }
    let owner = service::port_owner_given(port, unit == UnitState::Active);
    let sock = SmartSocket::new(port);
    let devices = if sock.is_up() {
        sock.devices()
            .ok()
            .map(|all| crate::adb::wireless_devices(&all))
    } else {
        Some(Vec::new())
    };
    Snapshot {
        port,
        adb,
        unit,
        owner,
        devices,
    }
}

pub struct WadbTray {
    pub port: u16,
    pub adb: Option<PathBuf>,
    pub unit: UnitState,
    pub owner: PortOwner,
    pub devices: Vec<Device>,
    /// The last menu action's result, kept until the next action replaces it.
    pub outcome: Option<String>,
    /// An action is running. Reconnect and Take over are disabled and ignored meanwhile.
    pub busy: bool,
    actions: Sender<Action>,
}

impl WadbTray {
    pub fn new(actions: Sender<Action>) -> Self {
        Self {
            port: crate::adb::DEFAULT_PORT,
            adb: None,
            unit: UnitState::NotInstalled,
            owner: PortOwner::Nobody,
            devices: Vec::new(),
            outcome: None,
            busy: false,
            actions,
        }
    }

    /// Write a snapshot in. Never touches `busy` or `outcome`: a refresh landing between a click
    /// and the action's end must not let a second action in, or wipe a result nobody has read.
    pub fn apply(&mut self, snapshot: Snapshot) {
        self.port = snapshot.port;
        self.adb = snapshot.adb;
        self.unit = snapshot.unit;
        self.owner = snapshot.owner;
        if let Some(devices) = snapshot.devices {
            self.devices = devices;
        }
    }

    pub fn attached(&self) -> bool {
        attached(&self.devices)
    }

    /// Hand an action to the run loop. A second Reconnect or Take over while one runs is ignored,
    /// not queued. Pair and Quit are always sent: Pair is not an adb action, and swallowing it
    /// would mean a click that does nothing at all for the half-minute a reconnect can take.
    pub fn request(&mut self, action: Action) {
        if action.guarded() && self.busy {
            return;
        }
        // Set only once the send has succeeded. A dropped receiver means the run loop is gone, and
        // latching `busy` on the way out would leave the menu permanently disabled.
        if self.actions.send(action).is_ok() && action.guarded() {
            self.busy = true;
        }
    }

    fn header(&self) -> String {
        if self.busy {
            "working…".to_string()
        } else {
            header(&self.unit, &self.owner, self.port)
        }
    }

    /// What a finished Pair writes: the outcome line, and only when there is something to say.
    /// Never `busy`, which it never set, and never a snapshot, because opening a terminal changes
    /// no adb state. A silent success leaves the previous outcome alone, so opening a terminal
    /// does not wipe a reconnect result the user has not read yet.
    fn finish_pair(&mut self, note: Option<String>) {
        if let Some(note) = note {
            self.outcome = Some(outcome_line(&note));
        }
    }

    fn takeover_offered(&self) -> bool {
        matches!(self.owner, PortOwner::Foreign | PortOwner::HeldUnknown)
            && self.unit != UnitState::NotInstalled
    }
}

fn disabled_line<T>(label: String) -> MenuItem<T> {
    StandardItem {
        label,
        enabled: false,
        ..Default::default()
    }
    .into()
}

impl Tray for WadbTray {
    /// A left click opens the menu. Without this a left click does nothing and the user has to
    /// guess at the right button.
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        "wadb".into()
    }

    fn title(&self) -> String {
        "wadb".into()
    }

    /// Empty, and it has to stay empty. A StatusNotifierItem host prefers `IconName` whenever
    /// its theme resolves the name, so any theme name here would win over the drawn pixmap — and
    /// the names this once used are the panel's own Wi-Fi glyphs.
    fn icon_name(&self) -> String {
        String::new()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        icon::pixmaps(self.attached())
    }

    /// The item reports on an attached device, not on communications or a system service.
    fn category(&self) -> Category {
        Category::Hardware
    }

    fn status(&self) -> Status {
        // Always Active: XFCE hides Passive items, and an icon that hides when things are fine
        // cannot be glanced at.
        Status::Active
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "wadb".into(),
            description: tooltip(&self.header(), &self.devices, self.outcome.as_deref()),
            // Empty for the same reason as `icon_name` above: this is the second place a theme
            // name would beat the pixmap.
            icon_name: String::new(),
            icon_pixmap: icon::pixmaps(self.attached()),
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items: Vec<MenuItem<Self>> = Vec::new();

        items.push(disabled_line(self.header()));
        if let Some(outcome) = &self.outcome {
            items.push(disabled_line(outcome.clone()));
        }
        items.push(MenuItem::Separator);

        if self.devices.is_empty() {
            items.push(disabled_line("No wireless devices".into()));
        }
        for device in &self.devices {
            items.push(disabled_line(device_label(device)));
        }

        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: "Open wadb to pair…".into(),
                activate: Box::new(|tray: &mut Self| tray.request(Action::Pair)),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: "Reconnect now".into(),
                enabled: matches!(self.owner, PortOwner::Ours(_)) && !self.busy,
                activate: Box::new(|tray: &mut Self| tray.request(Action::Reconnect)),
                ..Default::default()
            }
            .into(),
        );
        if self.takeover_offered() {
            let (label, enabled) = if self.adb.is_some() {
                ("Take over the port".to_string(), !self.busy)
            } else {
                ("Take over the port (adb not found)".to_string(), false)
            };
            items.push(
                StandardItem {
                    label,
                    enabled,
                    activate: Box::new(|tray: &mut Self| tray.request(Action::Takeover)),
                    ..Default::default()
                }
                .into(),
            );
        }

        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|tray: &mut Self| tray.request(Action::Quit)),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}

/// One Reconnect, as the daemon would do it, with a fresh failures map: a click is a human
/// saying "try again now", so no backoff from earlier clicks applies.
fn reconnect(port: u16) -> String {
    if !matches!(service::port_owner(port), PortOwner::Ours(_)) {
        return "port is no longer ours".to_string();
    }
    let mut failures = HashMap::new();
    reconnect_outcome(&crate::daemon::tick(port, &mut failures))
}

fn takeover(adb: Option<&PathBuf>, port: u16) -> String {
    match adb {
        None => "adb not found".to_string(),
        Some(adb) => match service::takeover(adb, port) {
            Ok(lines) => lines.join(" "),
            Err(e) => format!("{e:#}"),
        },
    }
}

/// Open the TUI, off the DBus thread. It writes no state and clears no flag: it is not an adb
/// action, so nothing it does changes the unit, the port or the device list. It writes the
/// outcome line only when it fails, so opening a terminal does not wipe a reconnect result the
/// user has not read yet.
fn perform_pair(handle: Handle<WadbTray>) {
    std::thread::spawn(move || {
        let note = match open_tui() {
            Ok(note) => note,
            Err(e) => {
                eprintln!("wadb: {e:#}");
                Some(format!("{e:#}"))
            }
        };
        if note.is_some() {
            handle.update(|tray| tray.finish_pair(note));
        }
    });
}

/// Run one adb action on its own thread, then write the fresh state and the outcome back in one
/// update. `busy` clears here and nowhere else.
fn perform(action: Action, handle: Handle<WadbTray>) {
    std::thread::spawn(move || {
        // The snapshot is inside the guard, not just the action: it shells out to systemctl and
        // ss, so it is at least as likely to panic as the action is, and a panic on either side
        // would leave `busy` set and the menu dead for the life of the process.
        let done = std::panic::catch_unwind(AssertUnwindSafe(|| {
            // Resolved now, not when the menu was built: up to a refresh can pass between the two.
            let port = crate::port();
            let adb = crate::adb_for_commands().ok();
            let outcome = match action {
                Action::Reconnect => reconnect(port),
                Action::Takeover => takeover(adb.as_ref(), port),
                Action::Pair | Action::Quit => unreachable!("not an adb action"),
            };
            (snapshot(), outcome)
        }));
        // `None` from update() means the service is gone — Quit, or the bus went away while the
        // action ran. Return quietly rather than panicking on it.
        let _ = match done {
            Ok((snapshot, outcome)) => handle.update(|tray| {
                tray.apply(snapshot);
                tray.outcome = Some(outcome_line(&outcome));
                tray.busy = false;
            }),
            // Outcome-only on this path: the snapshot is what failed, so there is none to apply.
            Err(panic) => handle.update(|tray| {
                tray.outcome = Some(outcome_line(&format!(
                    "action failed: {}",
                    panic_msg(&panic)
                )));
                tray.busy = false;
            }),
        };
    });
}

fn panic_msg(panic: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "panicked".to_string()
    }
}

/// Run the tray until the user quits it.
pub fn run() -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let mut tray = WadbTray::new(tx);
    // Guarded like the other two calls. This one is before the item exists, so a panic here would
    // abort the process with a backtrace rather than the one-line error every other startup
    // failure produces.
    let first = std::panic::catch_unwind(snapshot)
        .map_err(|p| anyhow!("could not read the adb server: {}", panic_msg(&p)))?;
    tray.apply(first);
    // A missing StatusNotifierWatcher at start is not fatal: the item registers when the panel
    // comes up, so this can be launched from a session autostart before the panel.
    let handle = tray
        .assume_sni_available(true)
        .spawn()
        .map_err(|e| anyhow!("could not register a tray icon: {e}"))?;
    loop {
        match rx.recv_timeout(REFRESH) {
            Ok(Action::Quit) => {
                handle.shutdown().wait();
                return Ok(());
            }
            Ok(Action::Pair) => perform_pair(handle.clone()),
            Ok(action) => perform(action, handle.clone()),
            Err(RecvTimeoutError::Timeout) => {
                // Guarded for the same reason as the action thread's, and a stronger one: a panic
                // here unwinds out of run() and the icon leaves the panel altogether, which is
                // worse than any stale reading. On a panic, keep what is on screen and say so.
                let refreshed = std::panic::catch_unwind(AssertUnwindSafe(snapshot));
                let update = match refreshed {
                    Ok(snapshot) => handle.update(|tray| tray.apply(snapshot)),
                    Err(panic) => handle.update(|tray| {
                        tray.outcome = Some(outcome_line(&format!(
                            "refresh failed: {}",
                            panic_msg(&panic)
                        )));
                    }),
                };
                // None: the service is gone, which means the session bus is. Same as Quit.
                if update.is_none() {
                    return Ok(());
                }
            }
            // The tray, and its sender, were dropped with the service.
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adb::parse_devices;
    use std::sync::mpsc::Receiver;

    const UNITS: [UnitState; 3] = [
        UnitState::NotInstalled,
        UnitState::Active,
        UnitState::Inactive,
    ];

    fn owners() -> [PortOwner; 4] {
        [
            PortOwner::Ours(42),
            PortOwner::Foreign,
            PortOwner::HeldUnknown,
            PortOwner::Nobody,
        ]
    }

    fn tray() -> (WadbTray, Receiver<Action>) {
        let (tx, rx) = mpsc::channel();
        let mut tray = WadbTray::new(tx);
        tray.port = 5037;
        tray.adb = Some(PathBuf::from("/opt/sdk/platform-tools/adb"));
        tray.unit = UnitState::Active;
        tray.owner = PortOwner::Ours(42);
        (tray, rx)
    }

    fn standard<'a>(
        items: &'a [MenuItem<WadbTray>],
        prefix: &str,
    ) -> Option<&'a StandardItem<WadbTray>> {
        items.iter().find_map(|item| match item {
            MenuItem::Standard(s) if s.label.starts_with(prefix) => Some(s),
            _ => None,
        })
    }

    #[test]
    fn the_header_covers_every_pair() {
        for unit in &UNITS {
            for owner in owners() {
                let text = header(unit, &owner, 5137);
                assert!(!text.is_empty());
                match (unit, &owner) {
                    (UnitState::NotInstalled, _) => assert_eq!(text, "wadb is not installed"),
                    (UnitState::Active, PortOwner::Ours(_)) => assert_eq!(text, "Supervised"),
                    (UnitState::Inactive, PortOwner::Ours(_)) => {
                        assert_eq!(text, "Unit inactive, port ours")
                    }
                    (_, PortOwner::Foreign) => {
                        assert_eq!(text, "Port 5137 held by another adb")
                    }
                    (_, PortOwner::HeldUnknown) => {
                        assert_eq!(text, "Port 5137 held, owner unknown")
                    }
                    (UnitState::Active, PortOwner::Nobody) => {
                        assert_eq!(text, "adb server down (unit active, nothing listening)")
                    }
                    (UnitState::Inactive, PortOwner::Nobody) => {
                        assert_eq!(text, "adb server down (unit inactive)")
                    }
                }
            }
        }
    }

    #[test]
    fn attached_means_a_wireless_row_in_state_device() {
        assert!(!attached(&[]));
        let offline = parse_devices("adb-X-y._adb-tls-connect._tcp offline model:Tab_S9\n");
        assert!(!attached(&offline), "offline is what a drop leaves behind");
        let unauthorized = parse_devices("192.168.86.45:42595 unauthorized model:Pixel_8a\n");
        assert!(
            !attached(&unauthorized),
            "unauthorized needs the user, not a reconnect"
        );
        let usb = parse_devices("3A081FDJH00123 device model:Pixel_8a\n");
        assert!(
            !attached(&usb),
            "a USB phone must not light the wireless icon"
        );
        let wireless = parse_devices("192.168.86.45:42595 device model:Pixel_8a\n");
        assert!(attached(&wireless));
    }

    #[test]
    fn no_icon_name_is_ever_offered() {
        // A StatusNotifierItem host prefers IconName over IconPixmap whenever its theme resolves
        // the name, so a name here would beat the drawn phone. This is the assertion that keeps
        // the panel's own Wi-Fi glyph off the tray.
        let (mut tray, _rx) = tray();
        tray.devices = parse_devices("192.168.86.45:42595 device model:Pixel_8a\n");
        assert_eq!(tray.icon_name(), "");
        assert_eq!(tray.tool_tip().icon_name, "");
        assert!(!tray.icon_pixmap().is_empty());
        assert!(!tray.tool_tip().icon_pixmap.is_empty());
    }

    #[test]
    fn the_icon_follows_whether_a_phone_is_attached() {
        let bytes = |t: &WadbTray| {
            t.icon_pixmap()
                .into_iter()
                .map(|i| i.data)
                .collect::<Vec<_>>()
        };
        let (mut tray, _rx) = tray();
        let detached = bytes(&tray);
        tray.devices = parse_devices("192.168.86.45:42595 device model:Pixel_8a\n");
        assert_ne!(bytes(&tray), detached);
    }

    #[test]
    fn the_tooltip_counts_devices_in_words_that_agree() {
        let one = parse_devices("192.168.86.45:42595 device model:Pixel_8a\n");
        let two = parse_devices(
            "192.168.86.45:42595 device model:Pixel_8a\n192.168.86.99:41000 device model:Tab_S9\n",
        );
        let head = header(&UnitState::Active, &PortOwner::Ours(1), 5037);
        assert!(tooltip(&head, &[], None).ends_with("no wireless devices"));
        assert!(tooltip(&head, &one, None).ends_with("1 wireless device"));
        assert!(tooltip(&head, &two, None).ends_with("2 wireless devices"));
        assert!(tooltip(&head, &two, None).starts_with("Supervised\n"));
        let with = tooltip(&head, &two, Some("reconnected 1 device"));
        assert!(with.ends_with("\nreconnected 1 device"));
    }

    #[test]
    fn the_hover_says_what_the_menu_says_while_an_action_runs() {
        let (mut tray, _rx) = tray();
        tray.busy = true;
        assert!(tray.tool_tip().description.starts_with("working…"));
    }

    #[test]
    fn device_labels_name_the_model_and_flag_a_bad_state() {
        let devices = parse_devices(
            "192.168.86.45:42595 device model:Pixel_8a\nadb-X-y._adb-tls-connect._tcp offline model:Tab_S9\n192.168.86.7:5555 device\n",
        );
        assert_eq!(device_label(&devices[0]), "Pixel_8a   192.168.86.45:42595");
        // An offline device must not look identical to a healthy one.
        assert!(device_label(&devices[1]).contains("(offline)"));
        assert!(!device_label(&devices[1]).contains("_adb-tls-connect._tcp"));
        assert_eq!(
            device_label(&devices[2]),
            "192.168.86.7:5555",
            "no model: the serial alone"
        );
    }

    #[test]
    fn outcome_line_is_one_short_line() {
        assert_eq!(
            outcome_line("adb kill-server failed: cannot connect\n* daemon not running\n"),
            "adb kill-server failed: cannot connect"
        );
        assert_eq!(outcome_line("\n  spaced   out\ttext \n"), "spaced out text");
        let long = "x".repeat(OUTCOME_MAX + 50);
        let cut = outcome_line(&long);
        assert!(cut.ends_with('…'));
        assert_eq!(cut.chars().count(), OUTCOME_MAX + 1);
        assert_eq!(outcome_line(""), "");
    }

    #[test]
    fn a_long_multibyte_line_is_cut_on_a_character_not_a_byte() {
        // The reason the cut counts chars(): a byte index landing mid-character panics, and this
        // runs inside a DBus handler, where a panic takes the icon off the panel. A phone whose
        // model name is not ASCII is an ordinary thing, not a corner case.
        let long = "é".repeat(OUTCOME_MAX + 50);
        let cut = outcome_line(&long);
        assert_eq!(cut.chars().count(), OUTCOME_MAX + 1);
        assert!(cut.ends_with('…'));
        assert!(
            cut.len() > cut.chars().count(),
            "the fixture must be multi-byte"
        );
        // Exactly at the limit, nothing is cut and nothing is appended.
        let exact = "日".repeat(OUTCOME_MAX);
        assert_eq!(outcome_line(&exact), exact);
    }

    #[test]
    fn menu_offers_takeover_only_for_a_held_port_with_a_unit() {
        let (mut tray, _rx) = tray();
        assert!(
            standard(&tray.menu(), "Take over").is_none(),
            "ours: nothing to take"
        );

        tray.owner = PortOwner::Foreign;
        let menu = tray.menu();
        let item = standard(&menu, "Take over").expect("foreign: offered");
        assert!(item.enabled);
        assert_eq!(item.label, "Take over the port");

        tray.owner = PortOwner::HeldUnknown;
        assert!(standard(&tray.menu(), "Take over").is_some());

        tray.adb = None;
        let menu = tray.menu();
        let item = standard(&menu, "Take over").unwrap();
        assert!(!item.enabled);
        assert_eq!(item.label, "Take over the port (adb not found)");

        tray.adb = Some(PathBuf::from("/x/adb"));
        tray.unit = UnitState::NotInstalled;
        assert!(
            standard(&tray.menu(), "Take over").is_none(),
            "no unit: it would only kill the user's other server"
        );
    }

    #[test]
    fn reconnect_is_enabled_only_when_the_port_is_ours_and_idle() {
        let (mut tray, _rx) = tray();
        assert!(standard(&tray.menu(), "Reconnect now").unwrap().enabled);
        tray.owner = PortOwner::Foreign;
        assert!(!standard(&tray.menu(), "Reconnect now").unwrap().enabled);
        tray.owner = PortOwner::Ours(42);
        tray.busy = true;
        assert!(!standard(&tray.menu(), "Reconnect now").unwrap().enabled);
        assert_eq!(standard(&tray.menu(), "working").unwrap().label, "working…");
        tray.owner = PortOwner::Foreign;
        assert!(!standard(&tray.menu(), "Take over").unwrap().enabled);
    }

    #[test]
    fn outcome_line_shows_only_when_set() {
        let (mut tray, _rx) = tray();
        assert!(standard(&tray.menu(), "reconnected").is_none());
        tray.outcome = Some("reconnected 1 device".into());
        let menu = tray.menu();
        let item = standard(&menu, "reconnected").unwrap();
        assert!(!item.enabled);
        // A refresh must not clear it.
        tray.apply(Snapshot {
            port: 5037,
            adb: None,
            unit: UnitState::Active,
            owner: PortOwner::Ours(42),
            devices: None,
        });
        assert!(tray.outcome.is_some());
    }

    #[test]
    fn activate_sends_the_action_and_marks_busy_without_doing_the_work() {
        let (mut tray, rx) = tray();
        let menu = tray.menu();
        (standard(&menu, "Reconnect now").unwrap().activate)(&mut tray);
        assert_eq!(rx.try_recv(), Ok(Action::Reconnect));
        assert!(tray.busy);

        // While busy, a second click neither queues nor replaces anything.
        tray.owner = PortOwner::Foreign;
        let menu = tray.menu();
        (standard(&menu, "Take over").unwrap().activate)(&mut tray);
        (standard(&menu, "Reconnect now").unwrap().activate)(&mut tray);
        assert!(rx.try_recv().is_err(), "ignored while busy");

        // Quit is exempt.
        (standard(&menu, "Quit").unwrap().activate)(&mut tray);
        assert_eq!(rx.try_recv(), Ok(Action::Quit));
    }

    #[test]
    fn pair_is_exempt_from_busy_in_both_directions() {
        // Both halves matter, and satisfying only one breaks something. If Pair *set* `busy` it
        // would disable Reconnect and show "working…" for opening a terminal. If Pair were
        // *ignored* while busy, a click during a reconnect - the half-minute when a user is most
        // likely to reach for pairing - would do nothing at all, silently.
        let (mut tray, rx) = tray();
        let menu = tray.menu();
        (standard(&menu, "Open wadb to pair").unwrap().activate)(&mut tray);
        assert_eq!(rx.try_recv(), Ok(Action::Pair));
        assert!(!tray.busy, "opening a terminal is not an adb action");

        tray.busy = true;
        let menu = tray.menu();
        (standard(&menu, "Open wadb to pair").unwrap().activate)(&mut tray);
        assert_eq!(rx.try_recv(), Ok(Action::Pair), "sent even while busy");
        assert!(tray.busy, "and it must not clear the flag either");
    }

    #[test]
    fn a_finished_pair_writes_the_outcome_and_nothing_else() {
        let (mut tray, _rx) = tray();
        tray.busy = true;
        tray.devices = parse_devices("192.168.86.45:42595 device model:Pixel_8a\n");
        tray.outcome = Some("reconnected 1 device".into());

        // A silent success must not wipe a result the user has not read yet.
        tray.finish_pair(None);
        assert_eq!(tray.outcome.as_deref(), Some("reconnected 1 device"));

        // Anything worth saying replaces it - and still touches nothing else. If Pair cleared
        // `busy` here it would re-enable Reconnect while one was still running.
        tray.finish_pair(Some("no terminal found; set $TERMINAL".into()));
        assert_eq!(
            tray.outcome.as_deref(),
            Some("no terminal found; set $TERMINAL")
        );
        assert!(tray.busy, "Pair never set busy, so it must never clear it");
        assert_eq!(tray.devices.len(), 1, "and it applies no snapshot");
    }

    #[test]
    fn a_dropped_run_loop_does_not_latch_the_menu_shut() {
        // `busy` is set only once the send has succeeded. A dropped receiver means the run loop is
        // gone; latching the flag on the way out would leave the menu permanently disabled.
        let (mut tray, rx) = tray();
        drop(rx);
        tray.request(Action::Reconnect);
        assert!(!tray.busy);
    }

    #[test]
    fn reconnect_outcome_says_what_actually_happened() {
        use crate::daemon::Outcome;
        let outcome = |connected: &[&str], failed: &[&str]| {
            Ok(Outcome {
                connected: connected.iter().map(|s| s.to_string()).collect(),
                failed: failed.iter().map(|s| s.to_string()).collect(),
            })
        };
        // 0/0 says no more than this on purpose: `tick` returns it both when every advertised
        // phone is already attached and when the browse found nothing because the phone is off,
        // and Outcome cannot tell the two apart. The older, friendlier wording was a lie in the
        // commoner of the two cases.
        assert_eq!(
            reconnect_outcome(&outcome(&[], &[])),
            "nothing to reconnect"
        );
        assert_eq!(
            reconnect_outcome(&outcome(&["a"], &[])),
            "reconnected 1 device"
        );
        assert_eq!(
            reconnect_outcome(&outcome(&["a", "b"], &[])),
            "reconnected 2 devices"
        );
        // `failed` holds "{endpoint}: {error}", not serials.
        assert_eq!(
            reconnect_outcome(&outcome(&[], &["192.168.86.45:42595: timed out"])),
            "could not connect 1 device: 192.168.86.45:42595: timed out"
        );
        assert_eq!(
            reconnect_outcome(&outcome(&["a"], &["b: x", "c: y"])),
            "reconnected 1, could not connect 2"
        );
        assert_eq!(
            reconnect_outcome(&Err(anyhow!("mDNS browse failed"))),
            "reconnect failed: mDNS browse failed"
        );
    }

    #[test]
    fn a_refresh_never_clears_busy_and_keeps_the_last_list_on_a_failed_read() {
        let (mut tray, _rx) = tray();
        tray.devices = parse_devices("192.168.86.45:42595 device model:Pixel_8a\n");
        tray.busy = true;
        tray.apply(Snapshot {
            port: 5137,
            adb: None,
            unit: UnitState::Inactive,
            owner: PortOwner::Nobody,
            devices: None,
        });
        assert!(tray.busy);
        assert_eq!(tray.port, 5137);
        assert_eq!(tray.devices.len(), 1, "a failed read keeps the last list");
        tray.apply(Snapshot {
            port: 5137,
            adb: None,
            unit: UnitState::Inactive,
            owner: PortOwner::Nobody,
            devices: Some(Vec::new()),
        });
        assert!(tray.devices.is_empty(), "a server that is down clears it");
    }
}
