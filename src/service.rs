// SPDX-License-Identifier: Apache-2.0

//! Rendering and installing the systemd --user unit that supervises the adb server.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::adb::{self, MdnsSupport, SmartSocket, DEFAULT_PORT};

pub const UNIT_NAME: &str = "wadb.service";
pub const CONNECT_UNIT_NAME: &str = "wadb-connect.service";

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
/// Quote a systemd directive *value* (not a command line) when it needs it.
pub fn quote_value(path: &Path) -> String {
    let s = path.display().to_string();
    if s.contains(char::is_whitespace) || s.contains(['"', '\\']) {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s
    }
}

pub fn quote_exec(path: &Path) -> String {
    let s = path.display().to_string();
    // systemd also treats quotes, backslashes and specifiers specially.
    if s.contains(char::is_whitespace) || s.contains(['"', '\\', '\'', '$', '%']) {
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
    let mount_dir = quote_value(spec.adb.parent().unwrap_or(Path::new("/")));

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

/// The watcher unit.
///
/// It is deliberately NOT `BindsTo=wadb.service`: `adb kill-server` restarts the server unit, and
/// `BindsTo` stops a dependent on restart without starting it again — which would kill the watcher
/// at precisely the moment its whole job begins. `Wants=`/`After=` express the ordering without
/// that behaviour, and the watcher waits for the server on its own anyway.
pub fn render_connect_unit(wadb: &Path, port: u16) -> String {
    let exec = quote_exec(wadb);
    let mount_dir = quote_value(wadb.parent().unwrap_or(Path::new("/")));
    format!(
        "\
[Unit]
Description=Reconnect wireless ADB devices that adb's own mDNS cannot find (wadb)
Documentation=https://github.com/hamen/wadb
Wants={UNIT_NAME}
After={UNIT_NAME}
StartLimitIntervalSec=0
RequiresMountsFor={mount_dir}

[Service]
Type=simple
# The watcher reaches the server over its smart socket and never runs adb, so only the port
# matters here. ExecStart is absolute: a --user unit inherits no PATH that would find it.
Environment=ANDROID_ADB_SERVER_PORT={port}
ExecStart={exec} daemon
Restart=always
RestartSec=5s

[Install]
WantedBy=default.target
"
    )
}

fn unit_dir() -> Result<PathBuf> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config")
        });
    Ok(base.join("systemd/user"))
}

fn connect_unit_path() -> Result<PathBuf> {
    Ok(unit_dir()?.join(CONNECT_UNIT_NAME))
}

pub fn connect_unit_active() -> bool {
    systemctl_query(&["is-active", CONNECT_UNIT_NAME]) == "active"
}

