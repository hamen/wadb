// SPDX-License-Identifier: Apache-2.0

//! Rendering and installing the systemd --user unit that supervises the adb server.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::adb::{self, MdnsSupport, SmartSocket, DEFAULT_PORT};

pub const UNIT_NAME: &str = "wadb.service";

/// `RestartSteps` and `RestartMaxDelaySec` need systemd 254. Ubuntu 22.04 ships 249, so a
/// lower version gets a plain fixed interval instead of a refusal: a coarser backoff is a
/// far better outcome than no supervision at all.
pub const BACKOFF_MIN_SYSTEMD: u32 = 254;

pub fn systemd_version() -> Option<u32> {
    let out = Command::new("systemctl").arg("--version").output().ok()?;
    parse_systemd_version(&String::from_utf8_lossy(&out.stdout))
}

pub fn parse_systemd_version(text: &str) -> Option<u32> {
    text.split_whitespace()
        .nth(1)
        .and_then(|t| t.split(['.', '-', '~']).next())
        .and_then(|t| t.parse().ok())
}

/// systemd command lines are not shell: quote only when the path contains whitespace, and
/// never use `systemd-escape`, which is for unit *names*.
pub fn quote_exec(path: &Path) -> String {
    let s = path.display().to_string();
    if s.contains(char::is_whitespace) {
        format!("\"{}\"", s.replace('"', "\\\""))
    } else {
        s
    }
}

pub struct UnitSpec {
    pub adb: PathBuf,
    pub port: u16,
    pub systemd: Option<u32>,
}

pub fn render_unit(spec: &UnitSpec) -> String {
    let exec = quote_exec(&spec.adb);
    // A non-default port has to be requested, and the listen spec must carry no hostname:
    // adb aborts on `tcp:127.0.0.1:<port>` and binds loopback by default anyway.
    let listen = if spec.port == DEFAULT_PORT {
        String::new()
    } else {
        format!(" -L tcp:{}", spec.port)
    };
    let mount_dir = spec
        .adb
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "/".into());

    let backoff = match spec.systemd {
        Some(v) if v >= BACKOFF_MIN_SYSTEMD => {
            "RestartSec=1s\nRestartSteps=5\nRestartMaxDelaySec=30s".to_string()
        }
        // Older systemd would fail to load the unit outright on unknown keys.
        _ => "RestartSec=5s".to_string(),
    };

    format!(
        "\
[Unit]
Description=Keep the ADB server running for wireless debugging (wadb)
Documentation=https://github.com/hamen/wadb
# Never latch into `failed`: giving up permanently would end supervision silently,
# which is the exact state this unit exists to prevent.
StartLimitIntervalSec=0
# The adb binary can live on a separate mount that is not ready when a lingering
# user manager starts.
RequiresMountsFor={mount_dir}

[Service]
Type=simple
ExecStart={exec}{listen} nodaemon server
Restart=always
{backoff}
# adb abort()s when its listen socket is taken, so every lost restart race would
# otherwise write a core dump.
LimitCORE=0

[Install]
WantedBy=default.target
"
    )
}

fn unit_path() -> Result<PathBuf> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config")
        });
    Ok(base.join("systemd/user").join(UNIT_NAME))
}

pub fn installed_unit() -> Option<String> {
    std::fs::read_to_string(unit_path().ok()?).ok()
}

/// The adb binary baked into the installed unit. `status` must re-validate *that* binary,
/// not whatever the resolver would pick today: an SDK upgrade or a changed $ADB would
/// otherwise report on a binary the unit is not running.
pub fn installed_adb() -> Option<PathBuf> {
    let unit = installed_unit()?;
    let exec = unit.lines().find(|l| l.starts_with("ExecStart="))?;
    let rest = exec.strip_prefix("ExecStart=")?.trim();
    let path = if let Some(quoted) = rest.strip_prefix('"') {
        quoted.split('"').next()?.to_string()
    } else {
        rest.split_whitespace().next()?.to_string()
    };
    Some(PathBuf::from(path))
}

/// The port the installed unit actually listens on, so status and the TUI never inspect
/// 5037 while the unit is elsewhere.
pub fn installed_port() -> Option<u16> {
    let unit = installed_unit()?;
    let exec = unit.lines().find(|l| l.starts_with("ExecStart="))?;
    match exec.split("-L tcp:").nth(1) {
        Some(rest) => rest.split_whitespace().next()?.parse().ok(),
        None => Some(DEFAULT_PORT),
    }
}

