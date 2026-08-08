# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.6] - 2026-08-08

### Changed

- The README's travel-router section now links to [`router/`](router/), which
  ships the procd service, UCI config and the hardware-slider handler. It
  previously described the service files without saying they exist in the repo,
  so anyone following it would have written them from scratch — and would never
  have found the post-reflash checklist of settings a firmware upgrade silently
  restores to their defaults.

## [0.1.5] - 2026-08-08

### Fixed

- **Re-auth no longer rotates first.** The portal is now tried on the MAC we
  already have, and a rotation only happens if it refuses. Arriving somewhere,
  the per-device cap has usually reset, so the common case is a form POST rather
  than a 31s daemon restart plus a spent MAC. Measured against a live portal on
  a MAC it had never seen: **~1.5s instead of ~35s.**
- **"Nothing to do here" is no longer counted as a failure.** `reconcile`
  returns `Online` / `Declined` / `Failed`, and only `Failed` drives the
  exponential backoff. On the router, a booting repeater reports `Down` on every
  tick while associating, which used to walk the poll interval to 15 minutes —
  so the moment the portal became reachable was the moment we had stopped
  looking.
- **`authenticity_token` was parsed with `name="…"\s+value="…"`**, but Rails'
  `hidden_field_tag` puts `id=` between the two. Any splash rendered that way
  failed forever while reporting that the markup had changed. The `continue_url`
  pattern already allowed for this; the two disagreed.
- **The portal POST reported success on any HTTP status** — 401, 500, or a
  splash re-served saying the free allowance was used up. Rejection was
  indistinguishable from acceptance.
- **A relative form action lost its path and query.**
  `action="/splash/billing_pick?mauth=…"` was discarded and replaced with a
  synthesised URL, dropping parameters the portal needs.
- **The connectivity probe could misread a working link.** A single 64-byte read
  can return a partial status line: `"HTTP"` classified as offline (costing a
  needless rotation), `"HTTP/1.1 20"` as a portal.
- **`ap_is_up()` matched interfaces that were down** — `ip -br link show` prints
  `DOWN … <NO-CARRIER,BROADCAST,MULTICAST,UP>` for a stopped device, and the
  check looked for `UP` anywhere in that line. Startup declared success on an AP
  that never began beaconing, printed the QR code, and then the watch loop's
  stricter `operstate` check disagreed — tearing down and restarting the hotspot
  every tick, indefinitely. Both checks now share one function. **This affects
  0.1.4.**
- **The saved hotspot passphrase was world-readable between creation and
  `chmod`** (measured `0644`, then `0600`), and an existing `0644` file was
  never repaired. It is now created `0600`.
- **Passphrases and SSIDs were not validated against what `create_ap` accepts.**
  It enforces 8–63 and 1–32 _characters_; sirensong checked `>= 8` _bytes_ with
  no upper bound, and never checked the saved file at all. An embedded newline
  reached hostapd's generated config unquoted.
- **`create_ap` arguments are now fenced with `--`.** It parses with GNU
  `getopt`, which permutes, so an SSID or passphrase beginning with `-` was read
  as a flag.
- **`sirensong Starbucks Customer`** (missing quotes) silently watched a network
  called `Customer`. Extra positional arguments are now an error that names the
  fix.
- **Non-UTF-8 arguments panicked.** SSIDs are byte strings; a latin-1 café name
  crashed with a backtrace instead of an error.
- **`portal_host` and `http_login` used different redirect policies** for the
  same URL, so they could disagree about where a chain ended — enough to make
  sirensong decline its own portal.
- **Ctrl-C during a rotation was ignored.** Signals were only observed between
  watch ticks, and a rotation blocks for as long as association takes. A second
  signal now always exits, and stops the hotspot on the way out.

- **The hotspot picks a frequency the radio can actually transmit on.** It
  defaulted to channel 1 regardless of where the Wi-Fi station was. Cards
  advertise two interface combinations — a permissive one capped at a single
  channel, and a narrow one allowing two — so an AP on a different channel from
  the station quietly demanded the narrow one and failed to start. The channel
  is now taken from the station unless `--hotspot-channel` says otherwise.
- **`--hotspot` moves the link to 2.4GHz when the current frequency cannot host
  an AP.** Under some regulatory domains every 5GHz frequency is flagged
  `no IR`: the card may associate there as a client but never initiate
  radiation. A laptop on 5GHz then cannot host a hotspot at all — not on 5GHz,
  and not on 2.4GHz either, since that needs the restricted combination. The
  switch is `--temporary`, warns that it is trading link speed for the hotspot,
  and only ever happens for `--hotspot`.
- **AP capability is decided by frequency, not channel number.** Channel numbers
  repeat across bands: on a Wi-Fi 6E card, 161 is both 5805 MHz (`no IR`) and
  6755 MHz (usable). Matching on the number made an unusable station channel
  look fine, so the check above silently did nothing.
