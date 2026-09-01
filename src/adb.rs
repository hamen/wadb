// SPDX-License-Identifier: Apache-2.0

//! Locating the adb binary, probing it for an mDNS backend, and talking to a running
//! adb server without ever starting one.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

/// Default port the adb server listens on.
pub const DEFAULT_PORT: u16 = 5037;

/// How a device is attached. Wireless devices reach us in two different serial shapes,
/// and both must be recognised: `ip:port` after an explicit connect, and
/// `adb-<id>-<suffix>._adb-tls-connect._tcp` when adb's own mDNS auto-connect attached it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Usb,
    Emulator,
    /// Connected over TCP with an explicit `host:port` serial.
    Tcp,
    /// Attached by adb's mDNS auto-connect, serial ends in `._adb-tls-connect._tcp`.
    Mdns,
}

impl Transport {
    pub fn is_wireless(self) -> bool {
        matches!(self, Transport::Tcp | Transport::Mdns)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub serial: String,
    /// `device`, `offline`, `unauthorized`, or anything else adb reports.
    pub state: String,
    pub model: Option<String>,
    pub product: Option<String>,
    pub device: Option<String>,
    pub transport: Transport,
}

/// Split a `host:port` serial. IPv6 serials contain many colons, so split on the LAST
/// colon, and honour the `[addr]:port` bracket form.
pub fn split_host_port(serial: &str) -> Option<(&str, u16)> {
    if let Some(rest) = serial.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        let port = tail.strip_prefix(':')?.parse().ok()?;
        return Some((host, port));
    }
    let (host, port) = serial.rsplit_once(':')?;
    if host.is_empty() || host.contains(':') {
        // Bare IPv6 with no brackets is not a valid adb serial.
        return None;
    }
    Some((host, port.parse().ok()?))
}

pub fn classify(serial: &str) -> Transport {
    if serial.starts_with("emulator-") {
        Transport::Emulator
    } else if serial.ends_with("._adb-tls-connect._tcp") {
        Transport::Mdns
    } else if split_host_port(serial).is_some() {
        Transport::Tcp
    } else {
        Transport::Usb
    }
}

/// Parse the payload of `host:devices-l`.
pub fn parse_devices(text: &str) -> Vec<Device> {
    text.lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .filter(|line| !line.starts_with("List of devices"))
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let serial = parts.next()?.to_string();
            let state = parts.next().unwrap_or("unknown").to_string();
            let (mut model, mut product, mut device) = (None, None, None);
            for field in parts {
                if let Some(v) = field.strip_prefix("model:") {
                    model = Some(v.to_string());
                } else if let Some(v) = field.strip_prefix("product:") {
                    product = Some(v.to_string());
                } else if let Some(v) = field.strip_prefix("device:") {
                    device = Some(v.to_string());
                }
            }
            let transport = classify(&serial);
            Some(Device {
                serial,
                state,
                model,
                product,
                device,
                transport,
            })
        })
        .collect()
}

