// SPDX-License-Identifier: Apache-2.0

//! wadb - keep the ADB server alive so paired phones stay available for wireless debugging.

mod adb;
mod discovery;
mod pairing;
mod qr;
mod service;
mod ui;

use std::io::IsTerminal;
use std::time::Duration;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};

use adb::{MdnsSupport, SmartSocket};
use service::PortOwner;

#[derive(Parser)]
#[command(
    name = "wadb",
    version,
    about = "Keep the ADB server alive for wireless debugging"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Install and start the systemd --user unit that supervises the adb server.
    Install,
    /// Stop and remove the unit. Never touches an adb server it does not own.
    Uninstall,
    /// Report the unit, the server, the adb binary and mDNS discovery.
    Status,
    /// Pair with a phone by typing the six-digit code it shows.
    Pair {
        /// `ip:port` from the phone's Wireless debugging screen.
        endpoint: String,
    },
    /// Ask a foreign adb server to stop so the unit can take the port.
    Takeover,
}

fn port() -> u16 {
    service::installed_port()
        .or_else(|| std::env::var("ANDROID_ADB_SERVER_PORT").ok()?.parse().ok())
        .unwrap_or(adb::DEFAULT_PORT)
}

fn unit_state() -> ui::UnitState {
    if service::installed_unit().is_none() {
        ui::UnitState::NotInstalled
    } else if service::is_active() {
        ui::UnitState::Active
    } else {
        ui::UnitState::Inactive
    }
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Some(Command::Install) => install(),
        Some(Command::Uninstall) => uninstall(),
        Some(Command::Status) => status(),
        Some(Command::Pair { endpoint }) => pair_manually(&endpoint),
        Some(Command::Takeover) => takeover(),
        None => tui(),
    }
}

fn install() -> Result<()> {
    let report = service::install()?;
    println!("adb:      {}", report.adb.display());
    println!("mdns:     {}", report.mdns);
    println!("port:     {}", report.port);
    println!(
        "unit:     {} ({} backoff)",
        if report.changed {
            "written"
        } else {
            "unchanged"
        },
        if report.backoff_full {
            "stepped"
        } else {
            "fixed interval, systemd < 254"
        }
    );

    if !report.lingering {
        println!(
            "\nlingering is off, so the unit stops at your last logout. Enable it with:\n  \
             loginctl enable-linger {}",
            std::env::var("USER").unwrap_or_else(|_| "$USER".into())
        );
    }

    // Report what the unit actually achieved rather than assuming `enable --now` won.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if let PortOwner::Ours(pid) = service::port_owner(report.port) {
            println!("\nsupervising adb, pid {pid}");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    match service::port_owner(report.port) {
        PortOwner::Foreign => println!(
            "\nport {} is held by another adb server, so the unit is retrying.\n\
             Stop that server once with `wadb takeover`, or quit whatever started it.",
            report.port
        ),
        _ => println!("\nthe unit is not listening yet; check `systemctl --user status wadb`"),
    }
    Ok(())
}

fn uninstall() -> Result<()> {
    service::uninstall()?;
    println!("unit removed.");
    println!(
        "Stopping it also stopped the adb server it owned, so USB and emulator sessions on\n\
         that server are gone. The next adb command from any tool will start a fresh server -\n\
         possibly a build with no mDNS backend, which will not reconnect wireless devices."
    );
    Ok(())
}

