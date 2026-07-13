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

### Options

```
starbypass [OPTIONS] [SSID]

  -w, --watch            Stay running; re-auth whenever connectivity drops
  -i, --interval <SECS>  Watch poll interval in seconds (default: 60)
  -q, --quiet            Suppress status output (errors still print)
  -h, --help             Print help
```

Before launching a browser, starbypass does a lightweight `generate_204`
connectivity check and exits early if you are already online, so repeated runs
are cheap.

### Watch mode

Captive-portal sessions expire on the venue's clock. Watch mode keeps you
authenticated by polling connectivity and re-running the portal login only when
it drops:

```bash
starbypass --watch --interval 60 "Starbucks Customer"
```

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

#### Run it as a service

A ready-made systemd user unit lives at
[`packaging/starbypass.service`](packaging/starbypass.service):

```bash
mkdir -p ~/.config/systemd/user
cp packaging/starbypass.service ~/.config/systemd/user/
# edit ExecStart if your binary path or SSID differ
systemctl --user daemon-reload
systemctl --user enable --now starbypass.service
sudo loginctl enable-linger "$USER"   # so it runs without an active login
```

## License

This project is licensed under the MIT License. See the [LICENSE](LICENSE) file
for details.
