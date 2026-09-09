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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Duration;

use anyhow::{anyhow, Result};
use ksni::blocking::{Handle, TrayMethods};
use ksni::{
    menu::{MenuItem, StandardItem},
    Icon, Status, Tray,
};

use crate::adb::{Device, SmartSocket};
use crate::service::{self, PortOwner};
use crate::ui::UnitState;

/// How often the icon and menu are refreshed from the adb server.
pub const REFRESH: Duration = Duration::from_secs(5);
/// The outcome line is one menu entry; adb's stderr is not.
pub const OUTCOME_MAX: usize = 120;

/// What the icon should say at a glance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// Supervised by our unit, and the port is ours.
    Supervised,
    /// Something is up but not ours, or the unit is not running.
    Degraded,
    /// No server, or nothing installed.
    Down,
}

pub fn health(unit: &UnitState, owner: &PortOwner) -> Health {
    match (unit, owner) {
        (UnitState::Active, PortOwner::Ours(_)) => Health::Supervised,
        (UnitState::NotInstalled, _) | (_, PortOwner::Nobody) => Health::Down,
        _ => Health::Degraded,
    }
}

/// Is a phone usable right now? Only a wireless row in state `device` counts: `offline` is what
/// adb leaves behind after a drop, and `unauthorized` needs the user, not a reconnect.
pub fn attached(devices: &[Device]) -> bool {
    devices
        .iter()
        .any(|d| d.transport.is_wireless() && d.state == "device")
}

/// Themed icon names, so the panel picks the user's own icon theme rather than a bundled bitmap.
/// The `-symbolic` variants are the ones Adwaita and Yaru actually ship.
pub fn icon_name(health: Health, attached: bool) -> &'static str {
    if attached {
        return "network-wireless-signal-excellent-symbolic";
    }
    match health {
        Health::Supervised => "network-wireless-signal-none-symbolic",
        Health::Degraded => "network-wireless-acquiring-symbolic",
        Health::Down => "network-wireless-offline-symbolic",
    }
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
pub fn tooltip(
    unit: &UnitState,
    owner: &PortOwner,
    port: u16,
    devices: &[Device],
    outcome: Option<&str>,
) -> String {
    let mut text = format!("{}\n{}", header(unit, owner, port), device_count(devices));
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

/// `$TERMINAL` may carry arguments ("alacritty --command"): the program, then the rest.
pub fn split_terminal(spec: &str) -> Option<(String, Vec<String>)> {
    let mut words = spec.split_whitespace().map(str::to_string);
    let program = words.next()?;
    Some((program, words.collect()))
}

fn terminal_candidates() -> Vec<(String, Vec<String>)> {
    let mut candidates = Vec::new();
    if let Some(spec) = std::env::var("TERMINAL")
        .ok()
        .and_then(|s| split_terminal(&s))
    {
        candidates.push(spec);
    }
    for name in ["x-terminal-emulator", "kitty", "xterm"] {
        candidates.push((name.to_string(), Vec::new()));
    }
    candidates
}

/// Open the TUI in a terminal, which is where pairing lives: the QR needs a text grid, and a menu
/// cannot draw one. `<terminal> [args] -e <this binary>` is assumed to work, as it does for
/// xterm, kitty, and the Debian alternatives wrapper.
fn open_tui() -> Result<()> {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("wadb"));
    for (program, args) in terminal_candidates() {
        let spawned = std::process::Command::new(&program)
            .args(&args)
            .arg("-e")
            .arg(&exe)
            .spawn();
        if let Ok(mut child) = spawned {
            // Reap it, or a closed terminal leaves a zombie for the life of the tray. Its exit
            // status says nothing: a terminal the user closes can exit non-zero too.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            return Ok(());
        }
    }
    Err(anyhow!("no terminal found; set $TERMINAL"))
}

