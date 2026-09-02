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
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::adb::{Device, SmartSocket, Transport};
use crate::discovery::Found;
use crate::pairing::{endpoint, parse_connect_output, usable_addresses, CONNECT_SERVICE};

/// Pause between passes. A full cycle is this plus `BROWSE`.
pub const POLL: Duration = Duration::from_secs(5);
/// How long each browse listens before acting on what it heard.
pub const BROWSE: Duration = Duration::from_secs(3);
/// After this many consecutive failures an endpoint is left alone for a while, so an unreachable
/// phone does not produce one `adb connect` every poll forever.
pub const FAILURES_BEFORE_BACKOFF: u32 = 3;
pub const BACKOFF: Duration = Duration::from_secs(60);

/// Is this discovered service present AND usable?
///
/// Presence is not enough. adb keeps an `offline` entry for a device whose transport died, which is
/// exactly what a suspend/resume or a dropped link leaves behind — so treating any entry as
/// "attached" would make the watcher skip the very case it exists for. Only `device` counts.
/// `unauthorized` also counts, because reconnecting cannot fix it: the user must accept the prompt.
///
/// Every advertised address is checked, not just the one we would dial. A phone attached over its
/// IPv6 address is attached, and dialing its IPv4 address would give the same handset a second row.
pub fn already_attached(found: &Found, devices: &[Device]) -> bool {
    let endpoints: Vec<String> = usable_addresses(&found.addresses)
        .into_iter()
        .map(|a| endpoint(a, found.port))
        .collect();
    devices
        .iter()
        .filter(|d| d.state == "device" || d.state == "unauthorized")
        .any(|d| match d.transport {
            Transport::Tcp => endpoints.contains(&d.serial),
            // `adb-<serial>-<suffix>._adb-tls-connect._tcp`. Compare on a label boundary so a short
            // instance cannot match a longer one.
            Transport::Mdns => d
                .serial
                .split('.')
                .next()
                .is_some_and(|i| i == found.instance()),
            _ => false,
        })
}

/// What the watcher should do about one discovered service.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Addresses to try, in order. More than one, so a dead IPv4 does not hide a working IPv6.
    Connect(Vec<String>),
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
    let addresses = usable_addresses(&found.addresses);
    if addresses.is_empty() {
        return Action::NoUsableAddress;
    }
    if already_attached(found, devices) {
        return Action::AlreadyAttached;
    }
    let backed_off = |ep: &String| {
        failures.get(ep).is_some_and(|(count, last)| {
            *count >= FAILURES_BEFORE_BACKOFF && now.duration_since(*last) < BACKOFF
        })
    };
    let candidates: Vec<String> = addresses
        .into_iter()
        .map(|a| endpoint(a, found.port))
        .filter(|ep| !backed_off(ep))
        .collect();
    if candidates.is_empty() {
        return Action::BackingOff;
    }
    Action::Connect(candidates)
}

/// What one pass did. Failures are reported, not swallowed: a caller that printed
/// "nothing to reconnect" after every attempt failed would be lying.
#[derive(Debug, Default)]
pub struct Outcome {
    pub connected: Vec<String>,
    pub failed: Vec<String>,
}