fn unit_path() -> Result<PathBuf> {
    Ok(unit_dir()?.join(UNIT_NAME))
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

fn port_from_env() -> u16 {
    std::env::var("ANDROID_ADB_SERVER_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PORT)
}

pub fn install() -> Result<InstallReport> {
    let adb_path = adb::resolve_adb()?;

    // The gate. Probe the resolved binary in isolation before trusting it with the unit.
    // The socket that matters belongs to whichever server is running *now*, which is the
    // currently installed unit's port - not the port we are about to install on. Checking only
    // the target port misses a server on the old port that still owns the mDNS socket, and the
    // probe then answers Absent underneath it.
    // installed_port() only says what the unit file *claims*. If that unit is stopped and some
    // other server is on the target port, checking the empty old port would probe while that
    // server owns the mDNS socket. Use whichever port actually has something listening.
    let target = port_from_env();
    let holding_port = [installed_port(), Some(target)]
        .into_iter()
        .flatten()
        .find(|p| adb::SmartSocket::new(*p).is_up())
        .unwrap_or(target);
    let mdns = match adb::mdns_support(&adb_path, holding_port)? {
        MdnsSupport::Present(v) => v,
        MdnsSupport::Absent => bail!(
            "{} (version {}) has no mDNS backend, so adb cannot reconnect paired devices \
             on its own.\n\
             Install Android SDK Platform-Tools and re-run, or set $ADB to a build that has it.\n\
             Check it yourself with: ADB_SERVER_SOCKET=tcp:127.0.0.1:<scratch port> {} mdns check",
            adb_path.display(),
            adb::version_of(&adb_path).unwrap_or_else(|| "unknown".into()),
            adb_path.display()
        ),
    };

    let port = target;
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

    // The watcher runs this same binary, by absolute path: a user unit inherits no PATH that
    // would find it.
    let wadb = std::env::current_exe().context("could not locate the wadb binary")?;
    // A unit pinned to a build directory dies the next time that directory is cleaned, and
    // Restart=always then crash-loops it forever.
    if wadb.components().any(|c| c.as_os_str() == "target") {
        eprintln!(
            "warning: installing from a build directory ({}).\n\
             `cargo clean` would leave the watcher unit crash-looping. Prefer `cargo install --path .`",
            wadb.display()
        );
    }
    let connect_rendered = render_connect_unit(&wadb, port);
    let connect_path = connect_unit_path()?;
    let connect_changed =
        std::fs::read_to_string(&connect_path).ok().as_deref() != Some(connect_rendered.as_str());
    if connect_changed {
        std::fs::write(&connect_path, &connect_rendered)?;
    }

    for step in install_steps(changed, connect_changed) {
        let args: Vec<&str> = step.iter().map(String::as_str).collect();
        systemctl(&args)?;
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

/// Poll until the unit owns the port, or give up. Startup is not instantaneous, and
/// "enable --now returned 0" is not the same statement as "the unit has the port".
pub fn wait_for_ownership(port: u16, timeout: std::time::Duration) -> PortOwner {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let owner = port_owner(port);
        if matches!(owner, PortOwner::Ours(_)) || std::time::Instant::now() > deadline {
            return owner;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

pub fn start() -> Result<()> {
    systemctl(&["start", UNIT_NAME])?;
    Ok(())
}

pub fn restart() -> Result<()> {
    systemctl(&["restart", UNIT_NAME])?;
    Ok(())
}

/// The systemctl calls an install performs, as data.
///
/// `enable --now` does not restart a unit that is already running with an older ExecStart or port,
/// so a changed unit needs an explicit restart.
pub fn install_steps(server_changed: bool, watcher_changed: bool) -> Vec<Vec<String>> {
    let mut steps: Vec<Vec<String>> = vec![
        vec!["daemon-reload".into()],
        vec!["enable".into(), "--now".into(), UNIT_NAME.into()],
        vec!["enable".into(), "--now".into(), CONNECT_UNIT_NAME.into()],
    ];
    if server_changed {
        steps.push(vec!["restart".into(), UNIT_NAME.into()]);
    }
    if watcher_changed {
        steps.push(vec!["restart".into(), CONNECT_UNIT_NAME.into()]);
    }
    steps
}

/// One action in an uninstall. Modelled as data because the *order* is what matters here, and an
/// order is exactly what a plain sequence of calls cannot assert.
#[derive(Debug, PartialEq, Eq)]
pub enum UninstallStep {
    /// Stop and disable a unit.
    Disable(&'static str),
    /// Delete a unit file.
    Remove(&'static str),
    /// Reload systemd. Must come *after* the files are gone, or systemd keeps the definitions of
    /// files that no longer exist.
    Reload,
}

/// The uninstall sequence. The watcher stops first: the other order leaves it reconnecting
/// devices to a server that is about to go away.
pub fn uninstall_steps(watcher_installed: bool, server_installed: bool) -> Vec<UninstallStep> {
    let mut steps = Vec::new();
    if watcher_installed {
        steps.push(UninstallStep::Disable(CONNECT_UNIT_NAME));
    }
    if server_installed {
        steps.push(UninstallStep::Disable(UNIT_NAME));
    }
    if watcher_installed {
        steps.push(UninstallStep::Remove(CONNECT_UNIT_NAME));
    }
    if server_installed {
        steps.push(UninstallStep::Remove(UNIT_NAME));
    }
    steps.push(UninstallStep::Reload);
    steps
}

pub fn uninstall() -> Result<()> {
    let watcher_path = connect_unit_path()?;
    let watcher_installed = watcher_path.exists();
    let server_installed = installed_unit().is_some();

    let server_path = unit_path()?;
    // Failing partway and still deleting a unit file would leave the server we own running with
    // nothing left to manage it, so a disable that fails aborts the whole thing.
    for step in uninstall_steps(watcher_installed, server_installed) {
        match step {
            UninstallStep::Disable(unit) => {
                systemctl(&["disable", "--now", unit])
                    .context("could not stop the units; the adb server may still be running")?;
            }
            UninstallStep::Remove(unit) => {
                let path = if unit == CONNECT_UNIT_NAME {
                    &watcher_path
                } else {
                    &server_path
                };
                if path.exists() {
                    std::fs::remove_file(path)?;
                }
            }
            UninstallStep::Reload => systemctl(&["daemon-reload"]).map(|_| ())?,
        }
    }
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
    fn install_enables_both_units_and_restarts_only_what_changed() {
        let steps = install_steps(false, false);
        assert!(steps.iter().any(|s| s == &["enable", "--now", UNIT_NAME]));
        assert!(steps
            .iter()
            .any(|s| s == &["enable", "--now", CONNECT_UNIT_NAME]));
        assert!(
            !steps.iter().any(|s| s[0] == "restart"),
            "nothing changed, nothing to restart"
        );

        let steps = install_steps(true, true);
        assert!(steps.iter().any(|s| s == &["restart", UNIT_NAME]));
        assert!(steps.iter().any(|s| s == &["restart", CONNECT_UNIT_NAME]));
        assert_eq!(steps[0], ["daemon-reload"], "reload before enabling");
    }

    #[test]
    fn uninstall_order_is_disable_then_delete_then_reload() {
        use UninstallStep::*;
        assert_eq!(
            uninstall_steps(true, true),
            vec![
                // The watcher stops first, or it reconnects devices to a dying server.
                Disable(CONNECT_UNIT_NAME),
                Disable(UNIT_NAME),
                // Files go before the reload, or systemd keeps definitions for files that are
                // no longer on disk.
                Remove(CONNECT_UNIT_NAME),
                Remove(UNIT_NAME),
                Reload,
            ]
        );
        assert_eq!(uninstall_steps(false, false), vec![Reload]);
        assert_eq!(
            uninstall_steps(false, true),
            vec![Disable(UNIT_NAME), Remove(UNIT_NAME), Reload]
        );
    }

    #[test]
    fn watcher_unit_survives_a_restart_of_the_server_unit() {
        let unit = render_connect_unit(Path::new("/home/u/.cargo/bin/wadb"), DEFAULT_PORT);
        // BindsTo would stop the watcher when the server unit restarts and never start it again -
        // killing it at exactly the moment `adb kill-server` makes it necessary.
        assert!(!unit.contains("BindsTo"));
        assert!(unit.contains("Wants=wadb.service"));
        assert!(unit.contains("After=wadb.service"));
        assert!(unit.contains("Restart=always"));
    }

    #[test]
    fn watcher_unit_runs_this_binary_by_absolute_path() {
        // A --user unit inherits no PATH that would find `wadb`.
        let unit = render_connect_unit(Path::new("/home/u/.cargo/bin/wadb"), 5137);
        assert!(unit.contains("ExecStart=/home/u/.cargo/bin/wadb daemon"));
        // And it must talk to the same port the server unit listens on.
        assert!(unit.contains("Environment=ANDROID_ADB_SERVER_PORT=5137"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn directive_values_are_quoted_when_they_need_it() {
        // RequiresMountsFor= takes a value, not a command line, and was interpolated raw.
        let unit = render_connect_unit(Path::new("/home/u/my tools/wadb"), DEFAULT_PORT);
        assert!(
            unit.contains("RequiresMountsFor=\"/home/u/my tools\""),
            "{unit}"
        );
        assert!(
            unit.contains("ExecStart=\"/home/u/my tools/wadb\" daemon"),
            "{unit}"
        );
        // The watcher reaches the server over the smart socket, so it needs no adb binary.
        assert!(!unit.contains("Environment=ADB="), "{unit}");
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
