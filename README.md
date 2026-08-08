# SirenSong

SirenSong answers the Starbucks siren's call for you — it automates login to
Starbucks Wi-Fi captive portals. It authenticates over plain HTTP and rolls a
fresh MAC address on each reconnect, so the portal's per-device time limit never
runs out.

**Which Starbucks?** The ones running a **Cisco Meraki free-plan splash.** There
is no single worldwide Starbucks portal — markets are run by different
licensees, and each picks its own captive-portal vendor. So this working at your
Starbucks says nothing about the one in the next country, and possibly not the
next city. Check before assuming:

```bash
RUST_LOG=debug sirensong --once
```

The log names the portal it found and whether it could use it.
`no Meraki free-plan form on this splash page` means a different vendor, or
Meraki running vouchers, click-through terms, or SMS instead — none of which are
supported.

## Prerequisites

Sirensong picks one of two backends at runtime, so what it needs depends on
where it runs.

**On a Linux machine** (the well-tested path):

- `nmcli` (NetworkManager command-line tool)
- Rust and Cargo (only to build from source)

MAC randomization is configured automatically — see
[How re-auth works](#how-re-auth-works-mac-rotation) if it can't be applied.

**On a GL.iNet travel router** — see
[Running on a travel router](#running-on-a-glinet-travel-router). It uses `uci`,
`ubus` and the `gl-repeater` daemon instead, and `nmcli` is neither present nor
needed.

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
  -i, --interval <SECS>  Watch poll interval in seconds (default: 30)
  -q, --quiet            Only log errors (overrides RUST_LOG)
  -h, --help             Print help
  -V, --version          Print version

      --hotspot              Share this connection over a Wi-Fi hotspot
      --hotspot-ssid <NAME>  Network name (default: <hostname>-sirensong)
      --hotspot-pass <PASS>  Passphrase (or set SIRENSONG_HOTSPOT_PASS)
      --hotspot-channel <N>  AP channel (default 1)
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
configured for vouchers, click-through terms, or SMS — is not supported.
Sirensong recognises that there is no free-plan form to submit, says so, and
stops there rather than retrying or rotating: a fresh MAC would land on the same
unusable page.

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

### Sharing the connection (`--hotspot`)

**Your phone is capped even when your laptop isn't.** MAC rotation gives the
machine running sirensong an unlimited session, but every other device you own
still gets the portal's usual hour and then stops. Phones can't escape that on
their own: iOS randomizes its MAC per network, but the address is _stable for
that SSID_, so the portal recognizes it on every visit and resumes the same
clock. Rotating it means "Forget This Network" and rejoining by hand, once an
hour.

`--hotspot` removes the problem instead of automating it. sirensong brings up a
Wi-Fi hotspot on the radio it is already using, and devices that join it reach
the internet through your machine's NAT — so **they never speak to the portal at
all**. The only MAC the café ever sees is the one sirensong is already rotating,
and one rotation covers everything behind the hotspot, however many devices that
is.

```bash
sirensong --hotspot
```

That is the whole command. The hotspot lands on **whatever channel your Wi-Fi is
already using** — most cards will only host an AP on a second channel in a
restricted mode that often fails outright, so following the station is both the
reliable choice and one less thing to look up. `--hotspot-channel` overrides it,
and warns if you pick one the station is not on.

The network is named `<hostname>-sirensong`, and on first run a 20-character
passphrase is generated and saved to `~/.config/sirensong/hotspot.pass` (mode
`0600`). Credentials print with a QR code — point your phone's camera at it to
join, no typing:

```
  network:  hostname-sirensong
  password: AbCdEfGhJkMnPqRsTuVw

    █▀▀▀▀▀█ █▀ ██ ▀▀██ ▄▀█▄█▄ █▀▀▀▀▀█
    █ ███ █ ▀▄▄▄▀  ▀ ▄  █▄▀▀  █ ███ █
    █ ▀▀▀ █ ▀█    █▀ ▀ ▀▄█ ▀▄ █ ▀▀▀ █
    ▀▀▀▀▀▀▀ ▀▄▀ ▀▄█ ▀▄█▄▀ ▀ ▀ ▀▀▀▀▀▀▀
    █▄ ███▀██▀█▄█▀ █ ▀ █ ▀  ▄▀▄ ▀▄█▀▀
    ▄▀▀█  ▀▀ ██ ▄ █▀▀ ▀██▀▀▀▀▀███ █▄▄
    ▄▄█▄▄▄▀▀▄▀███▀▀▀█ ▀█ ▀▀█▄▄▀▄▀█▄█▀
    ▄▀█▄▄▄▀█▄█▄▀█▀ ▄▀█▄█▀▄██▀  ██ ██
    ▄ █▀▀▀▀▀ █▄█▀▄ ▄▄▄▄ ▄▀▀▀▀█▄ ▄▄▀
    ▄▄█▄▀ ▀▄▀ ▄█ █▀▄ ▄▀▀▀ ▄▄▀▄█  ▄ ▀█
    ▄█ ▄ ▄▀ ▄█▄ ▄█▀▀▄▀▀▀ ▀▀▄█▄ ███▄▄▄
    █ █▄▄ ▀▄▄█▄  ▀▄▀█▄▄ ▄ ▀▄▀█████▄█▄
    ▀▀  ▀▀▀▀▄▀  █ █▀█  ▀█ █▀█▀▀▀██▄▀▀
    █▀▀▀▀▀█ █ ▄█▄▀▄▀  ██▀▀ ▄█ ▀ █ ▄█▀
    █ ███ █ ██▄▀▀█ ▀ ██▄  ▄▀▀▀▀▀█▀▄ ▀
    █ ▀▀▀ █  ██▄▄▄▀▄█▄▀▀ ▄  █ ▄ ▀▀██▀
    ▀▀▀▀▀▀▀ ▀        ▀▀▀▀▀▀  ▀▀  ▀ ▀

  scan with your phone's camera to join
```

The passphrase is **remembered**, so devices pair once and reconnect on their
own after that. Override with `--hotspot-pass`, or `SIRENSONG_HOTSPOT_PASS`.
Note that either way it is handed to `create_ap` as an argument, so it is
visible to other local users via `ps` for as long as the hotspot runs — the
environment variable keeps it out of sirensong's own command line and your shell
history, but it is not a secret from the machine.

**The hotspot stops when sirensong stops.** Ctrl-C tears it down rather than
leaving the radio beaconing and draining battery. That covers clean exit,
Ctrl-C, `SIGTERM` and panics — but not `kill -9`, after which you would need
`sudo create_ap --stop <iface>`. If the AP dies on its own mid-session, the
watch loop notices within one poll and restarts it.

Requirements:

- [`create_ap`](https://github.com/lakinduakash/linux-wifi-hotspot) and
  `dnsmasq`
- `sudo` (prompts in your terminal, so run it interactively rather than
  backgrounded)
- A card that can run AP and client modes at once — check `iw phy phyN info` for
  `valid interface combinations` listing `{ managed }` alongside `{ AP }`
- **A regulatory domain that permits transmitting.** If `iw reg get` reports
  `country 00`, most channels are `no IR` and the hotspot will fail with an
  opaque hostapd error. Set your country and persist it:

  ```bash
  sudo iw reg set PH
  echo 'options cfg80211 ieee80211_regdom=PH' | sudo tee /etc/modprobe.d/cfg80211.conf
  ```

Rotating the MAC does not disturb the hotspot: the AP is a separate virtual
interface, and on a card with multi-channel concurrency it holds its channel
while the client reassociates. Devices stay connected and only lose internet for
the length of the reconnect.

### Running on a GL.iNet travel router

Sirensong also runs **on** a GL.iNet travel router, so the router itself stays
signed in and everything behind it — phone, laptop, whatever — rides one
authenticated connection without any of them talking to the portal.

**Tested on exactly one device: a GL.iNet Slate 7 (GL-BE3600) on firmware 4.9.0,
against one Starbucks.** It is written against GL's `gl-repeater` daemon rather
than anything model-specific, so other GL.iNet models plausibly work, but nobody
has tried. Treat anything beyond that one combination as unverified.

The backend is chosen at runtime by the presence of `/etc/init.d/repeater` — no
flag, no build feature. On the router it uses `uci` and `ubus`; `nmcli` is never
called.

What differs from the laptop path:

- **Rotation restarts the repeater daemon.** The Qualcomm driver only accepts a
  MAC when the station interface is _created_, and that happens when the daemon
  starts — so rotating means writing the address into UCI and restarting
  `gl-repeater`. Roughly 30s, and no device reboot (GL's own
  `ubus call repeater connect` reboots, taking 2–6 minutes).
- **Rotation is a last resort.** Sirensong tries the portal on the MAC it
  already has first, which usually succeeds — arriving somewhere, the cap has
  normally reset — and that takes ~2s. It only rotates when the portal actually
  refuses.
- **It only rotates on the network you configured.** Anywhere else it will still
  sign into a Meraki free-plan splash if it finds one, but it will not touch the
  radio's identity.
- **It will not rotate when the Wi-Fi station isn't the uplink.** On ethernet or
  a tether, changing the station MAC would achieve nothing, so it declines and
  says so.
- `--hotspot` is rejected on this backend: the router is already an access
  point.

Run it as a service so a router powered on at the café signs itself in
unattended. `/etc/init.d/sirensong` (procd, `START=95`, after the network and
`gl-repeater`) plus `/etc/config/sirensong`:

```
config sirensong 'main'
	option enabled  '1'
	option ssid     'Starbucks Customer'
	option interval '30'
	option loglevel 'info'
```

Measured cold boot to authenticated: ~30s. Logs go to syslog —
`logread -e sirensong`.

**One firmware setting matters.** GL's own captive-portal helper drops Tailscale
around a portal login and does not reliably bring it back, which leaves the
router online but unreachable. Sirensong does that login itself, so turn the
helper off on the network with the portal:

```sh
uci set repeater.@network[N].auto_portal='0'   # find N by SSID, don't hardcode
uci commit repeater
```

The service files themselves — `sirensong.init`, `sirensong.config` and an
optional handler that toggles sirensong from the router's hardware slider — live
in [`router/`](router/), along with install steps and a post-reflash checklist
of the settings that a firmware upgrade or factory reset silently restores to
their defaults.

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