/// One pass: browse, then connect whatever is advertised and missing.
pub fn tick(port: u16, failures: &mut HashMap<String, (u32, Instant)>) -> Result<Outcome> {
    let sock = SmartSocket::new(port);
    // No server, nothing to do — and asking over the socket cannot start one.
    if !sock.is_up() {
        return Ok(Outcome::default());
    }
    // A device list we could not read is not an empty device list. Treating a failed query as
    // "nothing is attached" would make every advertised phone look missing and earn a connect.
    let devices = sock
        .devices()
        .context("could not read the device list; skipping this pass")?;
    let found = crate::discovery::browse_all(CONNECT_SERVICE, BROWSE)?;
    // The browse takes seconds. Re-read the device list afterwards, or a phone that attached
    // during the browse earns a redundant connect.
    let devices = sock.devices().unwrap_or(devices);

    let mut outcome = Outcome::default();
    for service in &found {
        match decide(service, &devices, failures, Instant::now()) {
            Action::Connect(candidates) => {
                let mut errors = Vec::new();
                let mut attached = None;
                for ep in &candidates {
                    // Over the smart socket: `adb connect` would fork a server if this one has
                    // just died, which is the very failure the watcher exists to prevent.
                    let result = sock
                        .connect_device(ep)
                        .and_then(|text| parse_connect_output(&text, true));
                    match result {
                        Ok(line) => {
                            attached = Some(line);
                            break;
                        }
                        Err(e) => {
                            let entry = failures.entry(ep.clone()).or_insert((0, Instant::now()));
                            entry.0 += 1;
                            entry.1 = Instant::now();
                            errors.push(format!("{ep}: {e}"));
                        }
                    }
                }
                match attached {
                    // One address working means the phone is up, so nothing about it is failing:
                    // clear every sibling endpoint, not just the one that answered, or a dead
                    // IPv4 stays backed off while the device is healthy.
                    Some(line) => {
                        for a in usable_addresses(&service.addresses) {
                            failures.remove(&endpoint(a, service.port));
                        }
                        outcome.connected.push(line);
                    }
                    // Only a service that failed on *every* address counts as a failed device.
                    None => outcome.failed.push(errors.join("; ")),
                }
            }
            // A device that came back by any means clears its history, or a stale streak would
            // keep it backed off after it is healthy again.
            Action::AlreadyAttached => {
                for a in usable_addresses(&service.addresses) {
                    failures.remove(&endpoint(a, service.port));
                }
            }
            Action::NoUsableAddress | Action::BackingOff => {}
        }
    }
    // Wireless debugging rotates the port, so a failed endpoint can never be advertised again.
    // Without this the map grows for the life of the daemon.
    let live: std::collections::HashSet<String> = found
        .iter()
        .flat_map(|f| {
            usable_addresses(&f.addresses)
                .into_iter()
                .map(move |a| endpoint(a, f.port))
        })
        .collect();
    failures.retain(|ep, _| live.contains(ep));

    Ok(outcome)
}

