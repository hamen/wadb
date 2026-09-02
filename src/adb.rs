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

/// Are these the same executable?
///
/// `/proc/<pid>/exe` is already resolved, so a candidate reached through a symlink - here the SDK
/// lives under a `~/Android` symlink into another filesystem - never string-matches it. Comparing
/// the raw paths silently disables the fast path and sends every check down the flaky probe.
fn same_binary(running: Option<&Path>, candidate: &Path) -> bool {
    let Some(running) = running else { return false };
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(running) == canon(candidate)
}

/// Serialises anything needing the host's mDNS socket. Only one process can hold it, so a browse
/// running beside a probe makes the probe's server come up without openscreen and report a good
/// binary as having no backend.
#[cfg(test)]
pub static MDNS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The executable behind a listening adb server, so we can tell whether the server already
/// running is the very binary we are about to gate.
pub fn parse_ss_pid(text: &str) -> Option<u32> {
    let (_, rest) = text.split_once("pid=")?;
    rest.split(|c: char| !c.is_ascii_digit())
        .find(|t| !t.is_empty())?
        .parse()
        .ok()
}

/// What is behind a listening socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listener {
    pub pid: u32,
    pub exe: PathBuf,
    /// The running image no longer exists at that path: `/proc/<pid>/exe` read `… (deleted)`,
    /// which is what an SDK update does to a server nobody restarted.
    pub replaced: bool,
}

/// Read a process's executable, and whether the file behind it is gone.
///
/// The `(deleted)` case must not be smoothed over. Stripping the suffix and comparing paths makes
/// an *old* running image look identical to the *new* file now at that path, so a good old server
/// would vouch for a new binary nobody has checked. Refusing on it instead brings back the false
/// refusal. Neither is true, so the caller is told the identity is unknown.
pub fn exe_of(pid: u32) -> Option<(PathBuf, bool)> {
    let link = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    let text = link.to_string_lossy().into_owned();
    Some(match text.strip_suffix(" (deleted)") {
        Some(real) => (PathBuf::from(real), true),
        None => (link, false),
    })
}

pub fn listener(port: u16) -> Option<Listener> {
    let out = Command::new("ss")
        .args(["-ltnpH", "sport", "=", &format!(":{port}")])
        .output()
        .ok()?;
    let pid = parse_ss_pid(&String::from_utf8_lossy(&out.stdout))?;
    let (exe, replaced) = exe_of(pid)?;
    Some(Listener { pid, exe, replaced })
}

/// Does this binary have an mDNS backend?
///
/// Asks the server already running on `port` when that server *is* this binary, and only spawns
/// an isolated probe otherwise.
///
/// The order matters, and getting it wrong is how this went wrong before. Only one adb server can
/// hold the mDNS socket at a time, so a probe server started while a working server is up cannot
/// initialise openscreen and answers empty — which the gate then reads as "this binary has no
/// backend" and refuses a perfectly good adb. That is the same class of error the isolated probe
/// exists to prevent, arriving from the other direction: the first version trusted a server that
/// was not this binary, this one distrusted a server that was.
pub fn mdns_support(adb: &Path, port: u16) -> Result<MdnsSupport> {
    let sock = SmartSocket::new(port);
    let before = sock.is_up().then(|| listener(port)).flatten();

    let answer = before.as_ref().and_then(|_| sock.mdns_check().ok());
    // Sampled again after the answer, and compared in full: an update during the request keeps the
    // pid and only flips `replaced`.
    let after = sock.is_up().then(|| listener(port)).flatten();

    match gate_step(before.as_ref(), after.as_ref(), adb) {
        GateStep::Trust => {
            if let Some(answer) = answer {
                return Ok(answer);
            }
            // Attributable server, but it would not answer. A probe cannot take the socket from
            // it either, so there is nothing true to report.
            bail!(
                "the adb server on port {port} did not answer an mDNS check, and a probe cannot \
                 take the socket while it is running.\n\
                 Restart it and re-run: systemctl --user restart wadb"
            );
        }
        GateStep::Probe => probe_mdns_support(adb),
        GateStep::Indeterminate => {
            let l = after.as_ref().or(before.as_ref());
            bail!(
                "an adb server on port {port}{} holds the mDNS socket and cannot be attributed to \
                 {}, so its support cannot be established and a probe cannot take the socket from \
                 it.\n\
                 Stop or restart that server and re-run: systemctl --user restart wadb",
                l.map(|l| format!(" (pid {})", l.pid)).unwrap_or_default(),
                adb.display()
            );
        }
    }
}

/// What the gate should do about the server currently on the port.
#[derive(Debug, PartialEq, Eq)]
pub enum GateStep {
    /// Take this server's answer for the candidate binary.
    Trust,
    /// Ask an isolated probe instead.
    Probe,
    /// Neither is trustworthy; tell the user to restart the server.
    Indeterminate,
}

/// Can this server's answer be attributed to the candidate binary?
fn attributable(l: &Listener, adb: &Path) -> bool {
    !l.replaced && same_binary(Some(&l.exe), adb)
}