- **Hotspot failures name the cause that applies.** The error blamed the
  regulatory domain unconditionally, which sent a user auditing `iw reg get`
  while their domain was set correctly and the real problem was the frequency.

### Changed

- **Default watch interval 60s → 30s**, and 5s until the first successful
  sign-in of a run. Time-to-notice was the largest single component of downtime
  — larger than the sign-in itself.
- **Rotation waits on evidence, not a clock.** The 10-minute cap was actively
  harmful: giving up does not cancel anything, and reporting failure let the
  watch loop rotate again and restart a nearly-complete association. Give-up is
  now a stall detector over the daemon state, live MAC and default route.
- **Rotation only happens on the configured network**, and only when the Wi-Fi
  station is actually carrying traffic — on ethernet or a tether it declines
  rather than restarting the repeater daemon for nothing.
- **A recognised portal with no free-plan form is `Declined`, not `Failed`.** A
  fresh MAC lands on the same unusable page, so rotating cannot help.

### Added

- **`router/`** — procd service, UCI config, hardware-slider handler and setup
  notes for running on a GL.iNet travel router. The **GL.iNet backend is now
  validated end-to-end against a live Meraki portal**, including unattended cold
  boot: ~30s from power-on to authenticated, on a MAC the portal had never seen.
  Tested on one device (Slate 7 / GL-BE3600, firmware 4.9.0) at one venue.
- README now documents the router backend, which was shipped in 0.1.3 and
  mentioned only in this changelog.

## [0.1.4] - 2026-08-07

### Changed

- **The README's hotspot example no longer carries real credentials.** The
  sample output was pasted verbatim from a live run, so it published an actual
  machine's hostname alongside the 20-character passphrase that run had
  generated. Both are placeholders now — `hostname-sirensong` and
  `AbCdEfGhJkMnPqRsTuVw`, the latter drawn from the same alphabet
  `generate_passphrase` uses, so it still shows the right shape without
  resembling live output.
- The example QR code is rendered in full instead of elided after a single row,
  so the docs show what `--hotspot` actually prints. It is generated by
  `print_join_details` itself rather than hand-drawn, and encodes the
  placeholder credentials.

Documentation only — no behaviour change, and no reason to upgrade from 0.1.3
for anything but the docs.

## [0.1.3] - 2026-08-07

### Added

- **Share the connection over a Wi-Fi hotspot** (`--hotspot`): brings up an AP
  on the same radio for the lifetime of the process, so one café login serves
  your phone too. Off unless asked for. The network is named
  `<hostname>-sirensong` (`--hotspot-ssid` to override) and a 20-character
  passphrase is generated on first use and remembered in
  `~/.config/sirensong/hotspot.pass` (mode `0600`), so devices pair once rather
  than on every run. `--hotspot-pass` and `SIRENSONG_HOTSPOT_PASS` take
  precedence, in that order; prefer the environment variable, since arguments
  are readable by other users via `ps`. Credentials print with a QR code in the
  standard `WIFI:` provisioning format, so joining is pointing a camera rather
  than typing twenty random characters on a phone.
- **The hotspot stops when sirensong stops**, so the radio isn't left beaconing
  and draining battery — via a `Drop` guard, covering clean exit, Ctrl-C,
  `SIGTERM` and panics (not `SIGKILL`). The watch loop also checks the AP each
  tick and restarts it if it died, since otherwise devices behind it lose
  network with nothing to say why. Liveness is a single
  `/sys/class/net/<iface>/operstate` read rather than spawning `iw` and `ip`
  (~6.6 ms measured) each poll.
- A warning before starting when the regulatory domain is `country 00`, under
  which most channels forbid transmitting. That is the cause of hostapd's
  otherwise opaque "could not determine operating frequency", and the warning
  names the fix.
- **Experimental: GL.iNet travel routers as a second backend**, selected at
  runtime by the presence of `/etc/init.d/repeater`. Rotation there writes the
  new MAC into UCI and restarts the `gl-repeater` daemon, which applies it when
  it recreates the station vdev — roughly 30s, and without the device reboot
  GL.iNet's own `ubus repeater connect` performs. Waiting is driven by the
  daemon's reported state rather than a fixed duration, because a reconnect
  takes ~30s on a quiet network and minutes on a busy one. (Not validated
  end-to-end at the time of this release — that landed later; see Unreleased.
  The NetworkManager path is untouched and unaffected.)

## [0.1.2] - 2026-07-30

### Fixed

- **Never cycle a network that is merely offline**: the reconnect guard was
  `ssid_in_range`, which only proves a matching AP is within radio range — not
  that we are on it. In a city with a Starbucks nearby, a home internet outage
  satisfied that check, so sirensong dropped the working home connection and
  jumped to the café AP. It now identifies the network by **who answered** its
  connectivity probe instead of by name.

### Added