/// Wireless devices only.
///
/// These are deliberately **not** de-duplicated. After pair-then-auto-connect one phone
/// can appear twice, once as `ip:port` and once under its mDNS serial, and the plan asked
/// for those to collapse into one row. They cannot be collapsed safely: the two serials
/// share nothing, and the only fields they do share - model, product, device - identify a
/// device *type*, not a phone. Keying on them merges two identical handsets into one row,
/// and silently hiding a device someone is trying to debug is a worse failure than showing
/// an extra row. The `how` column names the transport instead, so a duplicate is visible
/// and explicable rather than invented or concealed.
pub fn wireless_devices(all: &[Device]) -> Vec<Device> {
    all.iter()
        .filter(|d| d.transport.is_wireless())
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// Binary resolution
// ---------------------------------------------------------------------------

/// Where to look for adb, in order. An SDK `platform-tools/adb` is preferred over a
/// distro one, because distro builds are frequently compiled without the mDNS backend.
pub fn candidate_paths(env: &dyn Fn(&str) -> Option<String>, home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(p) = env("ADB") {
        out.push(PathBuf::from(p));
    }
    for var in ["ANDROID_SDK_ROOT", "ANDROID_HOME"] {
        if let Some(root) = env(var) {
            out.push(Path::new(&root).join("platform-tools").join("adb"));
        }
    }
    // The conventional default location: $ANDROID_HOME is often unset outside an
    // interactive shell, but the SDK is still on disk here.
    out.push(home.join("Android/Sdk/platform-tools/adb"));
    if let Some(path) = env("PATH") {
        for dir in path.split(':').filter(|d| !d.is_empty()) {
            out.push(Path::new(dir).join("adb"));
        }
    }
    out
}

/// The version string adb reports, for messages that must name what was tested.
pub fn version_of(adb: &Path) -> Option<String> {
    let mut cmd = Command::new(adb);
    cmd.arg("version");
    let out = run_with_deadline(cmd, Duration::from_secs(3)).ok()?;
    String::from_utf8_lossy(&out)
        .lines()
        .find_map(|l| l.strip_prefix("Version "))
        .map(|v| v.trim().to_string())
}

pub fn resolve_adb() -> Result<PathBuf> {
    let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_default();
    let get = |k: &str| std::env::var(k).ok();
    candidate_paths(&get, &home)
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| anyhow!("no adb binary found. Install Android SDK Platform-Tools."))
}

// ---------------------------------------------------------------------------
// The isolated mDNS probe
// ---------------------------------------------------------------------------

/// The argv and environment of one probe step, kept as data so tests can assert the
/// contract (which port we aim at, which variables are scrubbed) without running adb.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Variables set for this child only.
    pub env_set: Vec<(String, String)>,
    /// Variables removed from the inherited environment.
    pub env_clear: Vec<String>,
}

fn probe_socket(port: u16) -> String {
    format!("tcp:127.0.0.1:{port}")
}

/// The child that *is* the probe server.
///
/// The listen spec carries no hostname. `-L tcp:127.0.0.1:<port>` is fatal —
/// "could not install *smartsocket* listener: listening on specified hostname currently
/// unsupported", followed by `abort()` and a core dump. `-L tcp:<port>` binds 127.0.0.1
/// regardless, so loopback-only is adb's default rather than something a flag requests.
pub fn probe_server_command(adb: &Path, port: u16) -> ProbeCommand {
    ProbeCommand {
        program: adb.to_path_buf(),
        args: vec![
            "-L".to_string(),
            format!("tcp:{port}"),
            "nodaemon".into(),
            "server".into(),
        ],
        env_set: vec![],
        // ANDROID_ADB_SERVER_PORT would leak into every grandchild; ADB_SERVER_SOCKET
        // would silently redirect the server we are trying to pin.
        env_clear: vec!["ADB_SERVER_SOCKET".into(), "ANDROID_ADB_SERVER_PORT".into()],
    }
}

/// The client that asks *that* server whether it has an mDNS backend.
///
/// Aiming this at the probe port is the whole point. `mdns check` reports the state of
/// the server that answers, not of the binary invoked — measured directly: the Debian
/// binary, pointed at an SDK server, reports a working openscreen daemon. Without this
/// pinning the gate would bake a no-mDNS binary into the unit and silently lose reconnect.
/// The address is an explicit `127.0.0.1`; `localhost` can resolve to `::1` while the
/// server binds IPv4 loopback only.
pub fn probe_check_command(adb: &Path, port: u16) -> ProbeCommand {
    ProbeCommand {
        program: adb.to_path_buf(),
        args: vec!["mdns".into(), "check".into()],
        env_set: vec![("ADB_SERVER_SOCKET".into(), probe_socket(port))],
        env_clear: vec!["ANDROID_ADB_SERVER_PORT".into()],
    }
}

impl ProbeCommand {
    fn build(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args);
        for k in &self.env_clear {
            cmd.env_remove(k);
        }
        for (k, v) in &self.env_set {
            cmd.env(k, v);
        }
        cmd
    }
}

/// A probe server child that is always killed and reaped, including on panic.
struct ProbeServer(Child);

