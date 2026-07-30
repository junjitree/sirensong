# SirenSong

SirenSong answers the Starbucks siren's call for you — it automates login to
Starbucks Wi-Fi captive portals. It authenticates over plain HTTP and rolls a
fresh MAC address on each reconnect, so the portal's per-device time limit never
runs out.

## Prerequisites

- **Linux** — see [Platform support](#platform-support)
- `nmcli` (NetworkManager command-line tool)
- Rust and Cargo (only to build from source)

MAC randomization is configured automatically — see
[How re-auth works](#how-re-auth-works-mac-rotation) if it can't be applied.

## Platform support

SirenSong is **Linux-only**, and not merely for want of a second backend.
Building on any other OS fails with a message saying so, rather than installing
a binary that runs and silently does nothing.

The portal login itself is plain HTTP and would port anywhere. What does not
port is the part that makes re-auth work at all: rolling a fresh MAC on each
reconnect. On Linux that is declarative — NetworkManager's
`wifi.cloned-mac-address=random` assigns a new address on every activation.

**The macOS Wi-Fi driver permits exactly one MAC address: its own.** Verified on
an Apple M3 Pro running macOS 26.5.2, driving the `SIOCSIFLLADDR` ioctl
directly. Setting any other address on `en0` fails with `EADDRNOTAVAIL` whether
the interface is associated or the radio is powered off, for both a
locally-administered address and one reusing the adapter's own OUI — while
writing `en0`'s *current* address back to it succeeds. So the driver reaches the
point of validating the address and refuses every value but one.

This is not a system-wide prohibition. A non-Wi-Fi interface on the same machine
changes its MAC through the same ioctl without complaint, with SIP enabled, so
neither SIP nor privilege is what stands in the way. Nor does disassociating
help: a down interface rejects the write with `ENETDOWN` on *any* hardware,
which is why the old `airport -z` recipe cannot be revived.

Apple's own Private Wi-Fi Address is no substitute either: "Rotating" changes
about every two weeks, and the address is cached for 24 hours after forgetting a
network, so forget-and-rejoin reuses it. Purging that cache means writing to a
TCC-protected system plist, which needs Full Disk Access granted to the calling
application — not something a `cargo install`ed binary has, and not something
worth asking for. SirenSong needs rotation on the order of an hour.

Without rotation a macOS port would reduce to "logs you in once", which the
system captive-portal sheet already does.

## Installation

From [crates.io](https://crates.io/crates/sirensong):

```bash
cargo install sirensong
```

### From source

1. Clone this repository:

   ```bash
   git clone https://github.com/junjitree/sirensong.git
   cd sirensong
   ```

2. Build with Cargo:

   ```bash
   cargo build --release
   ```

Or install straight into your Cargo bin directory:

```bash
cargo install --path .
```

## Usage

If you installed the binary using `cargo install`, you can run it directly:

```bash
sirensong [SSID]
```

Otherwise, run the application using Cargo:

```bash
cargo run -- [SSID]
```

Or run the compiled binary:

```bash
./target/release/sirensong [SSID]
```

It defaults to "Starbucks Customer" if no SSID is provided.

By default sirensong runs in **watch mode** — it stays up and re-authenticates
whenever the portal drops. Pass `--once` to authenticate a single time and exit.

### Options

```
sirensong [OPTIONS] [SSID]

  -o, --once             Authenticate once and exit (default: watch and re-auth on drop)
  -i, --interval <SECS>  Watch poll interval in seconds (default: 60)
  -q, --quiet            Only log errors (overrides RUST_LOG)
  -h, --help             Print help
  -V, --version          Print version
```

By default it logs plain status lines — when you are watching, when you are good
to browse, and when the connection drops. For the underlying mechanics (MAC
cycling, association attempts, portal form submission), set `RUST_LOG=debug`.

Before doing anything, sirensong runs a lightweight `generate_204` connectivity
check and exits early if you are already online, so repeated runs are cheap.

### Watch mode (default)

Captive-portal sessions expire on the venue's clock. Watch mode keeps you
authenticated by polling connectivity and re-running the portal login only when
it drops. Just run it in a terminal while you are at the café and stop it
(Ctrl-C) when you leave:

```bash
sirensong                       # watch "Starbucks Customer"
sirensong "Some Other Cafe"     # watch a different SSID (see below)
sirensong -i 30                 # poll every 30s
```

**On other SSIDs:** the SSID argument only changes which network it joins — the
portal login itself is hardcoded to the Cisco Meraki **free-plan** splash. So a
different SSID works if the network is open with no portal at all (it
associates, sees connectivity, and does nothing further), or if it runs that
same Meraki free-plan splash. Any other portal — a different vendor, or Meraki
configured for vouchers, click-through terms, or SMS — is not supported;
sirensong logs that it found no free-plan form and keeps retrying without
adapting.

It only reconnects when it confirms it is offline (several failed probes, so a
momentary blip does not trigger a needless reconnect) **and** it can see that
the network it is attached to is a captive portal it knows how to log into.

That second check is what makes it safe to forget about. Rather than asking
"which network am I on", sirensong asks **who answered**: a captive portal
replies to its connectivity probe (with a splash page or a redirect to one),
while a network whose uplink is simply down replies with nothing at all. So if
you walk away and forget to stop it, and your home internet later drops, it sees
silence rather than a portal and leaves your connection alone. An unfamiliar
portal — a hotel or airport — is left alone too.

### How re-auth works (MAC rotation)

Captive portals cap usage **per device (MAC address)**. When connectivity drops,
SirenSong **cycles the connection down and back up**, which makes NetworkManager
roll a fresh random MAC. The portal then sees a brand-new device and grants a
new session, which the HTTP login claims. This is why it can re-authenticate
indefinitely — each pass looks like a different device.

Rotation needs `wifi.cloned-mac-address=random` to be in effect. SirenSong sets
that itself before each reconnect:

```bash
nmcli connection modify --temporary "<SSID>" wifi.cloned-mac-address random
```

`--temporary` means **in-memory only** — nothing is written to your saved
profile and the change is forgotten when NetworkManager restarts, so your
configuration is left alone.

That call can be denied, though: modifying a profile is a different polkit
action than activating one, so it may fail where `nmcli connection up` succeeds
(over SSH, for instance). SirenSong logs it and carries on rather than aborting.
If you hit that, set the global default yourself:

```ini
# /etc/NetworkManager/NetworkManager.conf
[connection]
wifi.cloned-mac-address=random
```

Either way SirenSong **verifies the outcome** rather than assuming it, comparing
the in-use MAC before and after the cycle. If it did not change, you get a
warning naming the fix — instead of a silent retry loop, which is what an
already-capped MAC otherwise looks like:

```
WARN MAC unchanged (AA:BB:CC:DD:EE:FF) — the portal still sees the same device, so
     re-auth will keep failing; set [connection] wifi.cloned-mac-address=random in
     /etc/NetworkManager/NetworkManager.conf
```

Note that a MAC pinned on the profile itself
(`802-11-wireless.cloned-mac-address` set to a fixed address or `permanent`)
overrides the global default — the before/after check catches that case too.

## Disclaimer

This is a personal automation/educational project. The MAC randomization
deliberately resets the captive portal's per-device usage allowance. Use it on
networks you are authorized to use and respect the venue's terms of service and
any applicable acceptable-use policies. You are responsible for how you use it.

## License

This project is licensed under the MIT License. See the [LICENSE](LICENSE) file
for details.
