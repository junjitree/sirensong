# StarBypass

StarBypass automates the process of connecting to Starbucks Wi-Fi captive
portals. It drives a headless browser to click "Accept" on the terms and
conditions page.

## Prerequisites

- `nmcli` (NetworkManager command-line tool)
- Google Chrome
- ChromeDriver (installed and in your `PATH`)
- Rust and Cargo

## Installation

1. Clone this repository:

   ```bash
   git clone https://github.com/junjitree/starbypass.git
   cd starbypass
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
starbypass [SSID]
```

Otherwise, run the application using Cargo:

```bash
cargo run -- [SSID]
```

Or run the compiled binary:

```bash
./target/release/starbypass [SSID]
```

It defaults to "Starbucks Customer" if no SSID is provided.

By default starbypass runs in **watch mode** — it stays up and re-authenticates
whenever the portal drops. Pass `--once` to authenticate a single time and exit.

### Options

```
starbypass [OPTIONS] [SSID]

  -o, --once             Authenticate once and exit (default: watch and re-auth on drop)
  -i, --interval <SECS>  Watch poll interval in seconds (default: 60)
  -q, --quiet            Suppress status output (errors still print)
  -h, --help             Print help
```

Before launching a browser, starbypass does a lightweight `generate_204`
connectivity check and exits early if you are already online, so repeated runs
are cheap.

### Watch mode (default)

Captive-portal sessions expire on the venue's clock. Watch mode keeps you
authenticated by polling connectivity and re-running the portal login only when
it drops. Just run it in a terminal while you are at the café and stop it
(Ctrl-C) when you leave:

```bash
starbypass                       # watch "Starbucks Customer"
starbypass "Some Other Cafe"     # watch a different SSID
starbypass -i 30                 # poll every 30s
```

It only reconnects when it confirms it is offline (several failed probes, so a
momentary blip does not trigger a needless reconnect) **and** the target SSID is
in range. If you walk away and forget to stop it, it leaves whatever network you
join next alone instead of hunting for the Starbucks AP.

### How re-auth works (MAC rotation)

Captive portals cap usage **per device (MAC address)**. StarBypass relies on
NetworkManager being configured to randomize the MAC on each connection —

```ini
# /etc/NetworkManager/NetworkManager.conf
[connection]
wifi.cloned-mac-address=random
```

When connectivity drops, StarBypass **cycles the connection down and back up**,
which makes NetworkManager roll a fresh random MAC. The portal then sees a
brand-new device and grants a new session, which the browser flow accepts. This
is why it can re-authenticate indefinitely — each pass looks like a different
device. Without `cloned-mac-address=random`, re-auth on the same (already
capped) MAC would be rejected.

## License

This project is licensed under the MIT License. See the [LICENSE](LICENSE) file
for details.