/// What a menu click asks the run loop to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Reconnect,
    Takeover,
    Quit,
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

    pub fn health(&self) -> Health {
        health(&self.unit, &self.owner)
    }

    pub fn attached(&self) -> bool {
        attached(&self.devices)
    }

    /// Hand an action to the run loop. A second Reconnect or Take over while one runs is ignored,
    /// not queued; Quit is always honoured.
    pub fn request(&mut self, action: Action) {
        if action != Action::Quit {
            if self.busy {
                return;
            }
            self.busy = true;
        }
        let _ = self.actions.send(action);
    }

    fn header(&self) -> String {
        if self.busy {
            "working…".to_string()
        } else {
            header(&self.unit, &self.owner, self.port)
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

    fn icon_name(&self) -> String {
        icon_name(self.health(), self.attached()).into()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        Vec::new()
    }

    fn status(&self) -> Status {
        // Always Active: XFCE hides Passive items, and an icon that hides when things are fine
        // cannot be glanced at.
        Status::Active
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "wadb".into(),
            description: tooltip(
                &self.unit,
                &self.owner,
                self.port,
                &self.devices,
                self.outcome.as_deref(),
            ),
            icon_name: icon_name(self.health(), self.attached()).into(),
            icon_pixmap: Vec::new(),
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
                activate: Box::new(|tray: &mut Self| {
                    if let Err(e) = open_tui() {
                        eprintln!("wadb: {e}");
                        tray.outcome = Some(outcome_line(&e.to_string()));
                    }
                }),
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
    match crate::daemon::tick(port, &mut failures) {
        Ok(outcome) => {
            let connected = outcome.connected.len();
            let failed = outcome.failed.len();
            match (connected, failed) {
                (0, 0) => "nothing to reconnect".to_string(),
                (n, 0) => format!("reconnected {n} device{}", if n == 1 { "" } else { "s" }),
                (n, m) => format!("reconnected {n}, {m} failed: {}", outcome.failed.join("; ")),
            }
        }
        Err(e) => format!("reconnect failed: {e:#}"),
    }
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

/// Run one action on its own thread, then write the fresh state and the outcome back in one
/// update. `busy` clears here and nowhere else.
fn perform(action: Action, handle: Handle<WadbTray>) {
    std::thread::spawn(move || {
        // Resolved now, not when the menu was built: up to a refresh can pass between the two.
        let port = crate::port();
        let adb = crate::adb_for_commands().ok();
        let outcome = match action {
            Action::Reconnect => reconnect(port),
            Action::Takeover => takeover(adb.as_ref(), port),
            Action::Quit => return,
        };
        let snapshot = snapshot();
        handle.update(|tray| {
            tray.apply(snapshot);
            tray.outcome = Some(outcome_line(&outcome));
            tray.busy = false;
        });
    });
}

/// Run the tray until the user quits it.
pub fn run() -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let mut tray = WadbTray::new(tx);
    tray.apply(snapshot());
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
            Ok(action) => perform(action, handle.clone()),
            Err(RecvTimeoutError::Timeout) => {
                let snapshot = snapshot();
                // None: the service is gone, which means the session bus is. Same as Quit.
                if handle.update(|tray| tray.apply(snapshot)).is_none() {
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
    fn health_and_header_cover_every_pair() {
        for unit in &UNITS {
            for owner in owners() {
                let h = health(unit, &owner);
                let expected = match (unit, &owner) {
                    (UnitState::Active, PortOwner::Ours(_)) => Health::Supervised,
                    (UnitState::NotInstalled, _) | (_, PortOwner::Nobody) => Health::Down,
                    _ => Health::Degraded,
                };
                assert_eq!(h, expected, "{unit:?} {owner:?}");

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
        let usb = parse_devices("3A081FDJH00123 device model:Pixel_8a\n");
        assert!(
            !attached(&usb),
            "a USB phone must not light the wireless icon"
        );
        let wireless = parse_devices("192.168.86.45:42595 device model:Pixel_8a\n");
        assert!(attached(&wireless));
    }

    #[test]
    fn four_icons_and_attached_wins() {
        let names: std::collections::HashSet<&str> = [
            icon_name(Health::Supervised, true),
            icon_name(Health::Supervised, false),
            icon_name(Health::Degraded, false),
            icon_name(Health::Down, false),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            names.len(),
            4,
            "a state that looks like another is not worth showing"
        );
        assert_eq!(
            icon_name(Health::Degraded, true),
            icon_name(Health::Supervised, true),
            "a phone on a foreign server is still a phone"
        );
        assert!(names.iter().all(|n| n.ends_with("-symbolic")));
    }

    #[test]
    fn the_tooltip_counts_devices_in_words_that_agree() {
        let one = parse_devices("192.168.86.45:42595 device model:Pixel_8a\n");
        let two = parse_devices(
            "192.168.86.45:42595 device model:Pixel_8a\n192.168.86.99:41000 device model:Tab_S9\n",
        );
        let (unit, owner) = (UnitState::Active, PortOwner::Ours(1));
        assert!(tooltip(&unit, &owner, 5037, &[], None).ends_with("no wireless devices"));
        assert!(tooltip(&unit, &owner, 5037, &one, None).ends_with("1 wireless device"));
        assert!(tooltip(&unit, &owner, 5037, &two, None).ends_with("2 wireless devices"));
        assert!(tooltip(&unit, &owner, 5037, &two, None).starts_with("Supervised\n"));
        let with = tooltip(&unit, &owner, 5037, &two, Some("reconnected 1 device"));
        assert!(with.ends_with("\nreconnected 1 device"));
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
    fn terminal_spec_splits_into_program_and_arguments() {
        assert_eq!(
            split_terminal("alacritty --command"),
            Some(("alacritty".into(), vec!["--command".into()]))
        );
        assert_eq!(split_terminal("kitty"), Some(("kitty".into(), vec![])));
        assert_eq!(split_terminal("   "), None);
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
