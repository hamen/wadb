// SPDX-License-Identifier: Apache-2.0

//! Reconnecting wireless devices, because adb's own mDNS cannot be relied on.
//!
//! adb auto-connects services named in `$ADB_MDNS_AUTO_CONNECT` (default `adb-tls-connect`) — but
//! only when its openscreen backend can actually discover them. On a machine running
//! `avahi-daemon`, which owns port 5353, that discovery returns nothing while `adb mdns check`
//! still cheerfully reports a daemon version. Measured on a real Pixel 8a: after `adb kill-server`
//! the wireless device never came back, while `mdns-sd` in this same process saw it immediately.
//!
//! So this watcher performs the connect adb cannot. It is not a new policy — it is adb's own
//! documented behaviour, carried out by the only mDNS implementation on this host that works.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::adb::{Device, SmartSocket, Transport};
use crate::discovery::Found;
use crate::pairing::{connect_cancellable, endpoint, usable_addresses, CONNECT_SERVICE};

/// How often the watcher looks.
pub const POLL: Duration = Duration::from_secs(5);
/// How long each browse listens before acting on what it heard.
pub const BROWSE: Duration = Duration::from_secs(3);
/// After this many consecutive failures an endpoint is left alone for a while, so an unreachable
/// phone does not produce one `adb connect` every poll forever.
pub const FAILURES_BEFORE_BACKOFF: u32 = 3;
pub const BACKOFF: Duration = Duration::from_secs(60);

/// Is this discovered service already attached?
///
/// A device can be present under either serial shape, and after a manual connect it is present
/// under both. Either counts as attached: reconnecting an attached device is at best noise.
pub fn already_attached(found: &Found, addr: IpAddr, devices: &[Device]) -> bool {
    let ep = endpoint(addr, found.port);
    devices.iter().any(|d| match d.transport {
        Transport::Tcp => d.serial == ep,
        // `adb-<serial>-<suffix>._adb-tls-connect._tcp`
        Transport::Mdns => d.serial.starts_with(found.instance()),
        _ => false,
    })
}

/// What the watcher should do about one discovered service.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Connect(String),
    AlreadyAttached,
    /// Nothing adb can dial: every address was loopback or link-local.
    NoUsableAddress,
    /// Failing repeatedly; left alone until the backoff expires.
    BackingOff,
}

/// The whole decision, kept pure so it can be tested without a network or an adb.
pub fn decide(
    found: &Found,
    devices: &[Device],
    failures: &HashMap<String, (u32, Instant)>,
    now: Instant,
) -> Action {
    let Some(addr) = usable_addresses(&found.addresses).into_iter().next() else {
        return Action::NoUsableAddress;
    };
    if already_attached(found, addr, devices) {
        return Action::AlreadyAttached;
    }
    let ep = endpoint(addr, found.port);
    if let Some((count, last)) = failures.get(&ep) {
        if *count >= FAILURES_BEFORE_BACKOFF && now.duration_since(*last) < BACKOFF {
            return Action::BackingOff;
        }
    }
    Action::Connect(ep)
}

/// One pass: browse, then connect whatever is advertised and missing.
pub fn tick(
    adb: &Path,
    port: u16,
    failures: &mut HashMap<String, (u32, Instant)>,
) -> Result<Vec<String>> {
    let sock = SmartSocket::new(port);
    // No server, nothing to do — and asking over the socket cannot start one.
    if !sock.is_up() {
        return Ok(Vec::new());
    }
    let devices = sock.devices().unwrap_or_default();
    let found = crate::discovery::browse_all(CONNECT_SERVICE, BROWSE)?;

    let mut connected = Vec::new();
    for service in &found {
        match decide(service, &devices, failures, Instant::now()) {
            Action::Connect(ep) => {
                let cancel = std::sync::atomic::AtomicBool::new(false);
                match connect_cancellable(adb, port, &ep, &cancel) {
                    Ok(line) => {
                        failures.remove(&ep);
                        connected.push(line);
                    }
                    Err(e) => {
                        let entry = failures.entry(ep).or_insert((0, Instant::now()));
                        entry.0 += 1;
                        entry.1 = Instant::now();
                        eprintln!("wadb: connect failed: {e}");
                    }
                }
            }
            Action::AlreadyAttached | Action::NoUsableAddress | Action::BackingOff => {}
        }
    }
    Ok(connected)
}

