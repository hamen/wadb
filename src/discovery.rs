// SPDX-License-Identifier: Apache-2.0

//! Browsing mDNS for the phone that scanned our QR.

use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// Full DNS-SD name, e.g. `studio-abc._adb-tls-pairing._tcp.local.`
    pub fullname: String,
    pub addresses: Vec<IpAddr>,
    pub port: u16,
}

impl Found {
    /// The instance component, which is what a QR payload is matched against.
    pub fn instance(&self) -> &str {
        self.fullname.split('.').next().unwrap_or(&self.fullname)
    }
}

/// Browse a service type until `accept` likes something, or the deadline passes.
///
/// The daemon is shut down on every exit path: leaving it running would hold the
/// multicast socket and make a second pairing attempt fail.
pub fn browse<F>(service: &str, timeout: Duration, accept: F) -> Result<Option<Found>>
where
    F: FnMut(&Found) -> bool,
{
    browse_cancellable(service, timeout, &AtomicBool::new(false), accept)
}

/// Browse until found, cancelled, or the deadline passes.
///
/// Cancellation has to reach the browser itself, not just the channel the UI reads: a
/// worker left running holds the multicast socket and can still pair a phone after the
/// user pressed Esc.
pub fn browse_cancellable<F>(
    service: &str,
    timeout: Duration,
    cancel: &AtomicBool,
    mut accept: F,
) -> Result<Option<Found>>
where
    F: FnMut(&Found) -> bool,
{
    let daemon = ServiceDaemon::new().context("could not start the mDNS browser")?;
    let receiver = daemon.browse(service).context("could not browse mDNS")?;
    let deadline = Instant::now() + timeout;
    let mut hit = None;

    while Instant::now() < deadline && !cancel.load(Ordering::Relaxed) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining.min(Duration::from_millis(500))) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                let found = Found {
                    fullname: info.get_fullname().to_string(),
                    addresses: info.get_addresses().iter().copied().collect(),
                    port: info.get_port(),
                };
                if accept(&found) {
                    hit = Some(found);
                    break;
                }
            }
            Ok(_) => continue,
            Err(_) => continue,
        }
    }

    // shutdown() returns a receiver that confirms the daemon has released its socket.
    // Dropping it unread lets the next browse start before port 5353 is free.
    if let Ok(rx) = daemon.shutdown() {
        let _ = rx.recv_timeout(Duration::from_secs(2));
    }
    Ok(hit)
}

/// Everything of a given type seen within the window. Used by `status` to show whether
/// discovery works at all on this network.
pub fn browse_all(service: &str, timeout: Duration) -> Result<Vec<Found>> {
    let mut seen: Vec<Found> = Vec::new();
    browse(service, timeout, |f| {
        if !seen.iter().any(|s| s.fullname == f.fullname) {
            seen.push(f.clone());
        }
        false
    })?;
    Ok(seen)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pairing::{CONNECT_SERVICE, PAIRING_SERVICE};

    #[test]
    fn instance_is_the_first_label() {
        let f = Found {
            fullname: "studio-Ab3xK9pQr2._adb-tls-pairing._tcp.local.".into(),
            addresses: vec![],
            port: 37219,
        };
        assert_eq!(f.instance(), "studio-Ab3xK9pQr2");
    }

    #[test]
    fn a_cancelled_browse_returns_immediately() {
        let cancel = AtomicBool::new(true);
        let start = Instant::now();
        let found = browse_cancellable(PAIRING_SERVICE, Duration::from_secs(30), &cancel, |_| true)
            .unwrap();
        assert!(found.is_none());
        // Without the flag reaching the loop this would block for 30 seconds.
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    /// Spike B: the browser must coexist with a running avahi-daemon, which already owns
    /// port 5353. If this cannot start, pairing has no discovery at all.
    #[test]
    #[ignore = "touches the network; run with --ignored"]
    fn browser_starts_alongside_avahi() {
        let started = std::time::Instant::now();
        let found = browse_all(PAIRING_SERVICE, Duration::from_secs(2))
            .expect("mdns-sd must start while avahi-daemon holds 5353");
        eprintln!("pairing services seen in 2s: {}", found.len());

        let connect = browse_all(CONNECT_SERVICE, Duration::from_secs(2))
            .expect("second browse must work after the first shut down");
        eprintln!("connect services seen in 2s: {}", connect.len());
        for f in &connect {
            eprintln!("  {} -> {:?}:{}", f.instance(), f.addresses, f.port);
        }
        // Finding nothing is fine (no phone is pairing right now); failing to browse is not.
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
