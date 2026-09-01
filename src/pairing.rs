// SPDX-License-Identifier: Apache-2.0

//! Generating a pairing payload, finding the phone that scanned it, and pairing.

use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use zeroize::Zeroize;

/// Android Studio's convention for the instance name it puts in the QR. Kept for
/// compatibility; this is a convention, not an OS requirement.
const PREFIX: &str = "studio-";
const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub const PAIRING_SERVICE: &str = "_adb-tls-pairing._tcp.local.";
pub const CONNECT_SERVICE: &str = "_adb-tls-connect._tcp.local.";

/// A secret that is wiped when it goes out of scope, including on an early `?` return.
pub struct Secret(String);

impl Secret {
    pub fn new(s: String) -> Self {
        Self(s)
    }
    pub fn as_str(&self) -> &str {
        self.0.trim()
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// A pairing credential. The password is written to a child's stdin and never appears in
/// argv, which is world-readable through `/proc/<pid>/cmdline`.
pub struct Payload {
    instance: String,
    password: String,
}

impl Drop for Payload {
    fn drop(&mut self) {
        self.password.zeroize();
        self.instance.zeroize();
    }
}

fn random_token(len: usize) -> Result<String> {
    use rand::TryRngCore;
    let mut raw = vec![0u8; len];
    rand::rngs::OsRng
        .try_fill_bytes(&mut raw)
        .map_err(|e| anyhow!("secure random generation failed: {e}"))?;
    let token = raw
        .iter()
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect();
    raw.zeroize();
    Ok(token)
}

impl Payload {
    pub fn random() -> Result<Self> {
        let payload = Self {
            instance: format!("{PREFIX}{}", random_token(10)?),
            password: random_token(10)?,
        };
        // A payload the phone cannot parse would fail as a silent no-scan.
        if !is_valid_qr_text(&payload.qr_text()) {
            bail!("generated an invalid pairing payload");
        }
        Ok(payload)
    }

    pub fn instance(&self) -> &str {
        &self.instance
    }

    /// Only ever handed to a child's stdin.
    pub fn password(&self) -> &str {
        &self.password
    }

    /// The string encoded into the QR the phone scans.
    pub fn qr_text(&self) -> String {
        format!("WIFI:T:ADB;S:{};P:{};;", self.instance, self.password)
    }

    /// Does a discovered mDNS service belong to the phone that scanned our code?
    ///
    /// Matched against the service's *instance* component, not the full DNS-SD name.
    pub fn matches_instance(&self, service_name: &str) -> bool {
        let instance = service_name
            .split_once("._adb-tls-pairing")
            .map(|(i, _)| i)
            .unwrap_or(service_name);
        instance == self.instance()
    }
}

pub fn is_valid_qr_text(text: &str) -> bool {
    let Some(body) = text
        .strip_prefix("WIFI:T:ADB;S:")
        .and_then(|b| b.strip_suffix(";;"))
    else {
        return false;
    };
    let Some((name, password)) = body.split_once(";P:") else {
        return false;
    };
    name.starts_with(PREFIX)
        && !password.is_empty()
        && [name, password]
            .iter()
            .all(|s| s.bytes().all(|b| ALPHABET.contains(&b)))
}

// ---------------------------------------------------------------------------
// Running adb pair / connect
// ---------------------------------------------------------------------------

/// Addresses to try, in order. Link-local IPv6 is dropped outright: adb cannot use a
/// scope id. IPv4 is preferred, but a global IPv6 is a usable fallback rather than a
/// dead end.
pub fn usable_addresses(addrs: &[IpAddr]) -> Vec<IpAddr> {
    let mut v4: Vec<IpAddr> = addrs.iter().copied().filter(IpAddr::is_ipv4).collect();
    let v6 = addrs.iter().copied().filter(|a| match a {
        IpAddr::V6(a) => !(a.is_loopback() || is_link_local_v6(a)),
        IpAddr::V4(_) => false,
    });
    v4.extend(v6);
    v4
}

fn is_link_local_v6(a: &std::net::Ipv6Addr) -> bool {
    a.segments()[0] & 0xffc0 == 0xfe80
}

/// `host:port` for adb, bracketing IPv6 literals.
pub fn endpoint(addr: IpAddr, port: u16) -> String {
    match addr {
        IpAddr::V4(a) => format!("{a}:{port}"),
        IpAddr::V6(a) => format!("[{a}]:{port}"),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum PairOutcome {
    /// Paired. Carries the guid adb reports, when it reports one.
    Paired {
        guid: Option<String>,
    },
    WrongCode,
    /// The phone's pairing window closed, or nothing was listening.
    Unreachable,
    /// Unrecognised output. `success` is the child's exit status, which decides whether a
    /// connect fallback is justified: unknown wording after a *failed* run is a failure.
    Other {
        msg: String,
        success: bool,
    },
}

/// Read `adb pair`'s result. A zero exit alone is not proof — some builds exit zero on a
/// refused code — and some failures exit non-zero with terse output, so both are read.
pub fn parse_pair_output(stdout: &str, stderr: &str, success_exit: bool) -> PairOutcome {
    let text = format!("{stdout}\n{stderr}");
    let lower = text.to_lowercase();
    // Both must agree. Output alone would accept a run that adb reported as failed.
    if success_exit && lower.contains("successfully paired") {
        // adb reports it as `[guid=adb-XXXX-YYYY]`; older builds print a bare token.
        let guid = text
            .split_once("guid=")
            .map(|(_, rest)| {
                rest.trim_start()
                    .split(|c: char| c.is_whitespace() || c == ']')
                    .next()
                    .unwrap_or("")
                    .to_string()
            })
            .or_else(|| {
                text.split_whitespace()
                    .find(|t| t.starts_with("adb-"))
                    .map(|t| t.trim_matches(['[', ']', '.', ',']).to_string())
            })
            .filter(|g| !g.is_empty());
        return PairOutcome::Paired { guid };
    }
    if lower.contains("wrong")
        || lower.contains("incorrect")
        || lower.contains("failed to authenticate")
    {
        return PairOutcome::WrongCode;
    }
    if lower.contains("connection refused")
        || lower.contains("protocol fault")
        || lower.contains("cannot connect")
        || lower.contains("no pairing code")
    {
        return PairOutcome::Unreachable;
    }
    if success_exit && lower.trim().is_empty() {
        // Silence with a zero exit is not success.
        return PairOutcome::Other {
            msg: "adb pair said nothing".into(),
            success: false,
        };
    }
    PairOutcome::Other {
        msg: text.trim().to_string(),
        success: success_exit,
    }
}

fn adb_command(adb: &Path, port: u16) -> Command {
    let mut cmd = Command::new(adb);
    // Never inherit a socket that would silently redirect us to another server, and be
    // explicit about auto-connect rather than depending on the user manager's env.
    cmd.env_remove("ADB_SERVER_SOCKET");
    cmd.env("ANDROID_ADB_SERVER_PORT", port.to_string());
    cmd.env("ADB_MDNS_AUTO_CONNECT", "adb-tls-connect");
    cmd
}

/// Is this outcome one the caller should follow with a connect attempt?
///
/// Both the QR worker and the CLI must answer this the same way, or a real pairing reads
/// as a failure on one path and not the other.
pub fn should_attempt_connect(outcome: &PairOutcome) -> bool {
    matches!(
        outcome,
        PairOutcome::Paired { .. } | PairOutcome::Other { success: true, .. }
    )
}

/// Run a child with a deadline, killing and reaping it on expiry so a hung adb cannot
/// block the pairing worker.
/// Run a child with a deadline *and* a cancel flag.
///
/// A flag the worker only reads between steps cannot stop an `adb pair` already in
/// flight, which leaves a twenty second window where a cancelled pairing still completes.
/// The child is killed as soon as the flag is set.
fn run_bounded_cancellable(
    mut cmd: Command,
    stdin_data: Option<&str>,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<(String, String, bool)> {
    cmd.stdin(if stdin_data.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("could not run adb")?;
    if let Some(data) = stdin_data {
        let mut pipe = child.stdin.take().ok_or_else(|| anyhow!("no stdin pipe"))?;
        pipe.write_all(data.as_bytes())?;
        // Dropping the pipe closes it. adb waits for EOF on stdin, so leaving it open
        // turns every pairing into a timeout.
        drop(pipe);
    }
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("cancelled");
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("adb timed out after {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = child.wait_with_output()?;
    Ok((
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    ))
}

/// Pair with a phone. The code goes in on stdin only.
pub fn pair(adb: &Path, port: u16, endpoint: &str, code: &str) -> Result<PairOutcome> {
    pair_cancellable(adb, port, endpoint, code, &AtomicBool::new(false))
}

pub fn pair_cancellable(
    adb: &Path,
    port: u16,
    endpoint: &str,
    code: &str,
    cancel: &AtomicBool,
) -> Result<PairOutcome> {
    let argv = pair_argv(adb, endpoint);
    let mut cmd = adb_command(adb, port);
    cmd.args(&argv[1..]);
    let mut line = format!("{code}\n");
    let result = run_bounded_cancellable(cmd, Some(&line), Duration::from_secs(20), cancel);
    line.zeroize();
    let (stdout, stderr, ok) = result?;
    Ok(parse_pair_output(&stdout, &stderr, ok))
}

/// `adb connect` reports failure in its output as well as its exit status, and on some
/// builds only in the output ("failed to connect to ..." with a zero exit).
pub fn parse_connect_output(text: &str, success_exit: bool) -> Result<String> {
    let trimmed = text.trim();
    let lower = trimmed.to_lowercase();
    if !success_exit
        || trimmed.is_empty()
        || lower.starts_with("failed to connect")
        || lower.contains("cannot connect")
        || lower.contains("connection refused")
    {
        bail!(
            "{}",
            if trimmed.is_empty() {
                "adb connect failed"
            } else {
                trimmed
            }
        );
    }
    Ok(trimmed.to_string())
}

pub fn connect_cancellable(
    adb: &Path,
    port: u16,
    endpoint: &str,
    cancel: &AtomicBool,
) -> Result<String> {
    let mut cmd = adb_command(adb, port);
    cmd.args(["connect", endpoint]);
    // A bad address blocks the worker just as effectively as a bad code.
    let (stdout, stderr, ok) = run_bounded_cancellable(cmd, None, Duration::from_secs(10), cancel)?;
    let text = format!("{}\n{}", stdout.trim(), stderr.trim());
    parse_connect_output(&text, ok)
}

/// The argv `adb pair` is invoked with. `pair` builds its command from this, so the test
/// asserting the password is absent covers the command that actually runs.
pub fn pair_argv(adb: &Path, endpoint: &str) -> Vec<PathBuf> {
    vec![adb.to_path_buf(), "pair".into(), endpoint.into()]
}

// ---------------------------------------------------------------------------
// The pairing worker
// ---------------------------------------------------------------------------

/// What the worker reports back to the UI.
#[derive(Debug, Clone)]
pub enum PairEvent {
    Found(String),
    Connected(String),
    Failed(String),
}

/// Browse for the phone that scanned our code, pair with it, then connect.
///
/// Pairing and connecting are different services on different ports, so a successful
/// `adb pair` is not an attached device: a connect must follow, matched on the guid adb
/// reported or on the host we paired with. Matching on the QR name would find nothing,
/// and taking the first connect service on the LAN could attach somebody else's phone.
pub fn run_pairing(
    adb: PathBuf,
    port: u16,
    payload: Payload,
    timeout: Duration,
    cancel: std::sync::Arc<AtomicBool>,
    tx: std::sync::mpsc::Sender<PairEvent>,
) {
    let cancelled = || cancel.load(Ordering::Relaxed);
    let found = match crate::discovery::browse_cancellable(PAIRING_SERVICE, timeout, &cancel, |f| {
        payload.matches_instance(&f.fullname)
    }) {
        Ok(Some(f)) => f,
        Ok(None) => {
            let _ = tx.send(PairEvent::Failed(
                "no phone scanned the code in time".into(),
            ));
            return;
        }
        Err(e) => {
            let _ = tx.send(PairEvent::Failed(format!("mDNS discovery failed: {e}")));
            return;
        }
    };

    if cancelled() {
        return;
    }
    let addresses = usable_addresses(&found.addresses);
    if addresses.is_empty() {
        let _ = tx.send(PairEvent::Failed(
            "phone advertised no usable address".into(),
        ));
        return;
    }
    let _ = tx.send(PairEvent::Found(addresses[0].to_string()));

    let mut last = String::from("no usable address");
    for addr in &addresses {
        // Checked before each attempt: pairing a phone the user has cancelled would be
        // worse than doing nothing.
        if cancelled() {
            return;
        }
        let ep = endpoint(*addr, found.port);
        match pair_cancellable(&adb, port, &ep, payload.password(), &cancel) {
            Ok(PairOutcome::Paired { guid }) => {
                if cancelled() {
                    return;
                }
                match connect_paired_cancellable(&adb, port, *addr, guid.as_deref(), &cancel) {
                    Ok(msg) => {
                        let _ = tx.send(PairEvent::Connected(msg));
                    }
                    Err(e) => {
                        let _ = tx.send(PairEvent::Failed(format!(
                            "paired, but connect failed: {e}"
                        )));
                    }
                }
                return;
            }
            Ok(PairOutcome::WrongCode) => {
                let _ = tx.send(PairEvent::Failed("the phone rejected the code".into()));
                return;
            }
            // Wording varies across adb versions. A run that did not clearly fail is
            // worth a connect attempt: if it really failed, the connect fails too, and
            // the user sees that rather than a false "pairing failed".
            // Only unknown output from a *successful* run earns a connect attempt.
            Ok(outcome @ PairOutcome::Other { .. }) if should_attempt_connect(&outcome) => {
                if cancelled() {
                    return;
                }
                let PairOutcome::Other { msg, .. } = outcome else {
                    unreachable!()
                };
                if let Ok(m) = connect_paired_cancellable(&adb, port, *addr, None, &cancel) {
                    let _ = tx.send(PairEvent::Connected(m));
                    return;
                }
                last = msg;
            }
            Ok(other) => last = format!("{other:?}"),
            Err(e) => last = e.to_string(),
        }
    }
    let _ = tx.send(PairEvent::Failed(last));
}

/// Find the connect service for the phone we just paired with, and attach it.
///
/// The guid is tried first, then the host we paired with. A guid that does not match any
/// advertised instance must not end the attempt: adb's wording varies between versions,
/// so a parsed guid can be wrong while the address is still right.
pub fn connect_paired(adb: &Path, port: u16, host: IpAddr, guid: Option<&str>) -> Result<String> {
    connect_paired_cancellable(adb, port, host, guid, &AtomicBool::new(false))
}

/// As above, but abandons the browse when the user cancels. Without this a cancel that
/// lands after `adb pair` succeeded still spends up to twelve seconds browsing, and then
/// attaches a phone the user asked us not to.
pub fn connect_paired_cancellable(
    adb: &Path,
    port: u16,
    host: IpAddr,
    guid: Option<&str>,
    cancel: &AtomicBool,
) -> Result<String> {
    let mut found = None;
    if let Some(g) = guid {
        found = crate::discovery::browse_cancellable(
            CONNECT_SERVICE,
            Duration::from_secs(6),
            cancel,
            |f| f.instance() == g,
        )?;
    }
    if found.is_none() && !cancel.load(Ordering::Relaxed) {
        found = crate::discovery::browse_cancellable(
            CONNECT_SERVICE,
            Duration::from_secs(6),
            cancel,
            |f| f.addresses.contains(&host),
        )?;
    }
    if cancel.load(Ordering::Relaxed) {
        bail!("cancelled");
    }

    let ep = match found {
        Some(f) => {
            let addr = usable_addresses(&f.addresses)
                .into_iter()
                .next()
                .unwrap_or(host);
            endpoint(addr, f.port)
        }
        None => bail!("paired, but the phone never advertised a connect service"),
    };
    connect_cancellable(adb, port, &ep, cancel)
}

/// Same, from a textual host as typed on the command line.
pub fn connect_after_pair(adb: &Path, port: u16, host: &str, guid: Option<&str>) -> Result<String> {
    let addr: IpAddr = host
        .parse()
        .with_context(|| format!("{host} is not an IP address"))?;
    connect_paired(adb, port, addr, guid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn payload_shape_matches_what_android_expects() {
        let p = Payload::random().unwrap();
        let text = p.qr_text();
        assert!(text.starts_with("WIFI:T:ADB;S:studio-"));
        assert!(text.ends_with(";;"));
        assert!(is_valid_qr_text(&text));
        assert_eq!(p.instance().len(), PREFIX.len() + 10);
    }

    #[test]
    fn two_payloads_differ() {
        assert_ne!(
            Payload::random().unwrap().qr_text(),
            Payload::random().unwrap().qr_text()
        );
    }

    #[test]
    fn rejects_malformed_or_unsafe_payloads() {
        assert!(
            !is_valid_qr_text("WIFI:T:ADB;S:studio-abc;P:;;"),
            "empty password"
        );
        assert!(
            !is_valid_qr_text("WIFI:T:ADB;S:nope-abc;P:xyz;;"),
            "missing studio- prefix"
        );
        assert!(
            !is_valid_qr_text("WIFI:T:ADB;S:studio-a b;P:xyz;;"),
            "space is not in the alphabet"
        );
        assert!(
            !is_valid_qr_text("WIFI:T:WPA;S:studio-a;P:x;;"),
            "not an ADB payload"
        );
        assert!(!is_valid_qr_text("garbage"));
    }

    #[test]
    fn matches_the_instance_not_the_full_dnssd_name() {
        let p = Payload::random().unwrap();
        let full = format!("{}.{}", p.instance(), "_adb-tls-pairing._tcp.local.");
        assert!(p.matches_instance(&full));
        assert!(p.matches_instance(p.instance()));
        assert!(!p.matches_instance("studio-somebodyelse._adb-tls-pairing._tcp.local."));
    }

    #[test]
    fn password_never_reaches_argv() {
        // argv is world-readable through /proc/<pid>/cmdline.
        let p = Payload::random().unwrap();
        let text = p.qr_text();
        let password = text.split(";P:").nth(1).unwrap().trim_end_matches(";;");
        let argv = pair_argv(Path::new("/opt/adb"), "192.168.1.42:37219");
        assert!(!argv.iter().any(|a| a.to_string_lossy().contains(password)));
        assert!(!argv
            .iter()
            .any(|a| a.to_string_lossy().contains(p.instance())));
    }

    #[test]
    fn drops_link_local_v6_and_prefers_v4() {
        let addrs = vec![
            IpAddr::V6("fe80::1".parse::<Ipv6Addr>().unwrap()),
            IpAddr::V6("2001:db8::1".parse::<Ipv6Addr>().unwrap()),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42)),
        ];
        let usable = usable_addresses(&addrs);
        assert_eq!(usable[0], IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42)));
        assert_eq!(
            usable.len(),
            2,
            "link-local is unusable, global v6 is a fallback"
        );
        assert!(!usable.iter().any(|a| a.to_string().starts_with("fe80")));
    }

    #[test]
    fn cli_and_qr_paths_agree_on_when_to_connect() {
        assert!(should_attempt_connect(&PairOutcome::Paired { guid: None }));
        // Unknown wording from a successful run: adb's phrasing varies across 30..36.
        assert!(should_attempt_connect(&PairOutcome::Other {
            msg: "paired ok".into(),
            success: true
        }));
        // Unknown wording from a failed run must not connect.
        assert!(!should_attempt_connect(&PairOutcome::Other {
            msg: "something went wrong".into(),
            success: false
        }));
        assert!(!should_attempt_connect(&PairOutcome::WrongCode));
        assert!(!should_attempt_connect(&PairOutcome::Unreachable));
    }

    #[test]
    fn a_cancelled_connect_does_not_attach_anything() {
        let cancel = AtomicBool::new(true);
        let start = std::time::Instant::now();
        let result = connect_paired_cancellable(
            Path::new("/nonexistent/adb"),
            5037,
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42)),
            Some("adb-XYZ"),
            &cancel,
        );
        assert!(result.is_err(), "a cancelled pairing must not connect");
        // Two 6s browses would otherwise run before it noticed.
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn brackets_ipv6_endpoints() {
        assert_eq!(
            endpoint(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 5555),
            "10.0.0.1:5555"
        );
        assert_eq!(
            endpoint(IpAddr::V6("2001:db8::1".parse::<Ipv6Addr>().unwrap()), 5555),
            "[2001:db8::1]:5555"
        );
    }

    #[test]
    fn reads_a_successful_pair_and_its_guid() {
        let out = "Successfully paired to 192.168.1.42:37219 [guid=adb-39061FDJH00KZR-vWTMTB]\n";
        assert_eq!(
            parse_pair_output(out, "", true),
            PairOutcome::Paired {
                guid: Some("adb-39061FDJH00KZR-vWTMTB".into())
            }
        );

        // Older builds print the guid bare, with no `guid=` key.
        assert_eq!(
            parse_pair_output(
                "Successfully paired to 10.0.0.5:41234 adb-XYZ-ABC\n",
                "",
                true
            ),
            PairOutcome::Paired {
                guid: Some("adb-XYZ-ABC".into())
            }
        );
    }

    #[test]
    fn reads_failure_modes_apart() {
        assert_eq!(
            parse_pair_output("", "adb: failed to authenticate\n", false),
            PairOutcome::WrongCode
        );
        assert_eq!(
            parse_pair_output(
                "",
                "error: protocol fault (couldn't read status message)\n",
                false
            ),
            PairOutcome::Unreachable
        );
        assert_eq!(
            parse_pair_output("", "adb: No pairing code provided\n", false),
            PairOutcome::Unreachable
        );
    }

    #[test]
    fn a_nonzero_exit_is_never_a_successful_pair() {
        // Trusting the output alone would report a run adb itself called a failure.
        assert!(matches!(
            parse_pair_output("Successfully paired to 10.0.0.5:41234\n", "", false),
            PairOutcome::Other { .. }
        ));
    }

    #[test]
    fn connect_failure_is_not_reported_as_connected() {
        assert!(parse_connect_output("connected to 10.0.0.5:41234", true).is_ok());
        assert!(parse_connect_output("already connected to 10.0.0.5:41234", true).is_ok());
        // Some builds report this with a zero exit.
        assert!(parse_connect_output("failed to connect to 10.0.0.5:41234", true).is_err());
        assert!(parse_connect_output("connected to 10.0.0.5:41234", false).is_err());
        assert!(parse_connect_output("", true).is_err());
    }

    #[test]
    fn silence_with_a_zero_exit_is_not_success() {
        assert!(matches!(
            parse_pair_output("", "", true),
            PairOutcome::Other { .. }
        ));
    }

    #[test]
    fn unrecognised_but_successful_output_does_not_hard_fail() {
        // Wording varies across adb 30..36; an unknown line falls through to Other for
        // the caller to fall back on a host-address match, rather than claiming failure.
        // Success is carried through, so the caller can justify a connect attempt.
        assert!(matches!(
            parse_pair_output("paired ok, some new wording\n", "", true),
            PairOutcome::Other { success: true, .. }
        ));
        // The same wording after a failed run must not trigger a connect.
        assert!(matches!(
            parse_pair_output("paired ok, some new wording\n", "", false),
            PairOutcome::Other { success: false, .. }
        ));
    }
}
