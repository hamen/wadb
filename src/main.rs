// SPDX-License-Identifier: Apache-2.0

//! wadb - keep the ADB server alive so paired phones stay available for wireless debugging.

mod adb;
mod daemon;
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
    /// Reconnect every advertised wireless device once, then exit.
    Connect,
    /// Run the reconnect watcher. This is what `wadb-connect.service` runs.
    Daemon,
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
        Some(Command::Connect) => connect_once(),
        Some(Command::Daemon) => daemon::run(adb_for_commands()?, port()),
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

    // A different adb first on PATH can win a restart race and start a server with no
    // mDNS backend, which is the failure this tool exists to prevent.
    if let Some(path_adb) = std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p)
            .map(|d| d.join("adb"))
            .find(|c| c.is_file())
    }) {
        if path_adb != report.adb {
            println!(
                "\nnote: `adb` on your PATH is {}, not the binary this unit runs.\n\
                 If that one starts a server first it may have no mDNS backend.",
                path_adb.display()
            );
        }
    }

    if !report.lingering {
        println!(
            "\nlingering is off, so the unit stops at your last logout. Enable it with:\n  \
             loginctl enable-linger {}",
            std::env::var("USER").unwrap_or_else(|_| "$USER".into())
        );
    }

    // Report what the unit actually achieved rather than assuming `enable --now` won,
    // and exit non-zero when it did not: a script that reads only the status code must
    // not be told a foreign server is our supervision.
    match service::wait_for_ownership(report.port, Duration::from_secs(5)) {
        PortOwner::Ours(pid) => {
            println!("\nsupervising adb, pid {pid}");
            Ok(())
        }
        PortOwner::Foreign | PortOwner::HeldUnknown => bail!(
            "port {} is held by another adb server, so the unit is retrying.\n\
             Stop that server once with `wadb takeover`, or quit whatever started it.",
            report.port
        ),
        PortOwner::Nobody => {
            bail!("the unit is not listening yet; check `systemctl --user status wadb`")
        }
    }
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
    let installed = unit_state();
    let port = port();
    match installed {
        ui::UnitState::NotInstalled => println!("unit:     not installed - run `wadb install`"),
        ui::UnitState::Active => println!("unit:     active"),
        ui::UnitState::Inactive => println!("unit:     installed but not active"),
    }
    if installed != ui::UnitState::NotInstalled {
        println!(
            "watcher:  {}",
            if service::connect_unit_active() {
                "active"
            } else {
                "NOT running - wireless devices will not come back on their own"
            }
        );
    }

    // With no unit there is nothing of ours on any port, and describing whatever else is
    // listening would attribute a stranger's server to this tool.
    if installed == ui::UnitState::NotInstalled {
        match adb::resolve_adb() {
            Ok(path) => println!("adb:      {} (would be used)", path.display()),
            Err(e) => println!("adb:      {e}"),
        }
        return Ok(());
    }

    println!("port:     {port}");
    match service::port_owner(port) {
        PortOwner::Ours(pid) => println!("server:   ours, pid {pid}"),
        PortOwner::Foreign => {
            println!("server:   held by another adb server - `wadb takeover` to hand the port over")
        }
        PortOwner::HeldUnknown => {
            println!("server:   port held, owner could not be read")
        }
        PortOwner::Nobody => println!("server:   down"),
    }

    // Re-validate the binary the unit actually runs, with the same isolated probe as
    // install: a plain `mdns check` would answer from whichever server owns the port.
    match service::installed_adb() {
        Some(path) if path.is_file() => {
            println!("adb:      {}", path.display());
            match adb::probe_mdns_support(&path) {
                Ok(MdnsSupport::Present(v)) => println!("mdns:     {v}"),
                Ok(MdnsSupport::Absent) => println!(
                    "mdns:     MISSING - this adb cannot reconnect devices on its own; re-run `wadb install`"
                ),
                Err(e) => println!("mdns:     could not probe ({e})"),
            }
        }
        Some(path) => println!(
            "adb:      {} is gone - the unit will fail; re-run `wadb install`",
            path.display()
        ),
        None => println!("adb:      could not read the unit's ExecStart"),
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

    // And whether our own browser works, which is what QR pairing depends on.
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

/// The binary to run for mutating commands: the one the unit actually supervises, so a
/// `$ADB` or PATH win cannot drive a different adb against our server.
fn adb_for_commands() -> Result<std::path::PathBuf> {
    service::installed_adb()
        .filter(|p| p.is_file())
        .map_or_else(adb::resolve_adb, Ok)
}

fn pair_manually(endpoint: &str) -> Result<()> {
    let adb_path = adb_for_commands()?;
    let port = port();
    if !SmartSocket::new(port).is_up() {
        bail!("no adb server on port {port}. Run `wadb install` first.");
    }
    // Read without echo: the code is a shared secret for the life of the dialog.
    // Wrapped so it is wiped even if `pair` returns early through `?`.
    let code = pairing::Secret::new(rpassword::prompt_password(
        "pairing code shown on the phone: ",
    )?);
    let outcome = pairing::pair(&adb_path, port, endpoint, code.as_str())?;
    let host = endpoint
        .rsplit_once(':')
        .map(|(h, _)| h.trim_matches(['[', ']']))
        .unwrap_or(endpoint);
    match outcome {
        pairing::PairOutcome::Paired { guid } => {
            println!(
                "paired{}",
                guid.as_deref()
                    .map(|g| format!(" ({g})"))
                    .unwrap_or_default()
            );
            // The pairing port is not the connect port: they are different mDNS services.
            // Connecting to `endpoint` here would attach to the pairing socket, which is
            // already gone.
            let host = endpoint
                .rsplit_once(':')
                .map(|(h, _)| h.trim_matches(['[', ']']))
                .unwrap_or(endpoint);
            println!(
                "{}",
                pairing::connect_after_pair(&adb_path, port, host, guid.as_deref())?
            );
            Ok(())
        }
        pairing::PairOutcome::WrongCode => bail!("the phone rejected that code"),
        pairing::PairOutcome::Unreachable => {
            bail!("nothing answered at {endpoint} - the pairing dialog may have closed")
        }
        // Wording varies across adb 30..36, so an unrecognised line is not a failure.
        // The QR path already falls through to a connect attempt; so must this one, or a
        // real pairing reads as an error.
        // Unknown wording after a successful run is not a failure: adb's phrasing varies
        // across 30..36. Unknown wording after a *failed* run is.
        pairing::PairOutcome::Other { ref msg, .. } => {
            if !pairing::should_attempt_connect(&outcome) {
                bail!("{msg}");
            }
            match pairing::connect_after_pair(&adb_path, port, host, None) {
                Ok(line) => {
                    println!("{line}");
                    Ok(())
                }
                Err(_) => bail!("{msg}"),
            }
        }
    }
}

fn takeover() -> Result<()> {
    let port = port();
    let adb_path = adb_for_commands()?;
    match service::port_owner(port) {
        PortOwner::Ours(pid) => {
            println!("port {port} is already ours (pid {pid}); nothing to do.");
            return Ok(());
        }
        PortOwner::Nobody => {
            println!("nothing is holding port {port}.");
            return Ok(());
        }
        PortOwner::Foreign | PortOwner::HeldUnknown => {}
    }

    // A cooperative request, not a kill: we never signal a process we do not own.
    // The socket is pinned explicitly, or an inherited ADB_SERVER_SOCKET could send this
    // to a different server than the one we just checked.
    let out = std::process::Command::new(&adb_path)
        .env("ADB_SERVER_SOCKET", format!("tcp:127.0.0.1:{port}"))
        .env_remove("ANDROID_ADB_SERVER_PORT")
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
        service::restart()?;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if let PortOwner::Ours(pid) = service::port_owner(port) {
                println!("the unit now owns port {port}, pid {pid}.");
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        println!("restarted the unit, but it has not taken the port yet.");
    }
    Ok(())
}

/// One pass of the watcher, by hand.
fn connect_once() -> Result<()> {
    let adb_path = adb_for_commands()?;
    let port = port();
    if !SmartSocket::new(port).is_up() {
        bail!("no adb server on port {port}. Run `wadb install` first.");
    }
    let mut failures = std::collections::HashMap::new();
    let connected = daemon::tick(&adb_path, port, &mut failures)?;
    if connected.is_empty() {
        println!("nothing to reconnect.");
    }
    for line in connected {
        println!("{line}");
    }
    Ok(())
}

fn tui() -> Result<()> {
    // ratatui on a pipe would emit escape sequences into whatever is reading.
    if !std::io::stdout().is_terminal() {
        return status();
    }
    let mut app = ui::App::new(port(), adb_for_commands().ok(), unit_state());
    app.refresh();
    ui::run(&mut app)
}