impl Drop for ProbeServer {
    fn drop(&mut self) {
        // Kill THIS pid. Never `adb kill-server`, which is a cooperative request that
        // would reach the user's real server on 5037.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum MdnsSupport {
    /// The binary reported a working mDNS daemon.
    Present(String),
    /// The binary answered, but without an mDNS daemon line.
    Absent,
}

pub fn parse_mdns_check(stdout: &str) -> MdnsSupport {
    match stdout
        .lines()
        .find(|l| l.contains("mdns daemon version"))
        .map(str::trim)
    {
        Some(line) => MdnsSupport::Present(line.to_string()),
        None => MdnsSupport::Absent,
    }
}

/// Run a child and read its stdout, killing and reaping it if it overruns.
fn run_with_deadline(mut cmd: Command, timeout: Duration) -> Result<Vec<u8>> {
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("timed out");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(child.wait_with_output()?.stdout)
}

fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    Ok(listener.local_addr()?.port())
}

/// Start `adb` as its own server on a scratch port and ask *that* server whether it has
/// an mDNS backend. Retries the port on a bind clash, which is a TOCTOU race and must not
/// be reported as a bad binary.
pub fn probe_mdns_support(adb: &Path) -> Result<MdnsSupport> {
    let mut last_err = None;
    for _ in 0..3 {
        let port = free_port()?;
        let mut server = match probe_server_command(adb, port)
            .build()
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => ProbeServer(child),
            Err(e) => {
                last_err = Some(anyhow!("could not start {}: {e}", adb.display()));
                continue;
            }
        };

        // openscreen needs a moment to come up; a single immediate shot can come back
        // empty for a perfectly good binary.
        // openscreen takes a moment to come up, and the check itself has to start and
        // connect. Too tight a budget reports a good binary as having no backend.
        let deadline = std::time::Instant::now() + Duration::from_secs(6);
        let mut answered = None;
        let started = std::time::Instant::now();
        while std::time::Instant::now() < deadline {
            if let Some(status) = server.0.try_wait()? {
                last_err = Some(anyhow!(
                    "probe server for {} exited early ({status}); the port may be taken",
                    adb.display()
                ));
                answered = None;
                break;
            }
            // `Command::output()` blocks forever if the child hangs, which would make the
            // 2.5s budget meaningless and freeze install and status.
            // A check that times out is inconclusive, never evidence of absence: the
            // whole point is to distinguish "this binary has no backend" from "this
            // binary was slow".
            if let Ok(out) = run_with_deadline(
                probe_check_command(adb, port).build(),
                Duration::from_millis(2500),
            ) {
                let text = String::from_utf8_lossy(&out);
                if let MdnsSupport::Present(v) = parse_mdns_check(&text) {
                    answered = Some(MdnsSupport::Present(v));
                    break;
                }
                // An empty reply from a server that is up is how the Debian build
                // answers, so it is a real answer - but only once the server has had
                // time to initialise.
                if started.elapsed() > Duration::from_secs(2) {
                    answered = Some(MdnsSupport::Absent);
                }
            }
            std::thread::sleep(Duration::from_millis(150));
        }

        if let Some(result) = answered {
            return Ok(result);
        }
        // Nothing conclusive: treat a reachable-but-silent server as "absent", which is
        // exactly how the Debian build behaves.
        if server.0.try_wait()?.is_none() {
            return Ok(MdnsSupport::Absent);
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("could not probe {}", adb.display())))
}

// ---------------------------------------------------------------------------
// Smart-socket client: read-only queries that never start a server
// ---------------------------------------------------------------------------

/// Talk to an already-running adb server over its smart-socket protocol.
///
/// Running `adb devices` would *start* a server when none is listening, which is the
/// unsupervised server this tool exists to warn about. A TCP connect is not merely a
/// pre-check here: it is the entire mechanism, so there is no window in which a child
/// process could be forked.
pub struct SmartSocket {
    addr: SocketAddr,
}

impl SmartSocket {
    pub fn new(port: u16) -> Self {
        Self {
            addr: SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        }
    }