/// Run systemctl and *check the exit status*. Ignoring it lets `install` report success
/// after `enable --now` failed, which is the one outcome the user must never be told.
fn systemctl(args: &[&str]) -> Result<String> {
    let out = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .context("systemctl --user is not usable; is this a systemd user session?")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!(
            "systemctl --user {} failed: {}",
            args.join(" "),
            stderr.trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Queries whose non-zero exit is a legitimate answer rather than a failure:
/// `is-active` exits non-zero for an inactive unit, `show` for a missing one.
fn systemctl_query(args: &[&str]) -> String {
    Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// The PID actually holding the listening socket, read from `ss`.
///
/// Needed because "the unit is active" and "the unit owns the port" are different
/// statements: a client that won a restart race holds the port while the unit sits in
/// backoff. Returns None when the owner cannot be read at all, which happens for a
/// socket owned by another user.
pub fn listener_pid(port: u16) -> Option<u32> {
    let out = Command::new("ss")
        .args(["-ltnpH", "sport", "=", &format!(":{port}")])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (_, rest) = text.split_once("pid=")?;
    rest.split(|c: char| !c.is_ascii_digit())
        .find(|t| !t.is_empty())?
        .parse()
        .ok()
}

pub fn main_pid() -> Option<u32> {
    systemctl_query(&["show", UNIT_NAME, "-p", "MainPID", "--value"])
        .parse()
        .ok()
        .filter(|p| *p != 0)
}

pub fn is_active() -> bool {
    // A non-zero exit here means "inactive", not "failed".
    systemctl_query(&["is-active", UNIT_NAME]) == "active"
}

pub fn lingering() -> bool {
    Command::new("loginctl")
        .args(["show-user", "--property=Linger", "--value"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "yes")
        .unwrap_or(false)
}

pub struct InstallReport {
    pub adb: PathBuf,
    pub mdns: String,
    pub port: u16,
    pub changed: bool,
    pub lingering: bool,
    pub backoff_full: bool,
}

pub fn install() -> Result<InstallReport> {
    let adb_path = adb::resolve_adb()?;

    // The gate. Probe the resolved binary in isolation before trusting it with the unit.
    let mdns = match adb::probe_mdns_support(&adb_path)? {
        MdnsSupport::Present(v) => v,
        MdnsSupport::Absent => bail!(
            "{} has no mDNS backend, so adb cannot reconnect paired devices on its own.\n\
             Install Android SDK Platform-Tools and re-run, or set $ADB to a build that has it.\n\
             Check it yourself with: ADB_SERVER_SOCKET=tcp:127.0.0.1:<scratch port> {} mdns check",
            adb_path.display(),
            adb_path.display()
        ),
    };

    let port = std::env::var("ANDROID_ADB_SERVER_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PORT);
    let systemd = systemd_version();
    let spec = UnitSpec {
        adb: adb_path.clone(),
        port,
        systemd,
    };
    let rendered = render_unit(&spec);

    let path = unit_path()?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    let changed = installed_unit().as_deref() != Some(rendered.as_str());
    if changed {
        std::fs::write(&path, &rendered)?;
    }

    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", UNIT_NAME])?;
    // `enable --now` does not restart a unit that is already running with an older
    // ExecStart or port.
    if changed {
        systemctl(&["restart", UNIT_NAME])?;
    }

    Ok(InstallReport {
        adb: adb_path,
        mdns,
        port,
        changed,
        lingering: lingering(),
        backoff_full: systemd.is_some_and(|v| v >= BACKOFF_MIN_SYSTEMD),
    })
}

pub fn start() -> Result<()> {
    systemctl(&["start", UNIT_NAME])?;
    Ok(())
}

pub fn restart() -> Result<()> {
    systemctl(&["restart", UNIT_NAME])?;
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let _ = systemctl(&["disable", "--now", UNIT_NAME]);
    let path = unit_path()?;
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    let _ = systemctl(&["daemon-reload"]);
    Ok(())
}

/// Who holds the port.
#[derive(Debug, PartialEq, Eq)]
pub enum PortOwner {
    /// The listening socket belongs to our unit's main process.
    Ours(u32),
    /// Somebody else's adb server is listening.
    Foreign,
    /// Something is listening but the owner cannot be read, e.g. another user's socket.
    HeldUnknown,
    Nobody,
}

/// Decide ownership from the listening PID and the unit's MainPID.
///
/// Split out from the syscalls so the comparison itself is testable: "the unit is active"
/// is not the same statement as "the unit owns the port".
pub fn classify_owner(up: bool, listener: Option<u32>, main: Option<u32>) -> PortOwner {
    if !up {
        return PortOwner::Nobody;
    }
    match (listener, main) {
        (Some(l), Some(m)) if l == m => PortOwner::Ours(l),
        (Some(_), _) => PortOwner::Foreign,
        // An unreadable owner must not be reported as ours.
        (None, _) => PortOwner::HeldUnknown,
    }
}

pub fn port_owner(port: u16) -> PortOwner {
    classify_owner(
        SmartSocket::new(port).is_up(),
        listener_pid(port),
        main_pid().filter(|_| is_active()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(port: u16, systemd: Option<u32>) -> UnitSpec {
        UnitSpec {
            adb: PathBuf::from("/opt/sdk/platform-tools/adb"),
            port,
            systemd,
        }
    }

    #[test]
    fn unit_supervises_with_an_absolute_path_and_no_listen_hostname() {
        let unit = render_unit(&spec(DEFAULT_PORT, Some(257)));
        assert!(unit.contains("ExecStart=/opt/sdk/platform-tools/adb nodaemon server"));
        assert!(
            !unit.contains("127.0.0.1"),
            "a hostname in -L makes adb abort"
        );
        assert!(
            !unit.contains(" -a"),
            "-a would expose the server on the LAN"
        );
        assert!(unit.contains("Restart=always"));
        assert!(unit.contains("Type=simple"));
        assert!(unit.contains("LimitCORE=0"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn start_limit_is_in_unit_and_backoff_is_in_service() {
        let unit = render_unit(&spec(DEFAULT_PORT, Some(257)));
        let (unit_section, service_section) = unit.split_once("[Service]").unwrap();
        // Putting these in the wrong section is silently ignored by systemd.
        assert!(unit_section.contains("StartLimitIntervalSec=0"));
        assert!(service_section.contains("RestartSteps=5"));
        assert!(service_section.contains("RestartMaxDelaySec=30s"));
    }

    #[test]
    fn old_systemd_gets_a_fixed_interval_instead_of_a_refusal() {
        // Ubuntu 22.04 ships systemd 249; unknown keys would stop the unit loading at all.
        let unit = render_unit(&spec(DEFAULT_PORT, Some(249)));
        assert!(!unit.contains("RestartSteps"));
        assert!(!unit.contains("RestartMaxDelaySec"));
        assert!(unit.contains("RestartSec=5s"));
        assert!(
            unit.contains("Restart=always"),
            "still supervised, just more coarsely"
        );
    }

    #[test]
    fn unknown_systemd_version_takes_the_conservative_branch() {
        let unit = render_unit(&spec(DEFAULT_PORT, None));
        assert!(!unit.contains("RestartSteps"));
        assert!(unit.contains("RestartSec=5s"));
    }

    #[test]
    fn custom_port_is_requested_without_a_hostname() {
        let unit = render_unit(&spec(5137, Some(257)));
        assert!(unit.contains("ExecStart=/opt/sdk/platform-tools/adb -L tcp:5137 nodaemon server"));
    }

    #[test]
    fn requires_the_mount_the_adb_binary_lives_on() {
        // The SDK adb here sits under /mnt, which a lingering unit can beat at boot.
        let unit = render_unit(&spec(DEFAULT_PORT, Some(257)));
        assert!(unit.contains("RequiresMountsFor=/opt/sdk/platform-tools"));
    }

    #[test]
    fn no_network_online_target() {
        // It is a system target; a --user unit cannot order against it, so naming it
        // would add a failed job and achieve nothing.
        let unit = render_unit(&spec(DEFAULT_PORT, Some(257)));
        assert!(!unit.contains("network-online.target"));
        assert!(!unit.contains("After=network.target"));
    }

    #[test]
    fn parses_the_port_back_out_of_a_rendered_unit() {
        let unit = render_unit(&spec(5137, Some(257)));
        let exec = unit.lines().find(|l| l.starts_with("ExecStart=")).unwrap();
        let port: u16 = exec
            .split("-L tcp:")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(port, 5137);
    }

    #[test]
    fn quotes_only_paths_that_need_it() {
        assert_eq!(quote_exec(Path::new("/opt/sdk/adb")), "/opt/sdk/adb");
        assert_eq!(
            quote_exec(Path::new("/opt/my sdk/adb")),
            "\"/opt/my sdk/adb\""
        );
    }

    #[test]
    fn reads_the_adb_path_back_out_of_a_unit() {
        let unit = render_unit(&spec(5137, Some(257)));
        let exec = unit.lines().find(|l| l.starts_with("ExecStart=")).unwrap();
        let path = exec
            .strip_prefix("ExecStart=")
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap();
        assert_eq!(path, "/opt/sdk/platform-tools/adb");

        let quoted = render_unit(&UnitSpec {
            adb: PathBuf::from("/opt/my sdk/adb"),
            port: DEFAULT_PORT,
            systemd: Some(257),
        });
        let exec = quoted
            .lines()
            .find(|l| l.starts_with("ExecStart="))
            .unwrap();
        assert!(exec.contains("\"/opt/my sdk/adb\""));
    }

    #[test]
    fn ownership_needs_the_listening_pid_to_match_the_unit() {
        assert_eq!(
            classify_owner(true, Some(42), Some(42)),
            PortOwner::Ours(42)
        );
        // Active unit, but somebody else won the race for the port.
        assert_eq!(classify_owner(true, Some(99), Some(42)), PortOwner::Foreign);
        // A server with no unit behind it at all.
        assert_eq!(classify_owner(true, Some(99), None), PortOwner::Foreign);
        // Unreadable owner must never be reported as ours.
        assert_eq!(classify_owner(true, None, Some(42)), PortOwner::HeldUnknown);
        assert_eq!(classify_owner(false, None, None), PortOwner::Nobody);
    }

    #[test]
    fn reads_systemd_version() {
        assert_eq!(
            parse_systemd_version("systemd 257 (257.9-0ubuntu2.5)"),
            Some(257)
        );
        assert_eq!(
            parse_systemd_version("systemd 249 (249.11-0ubuntu3)"),
            Some(249)
        );
        assert_eq!(parse_systemd_version("nonsense"), None);
    }
}
