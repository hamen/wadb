// SPDX-License-Identifier: Apache-2.0

//! A status-bar icon, for the panel rather than the terminal.
//!
//! This speaks StatusNotifierItem over DBus, which is what XFCE's tray plugin, KDE and the GNOME
//! AppIndicator extension all consume. It is the same shape as the macOS menu-bar app this project
//! is modelled on: an icon that shows whether the server is supervised, and a menu listing the
//! wireless devices currently attached.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use ksni::blocking::TrayMethods;
use ksni::{
    menu::{MenuItem, StandardItem},
    Icon, Status, Tray,
};

use crate::adb::{Device, SmartSocket};
use crate::service::{self, PortOwner};
use crate::ui::UnitState;

/// How often the icon and menu are refreshed from the adb server.
pub const REFRESH: Duration = Duration::from_secs(3);

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

/// Themed icon names, so the panel picks the user's own icon theme rather than a bundled bitmap.
pub fn icon_name(health: Health) -> &'static str {
    match health {
        Health::Supervised => "network-wireless",
        Health::Degraded => "network-wireless-acquiring",
        Health::Down => "network-wireless-offline",
    }
}

/// The line under the icon on hover.
pub fn tooltip(health: Health, devices: &[Device], port: u16) -> String {
    let head = match health {
        Health::Supervised => format!("supervising adb on port {port}"),
        Health::Degraded => format!("port {port} is not held by wadb"),
        Health::Down => "adb server is down".to_string(),
    };
    match devices.len() {
        0 => format!("{head}\nno wireless devices"),
        1 => format!("{head}\n1 wireless device"),
        n => format!("{head}\n{n} wireless devices"),
    }
}

/// One menu line per device: model, then how it is attached.
pub fn device_label(device: &Device) -> String {
    let model = device.model.clone().unwrap_or_else(|| "unknown".into());
    let serial = crate::ui::devices::short_serial(&device.serial);
    if device.state == "device" {
        format!("{model}   {serial}")
    } else {
        format!("{model}   {serial}  ({})", device.state)
    }
}

pub struct WadbTray {
    pub port: u16,
    pub adb: Option<PathBuf>,
    pub unit: UnitState,
    pub owner: PortOwner,
    pub devices: Vec<Device>,
}

impl WadbTray {
    pub fn new(port: u16, adb: Option<PathBuf>) -> Self {
        let mut tray = Self {
            port,
            adb,
            unit: UnitState::NotInstalled,
            owner: PortOwner::Nobody,
            devices: Vec::new(),
        };
        tray.refresh();
        tray
    }

    /// Re-read everything from the adb server. Uses the smart socket, so a refresh cannot start a
    /// server the way `adb devices` would.
    pub fn refresh(&mut self) {
        self.unit = crate::ui::current_unit_state();
        if self.unit == UnitState::NotInstalled {
            self.owner = PortOwner::Nobody;
            self.devices.clear();
            return;
        }
        self.owner = service::port_owner(self.port);
        let sock = SmartSocket::new(self.port);
        self.devices = if sock.is_up() {
            sock.devices()
                .map(|all| crate::adb::wireless_devices(&all))
                .unwrap_or_default()
        } else {
            Vec::new()
        };
    }

    pub fn health(&self) -> Health {
        health(&self.unit, &self.owner)
    }

    /// Open the TUI in a terminal, which is where pairing lives: the QR needs a text grid, and a
    /// menu cannot draw one.
    fn open_tui(&self) {
        let terminal = std::env::var("TERMINAL").unwrap_or_else(|_| "xterm".into());
        for candidate in [terminal.as_str(), "kitty", "x-terminal-emulator", "xterm"] {
            let exe = std::env::current_exe()
                .unwrap_or_else(|_| PathBuf::from("wadb"))
                .display()
                .to_string();
            if std::process::Command::new(candidate)
                .arg("-e")
                .arg(&exe)
                .spawn()
                .is_ok()
            {
                return;
            }
        }
        eprintln!("wadb: could not find a terminal to open; set $TERMINAL");
    }
}

impl Tray for WadbTray {
    fn id(&self) -> String {
        "wadb".into()
    }

    fn title(&self) -> String {
        "wadb".into()
    }

    fn icon_name(&self) -> String {
        icon_name(self.health()).into()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        Vec::new()
    }