fn status() -> Result<()> {
    let port = port();
    match unit_state() {
        ui::UnitState::NotInstalled => println!("unit:     not installed - run `wadb install`"),
        ui::UnitState::Active => println!("unit:     active"),
        ui::UnitState::Inactive => println!("unit:     installed but not active"),
    }
    println!("port:     {port}");

    match service::port_owner(port) {
        PortOwner::Ours(pid) => println!("server:   ours, pid {pid}"),
        PortOwner::Foreign => {
            println!("server:   held by another adb server - `wadb takeover` to hand the port over")
        }
        PortOwner::Nobody => println!("server:   down"),
    }

    match adb::resolve_adb() {
        Ok(path) => {
            println!("adb:      {}", path.display());
            // Re-validated with the same isolated probe as install: a plain `mdns check`
            // would answer from whatever server owns the port.
            match adb::probe_mdns_support(&path) {
                Ok(MdnsSupport::Present(v)) => println!("mdns:     {v}"),
                Ok(MdnsSupport::Absent) => {
                    println!("mdns:     MISSING - this adb cannot reconnect devices on its own")
                }
                Err(e) => println!("mdns:     could not probe ({e})"),
            }
        }
        Err(e) => println!("adb:      {e}"),
    }

    let sock = SmartSocket::new(port);
    if sock.is_up() {
        if let Ok(v) = sock.version() {
            println!("protocol: {v}");
        }
        let devices = sock.devices().unwrap_or_default();
        let wireless = adb::wireless_devices(&devices);
        println!(
            "devices:  {} wireless, {} total",
            wireless.len(),
            devices.len()
        );
        for d in &wireless {
            println!(
                "          {} {} [{}]",
                d.serial,
                d.state,
                d.model.clone().unwrap_or_default()
            );
        }
        // A compiled-in backend is not proof that discovery works right now.
        if let Ok(s) = sock.mdns_services() {
            let n = s.lines().filter(|l| l.contains("_adb-tls")).count();
            println!("discovery: {n} adb service(s) visible to adb's own mDNS");
        }
    }

    // And whether our own browser works, which is what pairing depends on.
    match discovery::browse_all(pairing::CONNECT_SERVICE, Duration::from_secs(2)) {
        Ok(found) => {
            println!("browser:  {} connect service(s) seen in 2s", found.len());
            for f in &found {
                println!("          {} -> {:?}:{}", f.instance(), f.addresses, f.port);
            }
        }
        Err(e) => println!("browser:  FAILED ({e}) - QR pairing will not work"),
    }
    Ok(())
}

fn pair_manually(endpoint: &str) -> Result<()> {
    let adb_path = adb::resolve_adb()?;
    let port = port();
    if !SmartSocket::new(port).is_up() {
        bail!("no adb server on port {port}. Run `wadb install` first.");
    }
    // Read without echo: the code is a shared secret for the length of the dialog.
    let code = rpassword::prompt_password("pairing code shown on the phone: ")?;
    match pairing::pair(&adb_path, port, endpoint, code.trim())? {
        pairing::PairOutcome::Paired { guid } => {
            println!(
                "paired{}",
                guid.map(|g| format!(" ({g})")).unwrap_or_default()
            );
            println!("{}", pairing::connect(&adb_path, port, endpoint)?);
            Ok(())
        }
        pairing::PairOutcome::WrongCode => bail!("the phone rejected that code"),
        pairing::PairOutcome::Unreachable => {
            bail!("nothing answered at {endpoint} - the pairing dialog may have closed")
        }
        pairing::PairOutcome::Other(msg) => bail!("{msg}"),
    }
}

fn takeover() -> Result<()> {
    let port = port();
    let adb_path = adb::resolve_adb()?;
    match service::port_owner(port) {
        PortOwner::Ours(pid) => {
            println!("port {port} is already ours (pid {pid}); nothing to do.");
            return Ok(());
        }
        PortOwner::Nobody => {
            println!("nothing is holding port {port}.");
            return Ok(());
        }
        PortOwner::Foreign => {}
    }
    // A cooperative request, not a kill: we never signal a process we do not own.
    let out = std::process::Command::new(&adb_path)
        .env("ANDROID_ADB_SERVER_PORT", port.to_string())
        .arg("kill-server")
        .output()?;
    if !out.status.success() {
        bail!(
            "adb kill-server failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    println!("asked the foreign server to stop.");
    if service::installed_unit().is_some() {
        println!("restart the unit with: systemctl --user restart wadb");
    }
    Ok(())
}

fn tui() -> Result<()> {
    // ratatui on a pipe would emit escape sequences into whatever is reading.
    if !std::io::stdout().is_terminal() {
        return status();
    }
    let mut app = ui::App::new(port(), adb::resolve_adb().ok(), unit_state());
    app.refresh();
    ui::run(&mut app)
}