/// Kept pure so every branch is testable without a network or an adb.
///
/// The rule the previous three versions of this gate each missed one half of: **a probe is only
/// safe when nothing holds the mDNS socket.** A live server we cannot attribute to this binary
/// makes the question unanswerable — the probe underneath it would report `Absent` for a perfectly
/// good adb — so the honest outcome is `Indeterminate`, not a guess in either direction.
///
/// Both snapshots are compared in full, not just by pid: an SDK update during the request keeps
/// the pid and only flips `replaced`, which is how an old image came to vouch for a new file.
///
/// An `Absent` from a server that *is* this binary is trusted, not retried: a probe started
/// underneath it could not take the socket and would answer `Absent` too.
pub fn gate_step(before: Option<&Listener>, after: Option<&Listener>, adb: &Path) -> GateStep {
    let Some(after) = after else {
        // Nothing holds the socket now, so an isolated probe can actually get it.
        return GateStep::Probe;
    };
    match before {
        Some(before)
            if before.pid == after.pid && attributable(before, adb) && attributable(after, adb) =>
        {
            GateStep::Trust
        }
        _ => GateStep::Indeterminate,
    }
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
        self.request_with_timeout(service, Duration::from_secs(3))
    }

    fn request_with_timeout(&self, service: &str, read_timeout: Duration) -> Result<String> {
        let mut stream = TcpStream::connect_timeout(&self.addr, Duration::from_millis(800))
            .with_context(|| format!("no adb server on {}", self.addr))?;
        stream.set_read_timeout(Some(read_timeout))?;
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

    /// Attach a device, over the smart socket rather than by running `adb connect`.
    ///
    /// This is not a style preference. `adb connect` starts a server when none is listening, and
    /// the server can stop between a liveness check and the child actually running — which forks
    /// an unmanaged server that takes the port and locks our unit out. That failure was observed
    /// live on this machine. A request on an already-open socket cannot start anything: if the
    /// server is gone, the request simply fails.
    pub fn connect_device(&self, endpoint: &str) -> Result<String> {
        // adb blocks while it dials the phone, so this needs longer than a status query.
        self.request_with_timeout(&format!("host:connect:{endpoint}"), Duration::from_secs(12))
    }

    /// Ask a *running* server whether it has an mDNS backend, over the smart socket.
    ///
    /// This is the same question `adb mdns check` asks, without a child process.
    pub fn mdns_check(&self) -> Result<MdnsSupport> {
        Ok(parse_mdns_check(&self.request("host:mdns:check")?))
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
        let _mdns = super::MDNS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::var("HOME").unwrap();
        let sdk = PathBuf::from(format!("{home}/Android/Sdk/platform-tools/adb"));
        let debian = PathBuf::from("/usr/bin/adb");
        assert!(
            sdk.is_file() && debian.is_file(),
            "both binaries must be present"
        );

        // The point of this test is that a server already holding the mDNS socket does not make
        // the good binary look backend-less. With nothing on the port, or with the *other*
        // binary holding it, it proves nothing: the SDK candidate would fail the identity check,
        // the probe could not take the socket, and Absent would be correct rather than a bug.
        let server_is_up = SmartSocket::new(DEFAULT_PORT).is_up();
        assert!(
            server_is_up,
            "start an adb server on {DEFAULT_PORT} first, or this test passes vacuously"
        );
        let holder = listener(DEFAULT_PORT).map(|l| l.exe);
        assert!(
            same_binary(holder.as_deref(), &sdk),
            "the server on {DEFAULT_PORT} must be the SDK adb for this test to mean anything, \
             found {holder:?}"
        );

        // Through the real entry point: a server already holding the mDNS socket must not make
        // the good binary look backend-less, which is exactly what the bare probe did.
        match mdns_support(&sdk, DEFAULT_PORT).unwrap() {
            MdnsSupport::Present(v) => eprintln!("sdk adb -> {v}"),
            MdnsSupport::Absent => panic!("SDK adb has openscreen; gate reported Absent"),
        }
        assert_eq!(
            mdns_support(&debian, DEFAULT_PORT).unwrap(),
            MdnsSupport::Absent,
            "Debian adb has no mDNS backend; probe must not be fooled by a foreign server"
        );

        // The probe must leave nothing behind.
        assert!(
            SmartSocket::new(DEFAULT_PORT).is_up() == server_is_up,
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
    fn reads_the_pid_out_of_ss_output() {
        // Canned, because the live path is only exercised by an ignored hardware test.
        let line = "LISTEN 0 4096 127.0.0.1:5037 0.0.0.0:* users:((\"adb\",pid=2466481,fd=10))";
        assert_eq!(parse_ss_pid(line), Some(2466481));
        // `ss` without -p, or a socket owned by another user, prints no pid at all.
        assert_eq!(parse_ss_pid("LISTEN 0 4096 127.0.0.1:5037 0.0.0.0:*"), None);
        assert_eq!(parse_ss_pid(""), None);
    }

    #[test]
    fn listener_exe_finds_the_process_holding_a_port() {
        // Deterministic coverage of the live path: this test process is the listener.
        let held = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = held.local_addr().unwrap().port();
        let found = listener(port).map(|l| l.exe);
        if let Some(found) = found {
            let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
            assert_eq!(canon(&found), canon(&std::env::current_exe().unwrap()));
        }
        // A None result is legitimate where `ss` is absent; it degrades to the isolated probe.
    }

    #[test]
    fn a_replaced_binary_is_reported_as_replaced() {
        // Drives exe_of's `replaced` branch for real, by unlinking a running binary. The previous
        // version of this test hand-rolled the suffix strip instead, which is exactly why it did
        // not catch that stripping it let an old image vouch for the new file at that path.
        let dir = std::env::temp_dir().join(format!("wadb-del-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("sleeper");
        std::fs::copy("/bin/sleep", &bin).unwrap();

        // Spawning a file this process just wrote can fail with ETXTBSY while another thread
        // still holds a write descriptor to it — the copy is closed here, but a concurrent test
        // forking inherits open descriptors, so the kernel can still see the image as busy.
        // Retry briefly rather than making the suite order-dependent.
        let mut child = loop {
            match Command::new(&bin).arg("30").spawn() {
                Ok(child) => break child,
                Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => panic!("could not start the throwaway binary: {e}"),
            }
        };
        let pid = child.id();

        let (exe, replaced) = exe_of(pid).expect("a running child has a readable exe");
        assert_eq!(exe, bin);
        assert!(!replaced, "still on disk");

        // The update: the file is replaced under the running process.
        std::fs::remove_file(&bin).unwrap();
        let (exe, replaced) = exe_of(pid).expect("exe stays readable after the file goes");
        assert!(replaced, "/proc reports the image as deleted");
        assert_eq!(exe, bin, "and the suffix is not part of the path");

        // Which must make the gate refuse to answer rather than trust the stale image.
        let l = Listener { pid, exe, replaced };
        assert_eq!(gate_step(Some(&l), Some(&l), &bin), GateStep::Indeterminate);

        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_gate_trusts_probes_or_gives_up_for_the_right_reasons() {
        let adb = Path::new("/opt/sdk/adb");
        let ours = |pid, replaced| Listener {
            pid,
            exe: adb.to_path_buf(),
            replaced,
        };
        let other = |pid| Listener {
            pid,
            exe: PathBuf::from("/usr/bin/adb"),
            replaced: false,
        };

        // This binary, same process, intact at both samples: take its word.
        assert_eq!(
            gate_step(Some(&ours(42, false)), Some(&ours(42, false)), adb),
            GateStep::Trust
        );

        // Nothing holds the socket at the end: a probe can actually get it.
        assert_eq!(gate_step(None, None, adb), GateStep::Probe);
        assert_eq!(
            gate_step(Some(&ours(42, false)), None, adb),
            GateStep::Probe
        );

        // Replaced *during* the request: same pid, only `replaced` flips. This is the case that
        // let an old image vouch for the new file at that path, and comparing pids alone missed it.
        assert_eq!(
            gate_step(Some(&ours(42, false)), Some(&ours(42, true)), adb),
            GateStep::Indeterminate
        );
        // Replaced before we started.
        assert_eq!(
            gate_step(Some(&ours(42, true)), Some(&ours(42, true)), adb),
            GateStep::Indeterminate
        );
        // A different binary holds the socket. Probing under it would report Absent for a good
        // candidate, so this is unanswerable rather than a probe.
        assert_eq!(
            gate_step(Some(&other(42)), Some(&other(42)), adb),
            GateStep::Indeterminate
        );
        // The process changed under us.
        assert_eq!(
            gate_step(Some(&ours(42, false)), Some(&ours(99, false)), adb),
            GateStep::Indeterminate
        );
        // A server appeared during the check: it now owns the socket, so no probe.
        assert_eq!(
            gate_step(None, Some(&ours(42, false)), adb),
            GateStep::Indeterminate
        );
    }

    #[test]
    fn the_socket_body_parses_like_the_command_output() {
        // Captured from `host:mdns:check` on the smart socket, which is what the fast path reads.
        assert_eq!(
            parse_mdns_check("mdns daemon version [Openscreen discovery 0.0.0]"),
            MdnsSupport::Present("mdns daemon version [Openscreen discovery 0.0.0]".into())
        );
    }

    #[test]
    fn a_symlinked_path_is_recognised_as_the_same_binary() {
        // The SDK here is reached through a ~/Android symlink onto another filesystem, while
        // /proc/<pid>/exe reports the resolved path.
        let dir = std::env::temp_dir().join(format!("wadb-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("real")).unwrap();
        let real = dir.join("real/adb");
        std::fs::write(&real, "#!/bin/sh\n").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(dir.join("real"), &link).unwrap();

        assert!(same_binary(Some(&real), &link.join("adb")));
        assert!(!same_binary(Some(&real), Path::new("/usr/bin/adb")));
        assert!(!same_binary(None, &real));
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
