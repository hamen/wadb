# wadb

Keeps the ADB server running on Linux so paired Android phones stay available for wireless
debugging, without keeping Android Studio open. Pair once by scanning a QR code in your
terminal; after that wadb brings the phone back whenever the connection drops.

Two `systemd --user` units do the work: one supervises the standard server on `127.0.0.1:5037` and
restarts it if anything stops it, the other reconnects wireless devices when they drop. Android Studio, the `adb` command line and every other adb client see
the same devices.

```
┌ wadb   ● supervised, pid 4242 ───────────────────────────────────────────┐
├──────────────────────────────────────────────┬───────────────────────────┤
│ wireless devices                             │ pair a device             │
│   model      serial              state   how │   █▀▀▀▀▀█ ▄▀█ █▀▀▀▀▀█     │
│ ● Pixel_9    192.168.1.42:37219  device  tcp │   █ ███ █ ▀▄▄ █ ███ █     │
│ ○ Tab_S9     adb-39061FDJ-vWTMTB offline mdns│   █ ▀▀▀ █ █▄▀ █ ▀▀▀ █     │
│                                              │ ⠙ waiting for a scan  98s │
└──────────────────────────────────────────────┴───────────────────────────┘
 p pair    r refresh    q quit
```

## Requirements

- Linux with a `systemd --user` session.
- **Android SDK Platform-Tools.** Not any `adb`: it must be a build with an mDNS backend
  (see below). `wadb install` checks this and refuses otherwise.
- Your computer and phone on the same Wi-Fi network.
- **Wireless debugging** enabled in the phone's Developer options.

## Why wadb reconnects devices itself

adb is supposed to do this for you: with an mDNS backend it browses `_adb-tls-connect._tcp` and
auto-connects the services named in `$ADB_MDNS_AUTO_CONNECT`. On a machine running `avahi-daemon`,
which owns port 5353, that discovery frequently returns nothing — while `adb mdns check` still
reports a working daemon version. Measured on a real Pixel 8a: after `adb kill-server` the wireless
device never came back, and `adb mdns services` stayed empty on a server that had been up for
minutes, at an instant when both `avahi-browse` and wadb's own browser could see the phone.

A compiled-in backend is not evidence that discovery works. So wadb runs a small watcher
(`wadb-connect.service`) that browses for advertised devices itself and issues `adb connect` for
anything missing. That is adb's own documented behaviour, performed by the only mDNS implementation
on the host that works. `adb connect` succeeds only for a device that already trusts this host's
key, so nobody else's phone can be attached this way.

## Why the adb build matters

adb is *supposed* to reconnect trusted wireless devices itself, and it needs an mDNS backend to do
it. Distro builds frequently have none at all:

| adb | `mdns check` |
|---|---|
| Android SDK Platform-Tools 36.0.0 | `mdns daemon version [Openscreen discovery 0.0.0]` |
| Debian/Ubuntu `adb` 34.0.5 | *(nothing)* |

wadb's watcher does its own discovery, so it could in principle reconnect devices even behind an
adb with no backend at all. `wadb install` still refuses one, for a narrower reason: such a build is
a distro package that will also be first on your `PATH`, and the moment any tool runs it while the
supervised server is down it forks a replacement server that has no mDNS at all and takes the port.
Refusing it keeps the server you are supervising and the `adb` your shell finds from being two
different programs. The section above is why having the backend is not sufficient either.

Checking this correctly is subtle: `adb mdns check` reports the state of whichever **server**
answers, not of the binary you invoked. Point the Debian binary at an SDK server and it will
happily claim an openscreen daemon it does not have. `wadb` therefore starts the candidate
binary as its own server on a scratch port and questions *that* server, then tears it down.

## Getting started

```sh
cargo install --path .     # or: cargo build --release
wadb install               # probes adb, writes and starts the unit
wadb                       # the TUI
```

Press `p`, then on the phone open **Settings → Developer options → Wireless debugging →
Pair device with QR code** and scan the code in your terminal.

If your phone cannot scan a screen, use the six-digit code instead:

```sh
wadb pair 192.168.1.42:37219    # ip:port from the phone's Wireless debugging screen
```

## Commands

| | |
|---|---|
| `wadb` | the TUI (prints `status` instead when stdout is not a terminal) |
| `wadb install` | probe adb, write and start the unit |
| `wadb status` | unit, server, adb binary, mDNS discovery, devices |
| `wadb pair <ip:port>` | pair with a typed six-digit code |
| `wadb connect` | reconnect every advertised wireless device once, by hand |
| `wadb daemon` | the reconnect watcher; this is what `wadb-connect.service` runs |
| `wadb takeover` | ask a foreign adb server to stop so the unit can take the port |
| `wadb uninstall` | stop and remove both units |

## Notes and limitations

- **Only wireless devices are listed.** USB devices and emulators stay available to every
  adb client, they are just not what this tool is about.
- **wadb never kills an adb server it does not own.** If another server already holds the
  port, it says so and offers `wadb takeover`, which sends adb's own cooperative
  `kill-server` request. Nothing is ever signalled directly.
- `wadb uninstall` stops the server the unit owned, which drops the USB and emulator
  sessions attached to it. The next adb command from any tool starts a fresh server —
  possibly a build with no mDNS backend.
- Without `loginctl enable-linger`, a `systemd --user` unit stops at your last logout.
  `wadb install` tells you if lingering is off.
- Local Wi-Fi only. There is no remote relay.
- wadb never installs or replaces platform-tools.

## Security

The pairing password is generated from the OS random source, written to `adb pair` on
**stdin only** — never argv, which is world-readable through `/proc/<pid>/cmdline` — and
zeroized after use.

Be aware this is narrower than "the password is safe": for the seconds it is live it also
exists in the QR matrix, in the rendered terminal buffer, and in your terminal's
scrollback. It is a single-use credential with a short life, and wadb does not persist it,
but it is not a secret held only in locked memory.

wadb stores nothing: no device list, no keys, no pairing state. Device state is always read
live from the adb server. It uses your existing adb server, keys and pairings, with no
helper daemon and no third-party relay.

## Credits

An independent Rust rewrite for Linux, inspired by [wADB](https://github.com/c5inco/wADB)
by Chris Sinco, a macOS menu-bar app doing the same job. No code is shared between them.

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