    fn status(&self) -> Status {
        match self.health() {
            // Amber and red states are worth keeping visible even in a crowded panel.
            Health::Supervised => Status::Passive,
            _ => Status::Active,
        }
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "wadb".into(),
            description: tooltip(self.health(), &self.devices, self.port),
            icon_name: icon_name(self.health()).into(),
            icon_pixmap: Vec::new(),
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items: Vec<MenuItem<Self>> = Vec::new();

        items.push(
            StandardItem {
                label: match self.health() {
                    Health::Supervised => "Supervised".into(),
                    Health::Degraded => format!("Port {} not held by wadb", self.port),
                    Health::Down => "adb server down".into(),
                },
                enabled: false,
                ..Default::default()
            }
            .into(),
        );
        items.push(MenuItem::Separator);

        if self.devices.is_empty() {
            items.push(
                StandardItem {
                    label: "No wireless devices".into(),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
        }
        for device in &self.devices {
            items.push(
                StandardItem {
                    label: device_label(device),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
        }

        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: "Pair a device…".into(),
                activate: Box::new(|tray: &mut Self| tray.open_tui()),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: "Reconnect now".into(),
                enabled: matches!(self.owner, PortOwner::Ours(_)),
                activate: Box::new(|tray: &mut Self| {
                    let mut failures = std::collections::HashMap::new();
                    let _ = crate::daemon::tick(tray.port, &mut failures);
                    tray.refresh();
                }),
                ..Default::default()
            }
            .into(),
        );
        if matches!(self.owner, PortOwner::Foreign | PortOwner::HeldUnknown) {
            items.push(
                StandardItem {
                    label: "Take over the port".into(),
                    activate: Box::new(|tray: &mut Self| {
                        if let Some(adb) = tray.adb.clone() {
                            let _ = std::process::Command::new(adb)
                                .env("ADB_SERVER_SOCKET", format!("tcp:127.0.0.1:{}", tray.port))
                                .arg("kill-server")
                                .output();
                            let _ = service::restart();
                        }
                        tray.refresh();
                    }),
                    ..Default::default()
                }
                .into(),
            );
        }

        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|_: &mut Self| std::process::exit(0)),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}

/// Run the tray until the user quits it.
pub fn run(port: u16, adb: Option<PathBuf>) -> Result<()> {
    let handle = WadbTray::new(port, adb)
        .spawn()
        .map_err(|e| anyhow::anyhow!("could not register a tray icon: {e}"))?;
    eprintln!("wadb: tray running; the panel needs a StatusNotifierItem host to show it");
    loop {
        std::thread::sleep(REFRESH);
        // None means the tray is gone — the panel restarted, or the user quit it.
        if handle
            .update(|tray: &mut WadbTray| tray.refresh())
            .is_none()
        {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adb::parse_devices;

    #[test]
    fn health_reflects_who_holds_the_port() {
        assert_eq!(
            health(&UnitState::Active, &PortOwner::Ours(42)),
            Health::Supervised
        );
        // Active unit, somebody else on the port: not our supervision.
        assert_eq!(
            health(&UnitState::Active, &PortOwner::Foreign),
            Health::Degraded
        );
        assert_eq!(
            health(&UnitState::Inactive, &PortOwner::HeldUnknown),
            Health::Degraded
        );
        assert_eq!(health(&UnitState::Active, &PortOwner::Nobody), Health::Down);
        assert_eq!(
            health(&UnitState::NotInstalled, &PortOwner::Ours(42)),
            Health::Down,
            "nothing installed means nothing of ours is supervising"
        );
    }

    #[test]
    fn each_state_has_its_own_icon() {
        let names: Vec<&str> = [Health::Supervised, Health::Degraded, Health::Down]
            .iter()
            .map(|h| icon_name(*h))
            .collect();
        assert_eq!(names.len(), 3);
        assert_eq!(
            names.iter().collect::<std::collections::HashSet<_>>().len(),
            3,
            "a state that looks like another state is not worth showing"
        );
    }

    #[test]
    fn the_tooltip_counts_devices_in_words_that_agree() {
        let one = parse_devices("192.168.86.45:42595 device model:Pixel_8a\n");
        let two = parse_devices(
            "192.168.86.45:42595 device model:Pixel_8a\n192.168.86.99:41000 device model:Tab_S9\n",
        );
        assert!(tooltip(Health::Supervised, &[], 5037).contains("no wireless devices"));
        assert!(tooltip(Health::Supervised, &one, 5037).ends_with("1 wireless device"));
        assert!(tooltip(Health::Supervised, &two, 5037).ends_with("2 wireless devices"));
        assert!(tooltip(Health::Down, &[], 5037).starts_with("adb server is down"));
        assert!(tooltip(Health::Degraded, &[], 5137).contains("5137"));
    }

    #[test]
    fn device_labels_name_the_model_and_flag_a_bad_state() {
        let devices = parse_devices(
            "192.168.86.45:42595 device model:Pixel_8a\nadb-X-y._adb-tls-connect._tcp offline model:Tab_S9\n",
        );
        assert_eq!(device_label(&devices[0]), "Pixel_8a   192.168.86.45:42595");
        // An offline device must not look identical to a healthy one.
        assert!(device_label(&devices[1]).contains("(offline)"));
        assert!(!device_label(&devices[1]).contains("_adb-tls-connect._tcp"));
    }
}
