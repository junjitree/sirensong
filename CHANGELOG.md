# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[unreleased]: https://github.com/junjitree/sirensong/compare/v0.1.1...main
[0.1.1]: https://github.com/junjitree/sirensong/compare/d70766b...v0.1.1
[0.1.0]: https://github.com/junjitree/sirensong/commits/d70766b