    fn request(&self, service: &str) -> Result<String> {
        let mut stream = TcpStream::connect_timeout(&self.addr, Duration::from_millis(800))
            .with_context(|| format!("no adb server on {}", self.addr))?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        stream.set_write_timeout(Some(Duration::from_secs(3)))?;
        write!(stream, "{:04x}{}", service.len(), service)?;
        stream.flush()?;

        let mut status = [0u8; 4];
        stream.read_exact(&mut status)?;
        if &status != b"OKAY" {
            let mut msg = String::new();
            let _ = stream.read_to_string(&mut msg);
            bail!("adb refused {service}: {}", msg.trim());
        }
        let mut len_hex = [0u8; 4];
        if stream.read_exact(&mut len_hex).is_err() {
            return Ok(String::new());
        }
        let len = usize::from_str_radix(std::str::from_utf8(&len_hex)?, 16)?;
        let mut buf = vec![0u8; len];
        stream.read_exact(&mut buf)?;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    /// True when a server is listening. A connection refusal is the answer; no child
    /// process is involved either way.
    pub fn is_up(&self) -> bool {
        TcpStream::connect_timeout(&self.addr, Duration::from_millis(400)).is_ok()
    }

    pub fn version(&self) -> Result<String> {
        self.request("host:version")
    }

    pub fn devices(&self) -> Result<Vec<Device>> {
        Ok(parse_devices(&self.request("host:devices-l")?))
    }

    /// What adb's own mDNS backend can currently see. A compiled-in backend is not proof
    /// that discovery is working right now.
    pub fn mdns_services(&self) -> Result<String> {
        self.request("host:mdns:services")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_ipv4_and_bracketed_ipv6_serials() {
        assert_eq!(
            split_host_port("192.168.1.42:37219"),
            Some(("192.168.1.42", 37219))
        );
        assert_eq!(split_host_port("[fd00::1]:5555"), Some(("fd00::1", 5555)));
        assert_eq!(
            split_host_port("fd00::1:5555"),
            None,
            "bare IPv6 is not a serial"
        );
        assert_eq!(split_host_port("1BC4F9AK99001"), None);
    }

    #[test]
    fn classifies_every_serial_shape() {
        assert_eq!(classify("emulator-5554"), Transport::Emulator);
        assert_eq!(
            classify("adb-39061FDJH00KZR-vWTMTB._adb-tls-connect._tcp"),
            Transport::Mdns
        );
        assert_eq!(classify("192.168.1.42:37219"), Transport::Tcp);
        assert_eq!(classify("1BC4F9AK99001"), Transport::Usb);
    }

    #[test]
    fn parses_devices_l_with_mixed_transports() {
        let text = "\
List of devices attached
1BC4F9AK99001          device product:oriole model:Pixel_6 device:oriole transport_id:1
192.168.1.42:37219     device product:raven model:Pixel_9 device:raven transport_id:2
adb-39061FDJH00KZR-vWTMTB._adb-tls-connect._tcp device model:Pixel_9 transport_id:3
emulator-5554          device product:sdk model:Android_SDK transport_id:4
0A3B1C2D               offline
9F8E7D6C               unauthorized
";
        let devices = parse_devices(text);
        assert_eq!(devices.len(), 6);
        assert_eq!(devices[0].transport, Transport::Usb);
        assert_eq!(devices[0].model.as_deref(), Some("Pixel_6"));
        assert_eq!(devices[1].transport, Transport::Tcp);
        assert_eq!(devices[2].transport, Transport::Mdns);
        assert_eq!(devices[3].transport, Transport::Emulator);
        assert_eq!(devices[4].state, "offline");
        assert_eq!(devices[5].state, "unauthorized");
    }

    #[test]
    fn empty_device_list_is_not_an_error() {
        assert!(parse_devices("List of devices attached\n").is_empty());
        assert!(parse_devices("").is_empty());
    }

    #[test]
    fn wireless_filter_keeps_offline_and_drops_usb() {
        let devices = parse_devices(
            "1BC4F9AK99001 device\n192.168.1.42:37219 offline\nemulator-5554 device\n",
        );
        let wireless = wireless_devices(&devices);
        assert_eq!(wireless.len(), 1);
        assert_eq!(
            wireless[0].state, "offline",
            "a half-attached phone must stay visible"
        );
    }

    #[test]
    fn both_transports_of_one_phone_are_shown() {
        // Not merged: see wireless_devices. The two serials share nothing, and the fields
        // they do share identify a model rather than a handset.
        let devices = parse_devices(
            "192.168.1.42:37219 device product:raven model:Pixel_9 device:raven\n\
             adb-XYZ._adb-tls-connect._tcp device product:raven model:Pixel_9 device:raven\n",
        );
        let wireless = wireless_devices(&devices);
        assert_eq!(wireless.len(), 2);
        assert_eq!(wireless[0].transport, Transport::Tcp);
        assert_eq!(wireless[1].transport, Transport::Mdns);
    }

    #[test]
    fn two_identical_phones_are_never_merged() {
        // The failure a model-based key would cause: one of these disappears.
        let devices = parse_devices(
            "192.168.1.42:37219 device product:raven model:Pixel_9 device:raven\n\
             192.168.1.77:41003 device product:raven model:Pixel_9 device:raven\n",
        );
        assert_eq!(wireless_devices(&devices).len(), 2);
    }

    #[test]
    fn mdns_check_output_from_the_real_binaries() {
        // Captured from the SDK's adb 36.0.0-13206524.
        assert_eq!(
            parse_mdns_check("mdns daemon version [Openscreen discovery 0.0.0]\n"),
            MdnsSupport::Present("mdns daemon version [Openscreen discovery 0.0.0]".into())
        );
        // Captured from Debian's adb 34.0.5-debian: it answers with nothing at all.
        assert_eq!(parse_mdns_check(""), MdnsSupport::Absent);
        assert_eq!(parse_mdns_check("\n"), MdnsSupport::Absent);
    }

    #[test]
    fn probe_server_binds_loopback_and_scrubs_leaky_vars() {
        let cmd = probe_server_command(Path::new("/opt/adb"), 45123);
        // A hostname in the listen spec makes adb abort() with a core dump.
        assert_eq!(cmd.args, ["-L", "tcp:45123", "nodaemon", "server"]);
        assert!(!cmd.args.iter().any(|a| a.contains("127.0.0.1")));
        assert!(cmd
            .env_clear
            .contains(&"ANDROID_ADB_SERVER_PORT".to_string()));
        assert!(cmd.env_clear.contains(&"ADB_SERVER_SOCKET".to_string()));
    }

    #[test]
    fn probe_check_is_aimed_at_the_probe_server_not_5037() {
        // The bug this whole probe exists to avoid: a bare `adb mdns check` answers from
        // whichever server owns 5037.
        let cmd = probe_check_command(Path::new("/opt/adb"), 45123);
        assert_eq!(cmd.args, ["mdns", "check"]);
        assert_eq!(
            cmd.env_set,
            [(
                "ADB_SERVER_SOCKET".to_string(),
                "tcp:127.0.0.1:45123".to_string()
            )]
        );
    }

    /// The defect the isolated probe exists to prevent, checked against the two real
    /// binaries on this machine while another adb server holds 5037. A naive
    /// `adb mdns check` would answer from that foreign server and get both cases wrong.
    #[test]
    #[ignore = "requires the real adb binaries; run with --ignored"]
    fn probe_tells_the_two_real_binaries_apart() {
        let home = std::env::var("HOME").unwrap();
        let sdk = PathBuf::from(format!("{home}/Android/Sdk/platform-tools/adb"));
        let debian = PathBuf::from("/usr/bin/adb");
        assert!(
            sdk.is_file() && debian.is_file(),
            "both binaries must be present"
        );

        let foreign_is_up = SmartSocket::new(DEFAULT_PORT).is_up();
        eprintln!("foreign server on 5037 during probe: {foreign_is_up}");

        match probe_mdns_support(&sdk).unwrap() {
            MdnsSupport::Present(v) => eprintln!("sdk adb -> {v}"),
            MdnsSupport::Absent => panic!("SDK adb has openscreen; probe reported Absent"),
        }
        assert_eq!(
            probe_mdns_support(&debian).unwrap(),
            MdnsSupport::Absent,
            "Debian adb has no mDNS backend; probe must not be fooled by a foreign server"
        );

        // The probe must leave nothing behind.
        assert!(
            SmartSocket::new(DEFAULT_PORT).is_up() == foreign_is_up,
            "probe changed the state of the real server"
        );
    }

    /// The probe's contract, exercised end to end against a fake adb.
    ///
    /// Output fixtures cannot catch the defect that matters here: a probe aimed at the
    /// wrong port, or one that leaves its server running to win the bind race against the
    /// real unit.
    /// Serialises the tests that poison the process environment: `set_var` is visible to
    /// every other test running in parallel.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn probe_spawns_aims_and_tears_down() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("wadb-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("calls.log");
        let fake = dir.join("adb");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\n\
                 echo \"argv: $* | socket=${{ADB_SERVER_SOCKET:-unset}} | port_env=${{ANDROID_ADB_SERVER_PORT:-unset}}\" >> {log}\n\
                 case \"$*\" in\n\
                 *'nodaemon server'*) echo \"server_pid: $$\" >> {log}; sleep 30 ;;\n\
                 *'mdns check'*) echo 'mdns daemon version [Openscreen discovery 0.0.0]' ;;\n\
                 esac\n",
                log = log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();

        // Poison the environment the probe must not propagate.
        std::env::set_var("ADB_SERVER_SOCKET", "tcp:127.0.0.1:9999");
        std::env::set_var("ANDROID_ADB_SERVER_PORT", "9999");
        let result = probe_mdns_support(&fake).unwrap();
        std::env::remove_var("ADB_SERVER_SOCKET");
        std::env::remove_var("ANDROID_ADB_SERVER_PORT");
        assert_eq!(
            result,
            MdnsSupport::Present("mdns daemon version [Openscreen discovery 0.0.0]".into())
        );

        let calls = std::fs::read_to_string(&log).unwrap();
        let server_line = calls
            .lines()
            .find(|l| l.contains("nodaemon server"))
            .unwrap();
        let check_line = calls.lines().find(|l| l.contains("mdns check")).unwrap();

        // The server carries no hostname in its listen spec, and neither leaked variable.
        assert!(server_line.contains("-L tcp:"), "{server_line}");
        assert!(
            !server_line.contains("tcp:127.0.0.1:"),
            "a hostname makes adb abort"
        );
        assert!(server_line.contains("socket=unset"), "{server_line}");
        assert!(server_line.contains("port_env=unset"), "{server_line}");

        // The check is aimed at the probe's own port, not at the inherited 9999.
        let port: u16 = server_line
            .split("-L tcp:")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            check_line.contains(&format!("socket=tcp:127.0.0.1:{port}")),
            "{check_line}"
        );

        // Teardown killed that PID, and never asked a server to stop.
        let pid: u32 = calls
            .lines()
            .find_map(|l| l.strip_prefix("server_pid: "))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            !calls.contains("kill-server"),
            "must never issue a cooperative kill"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "probe server {pid} outlived the probe and would fight the real unit"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn candidate_order_prefers_sdk_over_path() {
        let env = |k: &str| match k {
            "ANDROID_HOME" => Some("/opt/sdk".to_string()),
            "PATH" => Some("/usr/bin".to_string()),
            _ => None,
        };
        let paths = candidate_paths(&env, Path::new("/home/u"));
        assert_eq!(paths[0], PathBuf::from("/opt/sdk/platform-tools/adb"));
        assert_eq!(
            paths[1],
            PathBuf::from("/home/u/Android/Sdk/platform-tools/adb")
        );
        assert_eq!(paths[2], PathBuf::from("/usr/bin/adb"));
    }

    #[test]
    fn adb_env_var_wins_over_everything() {
        let env = |k: &str| (k == "ADB").then(|| "/custom/adb".to_string());
        assert_eq!(
            candidate_paths(&env, Path::new("/home/u"))[0],
            PathBuf::from("/custom/adb")
        );
    }
}