/// The watcher loop, as run by `wadb-connect.service`.
pub fn run(adb: PathBuf, port: u16) -> Result<()> {
    eprintln!(
        "wadb: watching {CONNECT_SERVICE} for devices to reconnect on port {port} using {}",
        adb.display()
    );
    let mut failures: HashMap<String, (u32, Instant)> = HashMap::new();
    loop {
        match tick(&adb, port, &mut failures) {
            Ok(connected) => {
                for line in connected {
                    eprintln!("wadb: {line}");
                }
            }
            Err(e) => eprintln!("wadb: browse failed: {e}"),
        }
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adb::parse_devices;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn service(addrs: Vec<IpAddr>) -> Found {
        Found {
            fullname: "adb-EXAMPLEDEVICE-a1b2c3._adb-tls-connect._tcp.local.".into(),
            addresses: addrs,
            port: 42595,
        }
    }

    fn v4() -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(192, 168, 86, 45))
    }

    #[test]
    fn connects_a_device_that_is_advertised_but_missing() {
        let action = decide(&service(vec![v4()]), &[], &HashMap::new(), Instant::now());
        assert_eq!(action, Action::Connect("192.168.86.45:42595".into()));
    }

    #[test]
    fn skips_a_device_already_attached_by_endpoint() {
        let devices = parse_devices("192.168.86.45:42595 device model:Pixel_8a\n");
        assert_eq!(
            decide(
                &service(vec![v4()]),
                &devices,
                &HashMap::new(),
                Instant::now()
            ),
            Action::AlreadyAttached
        );
    }

    #[test]
    fn skips_a_device_already_attached_under_its_mdns_serial() {
        // This is the shape adb's own auto-connect produces, and the one that appears after a
        // restart on a machine where adb's discovery does work.
        let devices = parse_devices(
            "adb-EXAMPLEDEVICE-a1b2c3._adb-tls-connect._tcp device model:Pixel_8a\n",
        );
        assert_eq!(
            decide(
                &service(vec![v4()]),
                &devices,
                &HashMap::new(),
                Instant::now()
            ),
            Action::AlreadyAttached
        );
    }

    #[test]
    fn a_different_phone_is_not_mistaken_for_the_attached_one() {
        let devices = parse_devices("192.168.86.99:41000 device model:Pixel_Fold\n");
        assert!(matches!(
            decide(
                &service(vec![v4()]),
                &devices,
                &HashMap::new(),
                Instant::now()
            ),
            Action::Connect(_)
        ));
    }

    #[test]
    fn prefers_ipv4_and_ignores_link_local_ipv6() {
        // The real advert from the Pixel 8a carried nine addresses, mostly IPv6 including a
        // link-local one, which adb cannot dial without a scope id.
        let addrs = vec![
            IpAddr::V6("fe80::98db:69ff:fe7b:87f4".parse::<Ipv6Addr>().unwrap()),
            IpAddr::V6("fde7:3f3c:51e7:f10a::1".parse::<Ipv6Addr>().unwrap()),
            v4(),
        ];
        assert_eq!(
            decide(&service(addrs), &[], &HashMap::new(), Instant::now()),
            Action::Connect("192.168.86.45:42595".into())
        );
    }

    #[test]
    fn a_service_with_no_dialable_address_is_skipped_not_retried() {
        let addrs = vec![IpAddr::V6("fe80::1".parse::<Ipv6Addr>().unwrap())];
        assert_eq!(
            decide(&service(addrs), &[], &HashMap::new(), Instant::now()),
            Action::NoUsableAddress
        );
    }

    #[test]
    fn backs_off_after_repeated_failures_then_tries_again() {
        let now = Instant::now();
        let mut failures = HashMap::new();
        failures.insert(
            "192.168.86.45:42595".to_string(),
            (FAILURES_BEFORE_BACKOFF, now),
        );
        assert_eq!(
            decide(&service(vec![v4()]), &[], &failures, now),
            Action::BackingOff
        );
        // Once the window passes it must try again, or an unreachable phone would be abandoned
        // for the life of the daemon.
        let later = now + BACKOFF + Duration::from_secs(1);
        assert!(matches!(
            decide(&service(vec![v4()]), &[], &failures, later),
            Action::Connect(_)
        ));
    }

    #[test]
    fn a_couple_of_failures_do_not_trigger_backoff_yet() {
        let now = Instant::now();
        let mut failures = HashMap::new();
        failures.insert("192.168.86.45:42595".to_string(), (1, now));
        assert!(matches!(
            decide(&service(vec![v4()]), &[], &failures, now),
            Action::Connect(_)
        ));
    }
}