/// The watcher loop, as run by `wadb-connect.service`.
pub fn run(port: u16) -> Result<()> {
    // No adb binary is named here on purpose: connects go over the server's smart socket, so the
    // watcher never runs adb and cannot fork one.
    eprintln!("wadb: watching {CONNECT_SERVICE} for devices to reconnect on port {port}");
    let mut failures: HashMap<String, (u32, Instant)> = HashMap::new();
    loop {
        match tick(port, &mut failures) {
            Ok(outcome) => {
                for line in outcome.connected {
                    eprintln!("wadb: {line}");
                }
                for line in outcome.failed {
                    eprintln!("wadb: connect failed: {line}");
                }
            }
            Err(e) => eprintln!("wadb: pass failed: {e}"),
        }
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adb::parse_devices;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    fn service(addrs: Vec<IpAddr>) -> Found {
        Found {
            fullname: "adb-3C231JEKB44234-9igruZ._adb-tls-connect._tcp.local.".into(),
            addresses: addrs,
            port: 42595,
        }
    }

    fn v4() -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(192, 168, 86, 45))
    }

    fn connect_targets(action: Action) -> Vec<String> {
        match action {
            Action::Connect(eps) => eps,
            other => panic!("expected a connect, got {other:?}"),
        }
    }

    #[test]
    fn connects_a_device_that_is_advertised_but_missing() {
        let eps = connect_targets(decide(
            &service(vec![v4()]),
            &[],
            &HashMap::new(),
            Instant::now(),
        ));
        assert_eq!(eps, ["192.168.86.45:42595"]);
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
        let devices = parse_devices(
            "adb-3C231JEKB44234-9igruZ._adb-tls-connect._tcp device model:Pixel_8a\n",
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
    fn an_offline_device_is_reconnected_not_treated_as_present() {
        // The case the watcher exists for. adb keeps an `offline` entry after a transport dies -
        // a suspend/resume, a dropped link - and counting that as attached would skip it forever.
        for line in [
            "192.168.86.45:42595 offline model:Pixel_8a\n",
            "adb-3C231JEKB44234-9igruZ._adb-tls-connect._tcp offline model:Pixel_8a\n",
        ] {
            let devices = parse_devices(line);
            assert!(
                matches!(
                    decide(
                        &service(vec![v4()]),
                        &devices,
                        &HashMap::new(),
                        Instant::now()
                    ),
                    Action::Connect(_)
                ),
                "offline device must be reconnected: {line}"
            );
        }
    }

    #[test]
    fn an_unauthorized_device_is_left_alone_under_either_serial() {
        for line in [
            "192.168.86.45:42595 unauthorized model:Pixel_8a\n",
            "adb-3C231JEKB44234-9igruZ._adb-tls-connect._tcp unauthorized model:Pixel_8a\n",
        ] {
            let devices = parse_devices(line);
            assert_eq!(
                decide(
                    &service(vec![v4()]),
                    &devices,
                    &HashMap::new(),
                    Instant::now()
                ),
                Action::AlreadyAttached,
                "reconnect cannot clear an authorisation prompt: {line}"
            );
        }
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
    fn a_shorter_instance_does_not_match_a_longer_serial() {
        let mut svc = service(vec![v4()]);
        svc.fullname = "adb-3C231._adb-tls-connect._tcp.local.".into();
        let devices = parse_devices(
            "adb-3C231JEKB44234-9igruZ._adb-tls-connect._tcp device model:Pixel_8a\n",
        );
        assert!(matches!(
            decide(&svc, &devices, &HashMap::new(), Instant::now()),
            Action::Connect(_)
        ));
    }

    #[test]
    fn attachment_is_checked_against_every_advertised_address() {
        // Attached over IPv6; dialing the IPv4 address would give the same handset a second row.
        let devices = parse_devices("[fde7:3f3c:51e7:f10a::1]:42595 device model:Pixel_8a\n");
        let addrs = vec![
            v4(),
            IpAddr::V6("fde7:3f3c:51e7:f10a::1".parse::<Ipv6Addr>().unwrap()),
        ];
        assert_eq!(
            decide(&service(addrs), &devices, &HashMap::new(), Instant::now()),
            Action::AlreadyAttached
        );
    }

    #[test]
    fn offers_every_usable_address_in_order_ipv4_first() {
        // The real advert from the Pixel 8a carried nine addresses, mostly IPv6 including a
        // link-local one, which adb cannot dial without a scope id.
        let addrs = vec![
            IpAddr::V6("fe80::98db:69ff:fe7b:87f4".parse::<Ipv6Addr>().unwrap()),
            IpAddr::V6("fde7:3f3c:51e7:f10a::1".parse::<Ipv6Addr>().unwrap()),
            v4(),
        ];
        let eps = connect_targets(decide(
            &service(addrs),
            &[],
            &HashMap::new(),
            Instant::now(),
        ));
        assert_eq!(eps[0], "192.168.86.45:42595", "IPv4 is tried first");
        assert_eq!(
            eps.len(),
            2,
            "a dead IPv4 must not hide a working global IPv6"
        );
        assert!(eps[1].starts_with('['), "IPv6 endpoints are bracketed");
        assert!(
            !eps.iter().any(|e| e.contains("fe80")),
            "link-local is undialable"
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
    fn backoff_on_one_address_still_leaves_the_others() {
        let now = Instant::now();
        let mut failures = HashMap::new();
        failures.insert(
            "192.168.86.45:42595".to_string(),
            (FAILURES_BEFORE_BACKOFF, now),
        );
        let addrs = vec![
            v4(),
            IpAddr::V6("fde7:3f3c:51e7:f10a::1".parse::<Ipv6Addr>().unwrap()),
        ];
        let eps = connect_targets(decide(&service(addrs), &[], &failures, now));
        assert_eq!(
            eps.len(),
            1,
            "the backed-off IPv4 is dropped, the IPv6 remains"
        );
        assert!(eps[0].starts_with('['));
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