- **Portal-identity guard**: `Reach` splits the connectivity probe's three
  outcomes apart instead of collapsing them into a bool — `Online` (`204`),
  `Intercepted` (any other HTTP reply, meaning something answered on our
  behalf), and `Down` (no DNS, no TCP, or nothing resembling HTTP). A captive
  portal answers; a dead uplink does not. `reconcile` now acts only when a
  portal answers _and_ `portal_host` reports a host under `network-auth.com`; an
  unrecognized portal, or silence while associated, leaves the connection
  untouched. `ssid_in_range` survives only as a cheap skip when nothing is
  associated at all.
- Tests for `classify_response`, `is_known_portal` (including lookalike hosts
  such as `network-auth.com.example.org`), and `portal_host` against a mock
  splash — the guard is unit-testable, unlike the `nmcli` layer it replaces.

## [0.1.1] - 2026-07-30

### Added

- **Self-configuring MAC randomization**: `connect_to_wifi` now runs
  `nmcli connection modify --temporary <ssid> wifi.cloned-mac-address random`
  before cycling the connection, so re-auth no longer depends on the user having
  pre-set a global `[connection] wifi.cloned-mac-address=random`. `--temporary`
  keeps the change in memory only — no saved profile is written and it is
  forgotten on NetworkManager restart. Best-effort: modifying a profile is a
  different polkit action (`settings.modify.system`) than activating one
  (`network-control`) and can be denied over SSH, so failure is logged at debug
  and the reconnect proceeds.
- **MAC rotation verification**: the in-use MAC (`GENERAL.HWADDR`, which
  reflects the cloned address rather than the burned-in one) is compared before
  and after the cycle, and an unchanged MAC warns with the exact config fix.
  Previously a missing setting meant every reconnect re-associated on the
  already-capped MAC, the portal refused, and the retry loop backed off to the
  15-minute cap with no indication why. Also catches a MAC pinned on the profile
  itself, which overrides the global default.
- `parse_wifi_device` (matches the exact `:wifi` suffix so `wifi-p2p`
  pseudo-devices are skipped) and `unescape_terse` for nmcli's escaped colons,
  both verified against live nmcli 1.58 output.

### Changed

- **Portal warnings name the real cause**: "no `billing_pick` form on splash;
  portal markup may have changed" blamed a regression for what is far more often
  a different vendor's portal. It now states that only the Meraki free-plan
  splash is supported and offers both explanations.
- **README** documents `cargo install sirensong`, scopes the other-SSID claim
  (open networks with no portal work, as do other Meraki free-plan splashes; any
  other vendor or Meraki mode does not), corrects the `--quiet` description,
  adds the missing `-V`/`--version` entry, and notes `RUST_LOG=debug`.

## [0.1.0] - 2026-07-30

Initial release.

### Added

- **Captive-portal login over plain HTTP**: GET a `generate_204` captive-detect
  URL to pick up the splash session cookie, scrape the free-plan form's Rails
  `authenticity_token` (scoped to the `billing_pick` form so the prepaid form's
  token is never used), then POST it. No browser, no chromedriver.
- **Watch mode** (the default): polls connectivity and re-runs the portal login
  only when it drops, with exponential backoff capped at 15 minutes and prompt
  SIGTERM/Ctrl-C shutdown. `--once` authenticates a single time and exits.
- **MAC rotation for re-auth**: cycles the connection down and back up so
  NetworkManager rolls a fresh random MAC, which the portal treats as a
  brand-new device — the mechanism that defeats the per-device time cap.
- **Reconnect guards**: declares itself offline only after two consecutive
  failed probes, so a momentary blip does not trigger a needless reconnect, and
  only reconnects when the target SSID is actually in range — off-site it is a
  no-op instead of dropping the current network to hunt for the Starbucks AP.
- CLI: `[SSID]` positional (default `Starbucks Customer`), `--once`,
  `--interval`, `--quiet`, `--help`, `--version`.
- Structured logging via `tracing`. Info level reads as user-facing status —
  watching, good to browse, connection dropped — announced on state changes only
  so a healthy connection stays quiet; the mechanics (nmcli cycling,
  association, portal form POST) sit at debug behind `RUST_LOG`.

### Removed

- **Selenium/chromedriver**: the browser automation path and the `thirtyfour`
  dependency are gone; authentication is HTTP-only.
- The Python implementation and the bundled systemd unit (watch mode covers the
  latter).

[unreleased]: https://github.com/junjitree/sirensong/compare/v0.1.6...main
[0.1.6]: https://github.com/junjitree/sirensong/compare/v0.1.5...v0.1.6
[0.1.5]: https://github.com/junjitree/sirensong/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/junjitree/sirensong/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/junjitree/sirensong/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/junjitree/sirensong/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/junjitree/sirensong/compare/d70766b...v0.1.1
[0.1.0]: https://github.com/junjitree/sirensong/commits/d70766b
