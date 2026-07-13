# SirenSong

SirenSong answers the Starbucks siren's call for you — it automates login to
Starbucks Wi-Fi captive portals. It authenticates over plain HTTP (falling back
to a headless browser) and rolls a fresh MAC address on each reconnect, so the
portal's per-device time limit never runs out.

## Prerequisites

- `nmcli` (NetworkManager command-line tool)
- Google Chrome
- ChromeDriver (installed and in your `PATH`)
- Rust and Cargo

## Installation

1. Clone this repository:

   ```bash
   git clone https://github.com/junjitree/sirensong.git
   cd sirensong
   ```

2. Build with Cargo:

```bash
cargo build --release
```

Alternatively, you can install it directly to your system's Cargo bin directory:

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
  -q, --quiet            Suppress status output (errors still print)
  -h, --help             Print help
```

Before launching a browser, sirensong does a lightweight `generate_204`
connectivity check and exits early if you are already online, so repeated runs
are cheap.

### Watch mode (default)

Captive-portal sessions expire on the venue's clock. Watch mode keeps you
authenticated by polling connectivity and re-running the portal login only when
it drops. Just run it in a terminal while you are at the café and stop it
(Ctrl-C) when you leave:

```bash
sirensong                       # watch "Starbucks Customer"
sirensong "Some Other Cafe"     # watch a different SSID
sirensong -i 30                 # poll every 30s
```

It only reconnects when it confirms it is offline (several failed probes, so a
momentary blip does not trigger a needless reconnect) **and** the target SSID is
in range. If you walk away and forget to stop it, it leaves whatever network you
join next alone instead of hunting for the Starbucks AP.

### How re-auth works (MAC rotation)

Captive portals cap usage **per device (MAC address)**. SirenSong relies on
NetworkManager being configured to randomize the MAC on each connection —

```ini
# /etc/NetworkManager/NetworkManager.conf
[connection]
wifi.cloned-mac-address=random
```

When connectivity drops, SirenSong **cycles the connection down and back up**,
which makes NetworkManager roll a fresh random MAC. The portal then sees a
brand-new device and grants a new session, which the browser flow accepts. This
is why it can re-authenticate indefinitely — each pass looks like a different
device. Without `cloned-mac-address=random`, re-auth on the same (already
capped) MAC would be rejected.

## Disclaimer

This is a personal automation/educational project. The MAC randomization
deliberately resets the captive portal's per-device usage allowance. Use it on
networks you are authorized to use and respect the venue's terms of service and
any applicable acceptable-use policies. You are responsible for how you use it.

## License

This project is licensed under the MIT License. See the [LICENSE](LICENSE) file
for details.
