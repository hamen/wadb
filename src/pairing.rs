// SPDX-License-Identifier: Apache-2.0

//! Generating a pairing payload, finding the phone that scanned it, and pairing.

use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use zeroize::Zeroize;

/// Android Studio's convention for the instance name it puts in the QR. Kept for
/// compatibility; this is a convention, not an OS requirement.
const PREFIX: &str = "studio-";
const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub const PAIRING_SERVICE: &str = "_adb-tls-pairing._tcp.local.";
pub const CONNECT_SERVICE: &str = "_adb-tls-connect._tcp.local.";

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
        Ok(Self {
            instance: format!("{PREFIX}{}", random_token(10)?),
            password: random_token(10)?,
        })
    }

    pub fn instance(&self) -> &str {
        &self.instance
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
        instance == self.instance
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
    Other(String),
}

/// Read `adb pair`'s result. A zero exit alone is not proof — some builds exit zero on a
/// refused code — and some failures exit non-zero with terse output, so both are read.
pub fn parse_pair_output(stdout: &str, stderr: &str, success_exit: bool) -> PairOutcome {
    let text = format!("{stdout}\n{stderr}");
    let lower = text.to_lowercase();
    if lower.contains("successfully paired") {
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
        return PairOutcome::Other("adb pair said nothing".into());
    }
    PairOutcome::Other(text.trim().to_string())
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

/// Run a child with a deadline, killing and reaping it on expiry so a hung adb cannot
/// block the pairing worker.
fn run_bounded(
    mut cmd: Command,
    stdin_data: Option<&str>,
    timeout: Duration,
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
        child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("no stdin pipe"))?
            .write_all(data.as_bytes())?;
    }
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            break;
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
    let mut cmd = adb_command(adb, port);
    cmd.args(["pair", endpoint]);
    let mut line = format!("{code}\n");
    let result = run_bounded(cmd, Some(&line), Duration::from_secs(20));
    line.zeroize();
    let (stdout, stderr, ok) = result?;
    Ok(parse_pair_output(&stdout, &stderr, ok))
}

pub fn connect(adb: &Path, port: u16, endpoint: &str) -> Result<String> {
    let mut cmd = adb_command(adb, port);
    cmd.args(["connect", endpoint]);
    // A bad address blocks the worker just as effectively as a bad code.
    let (stdout, stderr, _) = run_bounded(cmd, None, Duration::from_secs(10))?;
    Ok(format!("{}{}", stdout.trim(), stderr.trim()))
}

/// The argv `adb pair` is invoked with, exposed so tests can prove the password is not in it.
pub fn pair_argv(adb: &Path, endpoint: &str) -> Vec<PathBuf> {
    vec![adb.to_path_buf(), "pair".into(), endpoint.into()]
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
    fn silence_with_a_zero_exit_is_not_success() {
        assert!(matches!(
            parse_pair_output("", "", true),
            PairOutcome::Other(_)
        ));
    }

    #[test]
    fn unrecognised_but_successful_output_does_not_hard_fail() {
        // Wording varies across adb 30..36; an unknown line falls through to Other for
        // the caller to fall back on a host-address match, rather than claiming failure.
        assert!(matches!(
            parse_pair_output("paired ok, some new wording\n", "", true),
            PairOutcome::Other(_)
        ));
    }
}
