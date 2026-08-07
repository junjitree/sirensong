use std::env;
use std::process::Command;
use std::time::Duration;

use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

const DEFAULT_SSID: &str = "Starbucks Customer";
const CONNECTIVITY_HOST: &str = "connectivitycheck.gstatic.com";
/// Captive-detect URL the HTTP login hits first; a portal intercepts it and
/// redirects to the splash page. Parameterized into `http_login` so tests can
/// point it at a mock server.
const CAPTIVE_DETECT_URL: &str = "http://connectivitycheck.gstatic.com/generate_204";
/// Present as a real browser so portals that gate on User-Agent (rejecting or
/// serving stripped pages to non-browser clients) treat the HTTP fast path the
/// same as the Selenium flow.
const BROWSER_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
/// Upper bound on watch-mode backoff between failed reconcile attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(900);
/// Registrable domain of the Cisco Meraki splash portals we can log into (hosts
/// look like `n143.network-auth.com`). This is how we recognize *our* portal
/// without ever asking which network we are on.
const PORTAL_HOST: &str = "network-auth.com";
/// Poll interval while waiting for a rotated repeater to come back.
const ROTATE_POLL: Duration = Duration::from_secs(2);
/// Give up after this many polls with no observable progress — no change in the
/// daemon's state and no change in the live MAC. This is a *stall* detector, not
/// a duration budget: how long a reconnect takes varies by minutes between a
/// quiet network and a busy café, so we wait on evidence rather than a guess.
const ROTATE_STALL_POLLS: u32 = 30;
/// Absolute backstop so a wedged daemon can't block the watch loop forever.
/// Should never be reached; the stall detector is the real mechanism.
const ROTATE_MAX_POLLS: u32 = 300;
/// How long to wait for the hotspot to actually start beaconing.
const AP_START_POLL: Duration = Duration::from_secs(1);
const AP_START_POLLS: u32 = 25;

struct Config {
    ssid: String,
    once: bool,
    interval: Duration,
    quiet: bool,
    hotspot: Option<Hotspot>,
}

/// Share the authenticated connection over a Wi-Fi hotspot, for the lifetime of
/// this process. Started before the watch loop and torn down on exit, so the
/// radio isn't left burning battery once sirensong stops.
///
/// The AP is a virtual interface on the same radio as the client. That survives
/// a MAC rotation — verified on an ath11k card, where the AP held its channel
/// while the station changed band, BSSID and MAC — provided the card advertises
/// multi-channel concurrency and the regulatory domain permits beaconing.
struct Hotspot {
    ssid: String,
    pass: String,
    channel: u32,
}

fn print_help() {
    println!(
        "sirensong - automate Starbucks Wi-Fi captive portal login\n\n\
         USAGE:\n    sirensong [OPTIONS] [SSID]\n\n\
         ARGS:\n    <SSID>    Wi-Fi network name (default: \"{DEFAULT_SSID}\")\n\n\
         OPTIONS:\n\
         \x20   -o, --once             Authenticate once and exit (default: watch and re-auth on drop)\n\
         \x20   -i, --interval <SECS>  Watch poll interval in seconds (default: 60)\n\
         \x20   -q, --quiet            Only log errors (overrides RUST_LOG)\n\
         \x20   -h, --help             Print this help\n\
         \x20   -V, --version          Print version\n\n\
         HOTSPOT (watch mode only, needs create_ap and root):\n\
         \x20       --hotspot <SSID>       Share this connection over a Wi-Fi hotspot\n\
         \x20       --hotspot-pass <PASS>  Passphrase (or set SIRENSONG_HOTSPOT_PASS)\n\
         \x20       --hotspot-channel <N>  AP channel (default 1; keep it off the client's band)\n\n\
         The hotspot is stopped when sirensong exits, so the radio doesn't keep\n\
         draining battery. Prefer SIRENSONG_HOTSPOT_PASS over --hotspot-pass:\n\
         arguments are visible to other users via ps.\n\n\
         Log verbosity is otherwise controlled by RUST_LOG (e.g. RUST_LOG=debug)."
    );
}

fn parse_args_from<I: Iterator<Item = String>>(args: I) -> Result<Config, String> {
    let mut ssid = None;
    let mut once = false;
    let mut interval = Duration::from_secs(60);
    let mut quiet = false;
    let mut hotspot_ssid = None;
    let mut hotspot_pass = None;
    let mut hotspot_channel = 1u32;
    let mut args = args.peekable();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-o" | "--once" => once = true,
            "-q" | "--quiet" => quiet = true,
            "--hotspot" => {
                hotspot_ssid = Some(
                    args.next()
                        .ok_or_else(|| "--hotspot requires an SSID".to_string())?,
                );
            }
            "--hotspot-pass" => {
                hotspot_pass = Some(
                    args.next()
                        .ok_or_else(|| "--hotspot-pass requires a passphrase".to_string())?,
                );
            }
            "--hotspot-channel" => {
                let val = args
                    .next()
                    .ok_or_else(|| "--hotspot-channel requires a number".to_string())?;
                hotspot_channel = val
                    .parse()
                    .map_err(|_| format!("invalid --hotspot-channel value: {val}"))?;
            }
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-i" | "--interval" => {
                let val = args
                    .next()
                    .ok_or_else(|| "--interval requires a value (seconds)".to_string())?;
                let secs: u64 = val
                    .parse()
                    .map_err(|_| format!("invalid --interval value: {val}"))?;
                if secs == 0 {
                    return Err("--interval must be greater than 0".to_string());
                }
                interval = Duration::from_secs(secs);
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown option: {other}"));
            }
            other => ssid = Some(other.to_string()),
        }
    }

    // Env var is preferred over the flag: argv is world-readable via ps.
    let hotspot = match hotspot_ssid {
        None => None,
        Some(hs_ssid) => {
            let pass = hotspot_pass
                .or_else(|| env::var("SIRENSONG_HOTSPOT_PASS").ok())
                .ok_or_else(|| {
                    "--hotspot needs a passphrase: set SIRENSONG_HOTSPOT_PASS or pass --hotspot-pass"
                        .to_string()
                })?;
            if pass.len() < 8 {
                return Err("hotspot passphrase must be at least 8 characters (WPA2)".to_string());
            }
            Some(Hotspot {
                ssid: hs_ssid,
                pass,
                channel: hotspot_channel,
            })
        }
    };

    if hotspot.is_some() && once {
        return Err(
            "--hotspot needs watch mode; it would stop immediately under --once".to_string(),
        );
    }

    Ok(Config {
        ssid: ssid.unwrap_or_else(|| DEFAULT_SSID.to_string()),
        once,
        interval,
        quiet,
        hotspot,
    })
}

fn init_logging(quiet: bool) {
    // --quiet forces error-only and ignores RUST_LOG; otherwise RUST_LOG wins,
    // defaulting to info.
    let filter = if quiet {
        tracing_subscriber::EnvFilter::new("error")
    } else {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

/// What a connectivity probe actually found.
///
/// `Intercepted` and `Down` both mean "no internet", but they are very
/// different situations: a captive portal *answers* (with a splash page or a
/// redirect to one), whereas a dead uplink answers nothing at all. Telling them
/// apart is what lets us re-authenticate our own portal while leaving someone
/// else's merely-broken network alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// `204` — real internet.
    Online,
    /// Some other HTTP reply: something answered on our behalf, i.e. a portal.
    Intercepted,
    /// No usable reply at all — no DNS, no TCP, or nothing that looks like HTTP.
    Down,
}

/// Classify the first bytes of a reply to the `generate_204` probe.
fn classify_response(head: &str) -> Reach {
    if head.starts_with("HTTP/1.0 204") || head.starts_with("HTTP/1.1 204") {
        Reach::Online
    } else if head.starts_with("HTTP/") {
        Reach::Intercepted
    } else {
        Reach::Down
    }
}

/// Lightweight captive-portal check: a raw HTTP GET to a well-known
/// `generate_204` endpoint. Real internet returns `204`; a captive portal
/// intercepts with a `200`/redirect; a dead uplink never replies.
fn probe_blocking() -> Reach {
    use std::io::{Read, Write};
    use std::net::{TcpStream, ToSocketAddrs};

    let addr = match (CONNECTIVITY_HOST, 80).to_socket_addrs() {
        Ok(mut it) => match it.next() {
            Some(a) => a,
            None => return Reach::Down,
        },
        Err(_) => return Reach::Down,
    };
    let mut stream = match TcpStream::connect_timeout(&addr, Duration::from_secs(5)) {
        Ok(s) => s,
        Err(_) => return Reach::Down,
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    let req = format!(
        "GET /generate_204 HTTP/1.0\r\nHost: {CONNECTIVITY_HOST}\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(req.as_bytes()).is_err() {
        return Reach::Down;
    }

    let mut buf = [0u8; 64];
    match stream.read(&mut buf) {
        Ok(0) | Err(_) => Reach::Down,
        Ok(n) => classify_response(&String::from_utf8_lossy(&buf[..n])),
    }
}

async fn probe() -> Reach {
    tokio::task::spawn_blocking(probe_blocking)
        .await
        .unwrap_or(Reach::Down)
}

async fn is_online() -> bool {
    probe().await == Reach::Online
}

/// Parse the currently-active SSID from `nmcli -t -f active,ssid dev wifi`.
/// Splitting on the first `:` only (via `strip_prefix`) keeps SSIDs that
/// themselves contain a colon intact.
fn parse_active_ssid(nmcli_out: &str) -> Option<String> {
    for line in nmcli_out.lines() {
        if let Some(rest) = line.strip_prefix("yes:") {
            return Some(rest.to_string());
        }
    }
    None
}

fn active_ssid() -> Option<String> {
    let out = Command::new("nmcli")
        .args(["-t", "-f", "active,ssid", "dev", "wifi"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_active_ssid(&String::from_utf8_lossy(&out.stdout))
}

/// Whether `ssid` appears in an `nmcli -t -f ssid dev wifi list` dump.
fn ssid_in_scan(scan: &str, ssid: &str) -> bool {
    scan.lines().any(|line| line == ssid)
}

/// Is the target SSID currently in range? Used to gate reconnection so the
/// service never yanks the radio off another network (e.g. home Wi-Fi) hunting
/// for a Starbucks AP that isn't there.
fn ssid_in_range(ssid: &str) -> bool {
    Command::new("nmcli")
        .args(["-t", "-f", "ssid", "dev", "wifi", "list"])
        .output()
        .map(|o| ssid_in_scan(&String::from_utf8_lossy(&o.stdout), ssid))
        .unwrap_or(false)
}

/// Declare offline only after two consecutive failed probes, so a single
/// transient blip does not trigger a needless (and disruptive) reconnect.
async fn confirmed_offline() -> bool {
    for i in 0..2 {
        if is_online().await {
            return false;
        }
        if i < 1 {
            sleep(Duration::from_secs(2)).await;
        }
    }
    true
}

/// Name of the first Wi-Fi device in an `nmcli -t -f DEVICE,TYPE device status`
/// dump. Matching the exact `:wifi` suffix skips `wifi-p2p` pseudo-devices.
fn parse_wifi_device(status: &str) -> Option<String> {
    status
        .lines()
        .find_map(|line| line.strip_suffix(":wifi").map(str::to_string))
}

fn wifi_device() -> Option<String> {
    let out = Command::new("nmcli")
        .args(["-t", "-f", "DEVICE,TYPE", "device", "status"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_wifi_device(&String::from_utf8_lossy(&out.stdout))
}

/// nmcli's terse output escapes the colons inside a MAC (`AA\:BB\:…`), so strip
/// the backslashes to get something printable and comparable.
fn unescape_terse(s: &str) -> String {
    s.trim().replace('\\', "")
}

/// The MAC currently *in use* on `dev` — `GENERAL.HWADDR` reflects the cloned
/// address, not the burned-in one (that's `GENERAL.PERM-HWADDR`), which is
/// exactly what we need to tell whether randomization actually took effect.
fn device_mac(dev: &str) -> Option<String> {
    let out = Command::new("nmcli")
        .args(["-g", "GENERAL.HWADDR", "device", "show", dev])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mac = unescape_terse(&String::from_utf8_lossy(&out.stdout));
    if mac.is_empty() { None } else { Some(mac) }
}

/// Ask NetworkManager to assign this profile a fresh random MAC on every
/// activation, so re-auth doesn't depend on the user having pre-configured a
/// global `[connection] wifi.cloned-mac-address=random`.
///
/// `--temporary` keeps the change in memory only: nothing is written to the
/// profile on disk and it is forgotten on NetworkManager restart, so we never
/// mutate saved configuration.
///
/// Best-effort. Modifying a profile is a different polkit action
/// (`settings.modify.system`) than activating one (`network-control`), so this
/// can be denied — over SSH, for instance — even when `connection up` succeeds.
/// It also fails when no saved profile exists yet. Either way a global setting
/// may already cover us, so we log and carry on; `connect_to_wifi` verifies the
/// real outcome by comparing MACs.
fn ensure_random_mac(ssid: &str) -> bool {
    let ok = Command::new("nmcli")
        .args([
            "connection",
            "modify",
            "--temporary",
            ssid,
            "wifi.cloned-mac-address",
            "random",
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if ok {
        debug!(ssid, "set wifi.cloned-mac-address=random (in-memory only)");
    } else {
        debug!(
            ssid,
            "could not set wifi.cloned-mac-address; relying on existing NM config"
        );
    }
    ok
}

// ---------------------------------------------------------------------------
// GL.iNet OpenWrt backend
//
// On a GL.iNet travel router the Wi-Fi link belongs to the `gl-repeater` daemon
// rather than NetworkManager, and the Qualcomm driver only accepts a station MAC
// at the moment it *creates* the vdev — changing it afterwards updates the netdev
// but not the radio, and association then fails on a 4-way handshake mismatch.
// Since vdev creation happens when the daemon starts, rotating means writing the
// new address into UCI and restarting the daemon. That takes ~30s and, unlike
// GL.iNet's own `ubus call repeater connect`, does not reboot the device.
// ---------------------------------------------------------------------------

/// Which stack owns the Wi-Fi link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    /// Desktop Linux: NetworkManager via `nmcli`.
    NetworkManager,
    /// GL.iNet OpenWrt: the `gl-repeater` daemon via `uci`.
    GlRepeater,
}

impl Backend {
    /// Pick a backend from what is actually installed, so one binary serves both.
    fn detect() -> Self {
        if std::path::Path::new("/etc/init.d/repeater").exists() {
            Backend::GlRepeater
        } else {
            Backend::NetworkManager
        }
    }
}

/// A random locally-administered unicast MAC — bit 1 of the first octet set,
/// bit 0 clear. Globally-administered or multicast addresses are rejected (GL.iNet's
/// own UI warns the second hex digit may not be odd).
fn random_laa_mac() -> Option<String> {
    use std::io::Read;
    let mut buf = [0u8; 6];
    std::fs::File::open("/dev/urandom")
        .ok()?
        .read_exact(&mut buf)
        .ok()?;
    buf[0] = (buf[0] & 0xFE) | 0x02;
    Some(
        buf.iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(":"),
    )
}

fn uci_get(key: &str) -> Option<String> {
    let out = Command::new("uci").args(["-q", "get", key]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if v.is_empty() { None } else { Some(v) }
}

fn uci_set(key: &str, val: &str) -> bool {
    Command::new("uci")
        .args(["set", &format!("{key}={val}")])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn uci_commit(pkg: &str) -> bool {
    Command::new("uci")
        .args(["commit", pkg])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Index of the saved repeater network with this SSID. The list is positional,
/// so it must be resolved by name — never hardcoded.
fn repeater_index(ssid: &str) -> Option<usize> {
    (0..16).find(|i| uci_get(&format!("repeater.@network[{i}].ssid")).as_deref() == Some(ssid))
}

/// SSID from a `ubus call repeater status` payload. Hand-rolled rather than
/// pulling in a JSON dependency for one field.
fn parse_repeater_ssid(status_json: &str) -> Option<String> {
    let at = status_json.find("\"ssid\"")?;
    let rest = &status_json[at + 6..];
    let open = rest.find('"')?;
    let tail = &rest[open + 1..];
    let close = tail.find('"')?;
    let ssid = &tail[..close];
    if ssid.is_empty() {
        None
    } else {
        Some(ssid.to_string())
    }
}

/// The daemon's own view of the link — `connecting`, `connected`, `failed`.
/// Watching this beats timing the reconnect, because it reports progress rather
/// than requiring us to guess a duration.
fn parse_repeater_state(status_json: &str) -> Option<String> {
    let at = status_json.find("\"state_s\"")?;
    let rest = &status_json[at + 9..];
    let open = rest.find('"')?;
    let tail = &rest[open + 1..];
    let close = tail.find('"')?;
    Some(tail[..close].to_string())
}

fn repeater_status() -> Option<String> {
    let out = Command::new("ubus")
        .args(["call", "repeater", "status"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn repeater_ssid() -> Option<String> {
    parse_repeater_ssid(&repeater_status()?)
}

fn repeater_state() -> Option<String> {
    parse_repeater_state(&repeater_status()?)
}

/// MAC field from a single `ip -br link show <dev>` line, lowercased.
fn parse_link_mac(line: &str) -> Option<String> {
    line.split_whitespace().nth(2).map(str::to_lowercase)
}

fn link_mac(dev: &str) -> Option<String> {
    let out = Command::new("ip")
        .args(["-br", "link", "show", dev])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_link_mac(&String::from_utf8_lossy(&out.stdout))
}

fn default_route_present() -> bool {
    Command::new("ip")
        .args(["route", "show", "default"])
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false)
}

/// Rotate the repeater's MAC and wait for the link to come back on it.
///
/// Writes the address to both places the daemon reads, then restarts the daemon
/// so it recreates the station vdev with the new address. Returns once the
/// station is up on that MAC with a default route — typically ~30s.
fn rotate_repeater(cfg: &Config) -> bool {
    let Some(idx) = repeater_index(&cfg.ssid) else {
        error!("no saved network on the router for {}", cfg.ssid);
        return false;
    };
    let Some(new_mac) = random_laa_mac() else {
        error!("could not generate a MAC address");
        return false;
    };

    let before = uci_get("wireless.sta.ifname").as_deref().and_then(link_mac);
    debug!(
        idx,
        attached_to = current_ssid().as_deref().unwrap_or("?"),
        before = before.as_deref().unwrap_or("?"),
        new = %new_mac,
        "rotating repeater MAC"
    );

    let writes = [
        (
            format!("repeater.@network[{idx}].macaddr"),
            format!("r,{new_mac}"),
        ),
        ("wireless.sta.macaddr".to_string(), new_mac.clone()),
    ];
    for (key, val) in &writes {
        if !uci_set(key, val) {
            error!("could not write {key}");
            return false;
        }
    }
    for pkg in ["repeater", "wireless"] {
        if !uci_commit(pkg) {
            error!("could not commit {pkg} config");
            return false;
        }
    }

    // The daemon applies the MAC when it starts; a device reboot is not needed.
    let restarted = Command::new("/etc/init.d/repeater")
        .arg("restart")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !restarted {
        error!("could not restart the repeater daemon");
        return false;
    }

    // Wait on evidence, not on the clock. A reconnect takes ~30s on a quiet
    // network and several minutes on a busy one, so watch the daemon's state and
    // the live MAC, and only give up once nothing is moving. The interface name
    // flips between sta0/sta1 across restarts, so re-read it every poll.
    let want = new_mac.to_lowercase();
    let mut last = String::new();
    let mut stalled = 0u32;

    for poll in 0..ROTATE_MAX_POLLS {
        std::thread::sleep(ROTATE_POLL);

        let state = repeater_state().unwrap_or_default();
        let live = uci_get("wireless.sta.ifname")
            .as_deref()
            .and_then(link_mac)
            .unwrap_or_default();

        if live == want && default_route_present() {
            debug!(mac = %want, polls = poll + 1, "repeater back up on the rotated MAC");
            return true;
        }

        // `failed` is transient, not terminal: the daemon reports it between
        // retries and then keeps going, so treating it as fatal aborts a
        // reconnect that would have succeeded (observed recovering ~8 minutes
        // later on a congested network). Let the stall detector decide instead —
        // an oscillation between `connecting` and `failed` is the daemon
        // working, whereas sitting in one state with nothing moving is not.
        let seen = format!("{state}|{live}");
        if state == "connecting" || seen != last {
            stalled = 0;
            last = seen;
        } else {
            stalled += 1;
            if stalled >= ROTATE_STALL_POLLS {
                warn!(state = %state, mac = %live, "repeater stopped making progress");
                return false;
            }
        }
    }
    warn!("repeater never settled; giving up so the watch loop can retry");
    false
}

/// The network we are currently attached to, however the platform reports it.
fn current_ssid() -> Option<String> {
    match Backend::detect() {
        Backend::GlRepeater => repeater_ssid(),
        Backend::NetworkManager => active_ssid(),
    }
}

/// Get onto the target network with a fresh MAC, using whichever stack is present.
fn connect_to_wifi(cfg: &Config) -> bool {
    match Backend::detect() {
        Backend::GlRepeater => rotate_repeater(cfg),
        Backend::NetworkManager => connect_via_networkmanager(cfg),
    }
}

/// (Re)connect to the Wi-Fi, forcing a fresh activation so NetworkManager rolls
/// a new randomized MAC (`wifi.cloned-mac-address=random`). The new MAC is what
/// lets us re-authenticate past the captive portal's per-device usage cap — the
/// portal treats each MAC as a brand-new visitor.
///
/// Tries an existing saved profile first (`connection down` then `up`), then
/// falls back to associating with an open network (`dev wifi connect`).
fn connect_via_networkmanager(cfg: &Config) -> bool {
    debug!(ssid = %cfg.ssid, "cycling connection for fresh MAC");

    let dev = wifi_device();
    let mac_before = dev.as_deref().and_then(device_mac);

    // Opt the profile into per-activation MAC randomization before cycling, so
    // the `up` below is what picks up the new address.
    ensure_random_mac(&cfg.ssid);

    // Down first so `up` is a full re-activation and NM re-applies a new
    // random MAC. Ignore failure here — the profile may already be down.
    let _ = Command::new("nmcli")
        .args(["connection", "down", &cfg.ssid])
        .output();

    let up_ok = Command::new("nmcli")
        .args(["connection", "up", &cfg.ssid])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if !up_ok {
        let connect_ok = Command::new("nmcli")
            .args(["dev", "wifi", "connect", &cfg.ssid])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !connect_ok {
            debug!(ssid = %cfg.ssid, "failed to connect to Wi-Fi");
            return false;
        }
        // The profile only exists now that `dev wifi connect` created it, so the
        // earlier attempt was a no-op; set it here to arm the *next* cycle. This
        // pass keeps whatever MAC it associated with, which is fine — a device
        // the portal has never seen doesn't need rotating yet.
        ensure_random_mac(&cfg.ssid);
    }

    for tries in 1..20 {
        if active_ssid().as_deref() == Some(cfg.ssid.as_str()) {
            debug!(tries, "associated");
            let mac_after = dev.as_deref().and_then(device_mac);
            report_mac_rotation(mac_before.as_deref(), mac_after.as_deref());
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    debug!(ssid = %cfg.ssid, "association never became active");
    false
}

/// Tell the user when MAC rotation silently isn't happening. Without it the
/// portal still sees the device whose allowance is already spent, so re-auth
/// fails forever and the retry/backoff loop gives no clue why.
///
/// Only warns on a confirmed *unchanged* MAC; if either reading is missing we
/// stay quiet rather than guess.
fn report_mac_rotation(before: Option<&str>, after: Option<&str>) {
    match (before, after) {
        (Some(before), Some(after)) if before == after => warn!(
            "MAC unchanged ({after}) — the portal still sees the same device, so re-auth will keep failing; \
             set [connection] wifi.cloned-mac-address=random in /etc/NetworkManager/NetworkManager.conf"
        ),
        (Some(before), Some(after)) => debug!(%before, %after, "MAC rotated"),
        _ => debug!("could not read MAC before/after; skipping rotation check"),
    }
}

/// Whether a splash page's host belongs to the Meraki portal family we know how
/// to log into. Matching the registrable domain rather than an exact host keeps
/// working as Meraki shuffles its numbered front-ends (`n143`, `n747`, …), while
/// the leading dot stops `network-auth.com.example.org` from matching.
fn is_known_portal(host: &str) -> bool {
    host == PORTAL_HOST || host.ends_with(&format!(".{PORTAL_HOST}"))
}

/// Follow the captive-detect URL to whatever splash page intercepted it and
/// report the host that served it.
///
/// This is the identity check that replaces "which SSID am I on": a portal that
/// answers from `*.network-auth.com` is demonstrably ours, no matter what the
/// network is called — and on a network whose uplink is merely dead, nothing
/// answers at all. Returns `None` if we ended up online after all, or if the
/// request failed.
async fn portal_host(detect_url: &str) -> Option<String> {
    let client = reqwest::Client::builder()
        .cookie_store(true)
        .user_agent(BROWSER_UA)
        .timeout(Duration::from_secs(15))
        .build()
        .ok()?;
    let resp = client.get(detect_url).send().await.ok()?;
    if resp.status().as_u16() == 204 {
        return None;
    }
    resp.url().host_str().map(str::to_string)
}

/// Minimal HTML entity decode — enough to recover attribute values (notably the
/// form `action` URL) from the Meraki splash page, whose markup is served
/// HTML-escaped. `&amp;` is decoded last so we don't re-expand other entities.
fn html_unescape(s: &str) -> String {
    s.replace("&#x2F;", "/")
        .replace("&#x2713;", "✓")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// First capture group of `pattern` in `haystack`, if it matches.
fn capture(haystack: &str, pattern: &str) -> Option<String> {
    regex::Regex::new(pattern)
        .ok()?
        .captures(haystack)?
        .get(1)
        .map(|m| m.as_str().to_string())
}

/// Isolate the `billing_pick` (free plan) `<form>…</form>` block so we read the
/// authenticity token that belongs to *that* form, not the prepaid form which
/// carries its own token.
fn billing_pick_form(html: &str) -> Option<&str> {
    let action = html.find("billing_pick")?;
    let start = html[..action].rfind("<form")?;
    let end = html[start..].find("</form>").map(|e| start + e)?;
    Some(&html[start..end])
}

/// HTTP-only captive-portal login (the fast path). Mirrors what the browser
/// does on the Cisco Meraki splash: GET a captive-detect URL (redirects to the
/// splash, setting a session cookie), scrape the free form's Rails
/// `authenticity_token`, then POST it. No browser, no chromedriver.
///
/// Returns whether the POST was accepted; the caller confirms real
/// connectivity. Any parsing/network failure returns `false` (and logs a
/// `warn!` if the markup looks like it changed, since that needs a code fix).
async fn http_login(detect_url: &str) -> bool {
    let client = match reqwest::Client::builder()
        .cookie_store(true)
        .user_agent(BROWSER_UA)
        .timeout(Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            debug!(error = %e, "could not build HTTP client");
            return false;
        }
    };

    // Follows the 307 to the splash page and picks up the session cookie.
    let resp = match client.get(detect_url).send().await {
        Ok(r) => r,
        Err(e) => {
            debug!(error = %e, "captive-detect request failed");
            return false;
        }
    };
    if resp.status().as_u16() == 204 {
        return true; // already online
    }

    let final_url = resp.url().clone();
    let html = match resp.text().await {
        Ok(body) => html_unescape(&body),
        Err(e) => {
            debug!(error = %e, "could not read splash page body");
            return false;
        }
    };

    let Some(form) = billing_pick_form(&html) else {
        warn!(
            "no Meraki free-plan form on this splash page — sirensong only handles the Meraki \
             free-plan portal, so this may be a different vendor (or Meraki markup that changed)"
        );
        return false;
    };

    let Some(token) = capture(form, r#"name="authenticity_token"\s+value="([^"]+)""#) else {
        warn!(
            "authenticity_token missing from the free-plan form — the Meraki splash markup \
             has likely changed"
        );
        return false;
    };
    let continue_url =
        capture(form, r#"name="continue_url"[^>]*value="([^"]*)""#).unwrap_or_default();
    let post_url = capture(form, r#"action="([^"]*billing_pick[^"]*)""#)
        .filter(|u| u.starts_with("http"))
        .unwrap_or_else(|| {
            format!(
                "{}://{}/splash/billing_pick",
                final_url.scheme(),
                final_url.host_str().unwrap_or("network-auth.com")
            )
        });

    debug!(url = %post_url, "submitting free-plan portal form");
    let params = [
        ("utf8", "✓"),
        ("authenticity_token", token.as_str()),
        ("pricing_plan", "free"),
        ("commit", "Continue"),
        ("continue_url", continue_url.as_str()),
    ];
    match client.post(&post_url).form(&params).send().await {
        Ok(_) => true,
        Err(e) => {
            debug!(error = %e, "portal POST failed");
            false
        }
    }
}

/// Poll connectivity a few times, giving the portal a moment to let us through.
async fn wait_online() -> bool {
    for _ in 0..6 {
        if is_online().await {
            return true;
        }
        sleep(Duration::from_secs(1)).await;
    }
    false
}

/// One reconciliation pass: if already online, do nothing. Otherwise decide
/// whether this network is ours to act on, and only then reconnect — which
/// rolls a fresh MAC (see `connect_to_wifi`) so the portal treats us as a new
/// device — and authenticate over HTTP against the captive portal.
///
/// The decision is made by asking *who answered*, never by asking which network
/// we are on. A portal that identifies itself is ours to log into; silence means
/// the uplink is simply dead and we should keep our hands off.
async fn reconcile(cfg: &Config) -> bool {
    match probe().await {
        Reach::Online => {
            debug!("already online");
            return true;
        }
        Reach::Intercepted => {
            // Something answered on our behalf. Touch the connection only if it
            // is a portal we can actually log into — otherwise we are a guest on
            // somebody else's network and have no business cycling it.
            match portal_host(CAPTIVE_DETECT_URL).await {
                Some(host) if is_known_portal(&host) => {
                    debug!(host, "captive portal recognized");
                }
                Some(host) => {
                    debug!(
                        host,
                        "unrecognized captive portal; leaving this network alone"
                    );
                    return false;
                }
                None => {
                    debug!("captive portal did not identify itself; leaving this network alone");
                    return false;
                }
            }
        }
        Reach::Down => {
            // Nothing answered at all. If we are associated to something, its
            // uplink is dead rather than gated — the case where a home outage
            // used to send us hunting for the café AP. Leave it alone.
            if active_ssid().is_some() {
                debug!("offline with no portal answering; leaving current network alone");
                return false;
            }
            // Associated to nothing, so there is no connection to disrupt. Still
            // skip the join if the target is not even in range.
            if !ssid_in_range(&cfg.ssid) {
                debug!(ssid = %cfg.ssid, "not associated and target SSID not in range");
                return false;
            }
        }
    }

    if !connect_to_wifi(cfg) {
        error!("couldn't join {}", cfg.ssid);
        return false;
    }

    // A fresh association occasionally restores connectivity on its own
    // (e.g. an open network with no portal); skip auth if so.
    if is_online().await {
        debug!("online after associating");
        return true;
    }

    debug!("authenticating over HTTP");
    if http_login(CAPTIVE_DETECT_URL).await && wait_online().await {
        debug!(method = "http", "authenticated");
        true
    } else {
        error!("could not sign in to the Wi-Fi portal");
        false
    }
}

// ---------------------------------------------------------------------------
// Hotspot: share the authenticated link over a virtual AP on the same radio.
// ---------------------------------------------------------------------------

/// Name of the interface currently in AP mode, from an `iw dev` dump. `create_ap`
/// names it `ap0` in practice, but it will pick another index if that's taken,
/// so find it by mode rather than assuming.
fn parse_ap_interface(iw_dev: &str) -> Option<String> {
    let mut current: Option<&str> = None;
    for line in iw_dev.lines() {
        let t = line.trim();
        if let Some(name) = t.strip_prefix("Interface ") {
            current = Some(name.trim());
        } else if t == "type AP" {
            return current.map(str::to_string);
        }
    }
    None
}

fn ap_interface() -> Option<String> {
    let out = Command::new("iw").arg("dev").output().ok()?;
    parse_ap_interface(&String::from_utf8_lossy(&out.stdout))
}

/// Whether the AP interface exists *and* is actually up. `create_ap` can leave a
/// created-but-DISABLED interface behind when hostapd fails to pick an operating
/// frequency — most often because the regulatory domain forbids beaconing on the
/// chosen channel (`iw reg get` showing `country 00` is the usual culprit).
fn ap_is_up() -> bool {
    let Some(dev) = ap_interface() else {
        return false;
    };
    Command::new("ip")
        .args(["-br", "link", "show", &dev])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("UP"))
        .unwrap_or(false)
}

/// Stops the hotspot when dropped, so it doesn't outlive sirensong and sit
/// there draining battery. Covers clean exit, Ctrl-C and panics; nothing can
/// cover SIGKILL.
struct HotspotGuard {
    iface: String,
}

impl Drop for HotspotGuard {
    fn drop(&mut self) {
        info!("stopping hotspot");
        let stopped = Command::new("sudo")
            .args(["create_ap", "--stop", &self.iface])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !stopped {
            warn!(
                iface = %self.iface,
                "could not stop the hotspot; check with: sudo create_ap --stop {}",
                self.iface
            );
        }
    }
}

/// Bring up the hotspot on the same radio as the client connection. Returns a
/// guard that tears it down on drop; `None` if it could not be started.
fn hotspot_start(hs: &Hotspot) -> Option<HotspotGuard> {
    let iface = wifi_device()?;
    info!(
        ssid = %hs.ssid,
        channel = hs.channel,
        "starting hotspot on {}", iface
    );

    let status = Command::new("sudo")
        .args([
            "create_ap",
            "--daemon",
            "-c",
            &hs.channel.to_string(),
            &iface,
            &iface,
            &hs.ssid,
            &hs.pass,
        ])
        .status();
    if !matches!(status, Ok(s) if s.success()) {
        error!("could not launch create_ap (is it installed, and does sudo work here?)");
        return None;
    }

    // Wait for it to actually beacon rather than assuming a duration — the
    // interface can appear seconds before hostapd finishes, or never come up at
    // all if the regulatory domain blocks the channel.
    let guard = HotspotGuard {
        iface: iface.clone(),
    };
    for _ in 0..AP_START_POLLS {
        std::thread::sleep(AP_START_POLL);
        if ap_is_up() {
            info!(ssid = %hs.ssid, "hotspot is up — devices can join it now");
            return Some(guard);
        }
    }

    error!(
        "hotspot interface never came up. Most often the regulatory domain forbids \
         beaconing — check `iw reg get`; if it says `country 00`, set your country \
         (e.g. `sudo iw reg set PH`) and retry"
    );
    None // guard drops here, cleaning up the half-started AP
}

/// Exponential backoff for watch mode: `interval * 2^(fails-1)`, capped.
fn backoff_delay(base: Duration, fails: u32) -> Duration {
    let shift = fails.saturating_sub(1).min(16);
    let secs = base.as_secs().saturating_mul(1u64 << shift);
    Duration::from_secs(secs).min(MAX_BACKOFF)
}

/// Sleep for `dur`, but wake early and return `true` if a shutdown signal
/// (SIGTERM / Ctrl-C) arrives — so `systemctl stop` exits promptly.
async fn sleep_or_shutdown(dur: Duration, sigterm: Option<&mut Signal>) -> bool {
    match sigterm {
        Some(sig) => tokio::select! {
            _ = sleep(dur) => false,
            _ = sig.recv() => true,
            _ = tokio::signal::ctrl_c() => true,
        },
        None => tokio::select! {
            _ = sleep(dur) => false,
            _ = tokio::signal::ctrl_c() => true,
        },
    }
}

async fn run_watch(cfg: &Config) {
    info!(
        "watching {} every {}s — I'll keep you signed in and re-connect if it drops",
        cfg.ssid,
        cfg.interval.as_secs()
    );

    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => Some(s),
        Err(e) => {
            warn!(error = %e, "no SIGTERM handler; Ctrl-C still works");
            None
        }
    };

    let mut consecutive_failures = 0u32;
    // Only announce on state *changes*, so a healthy connection stays quiet
    // instead of reprinting "you're good to browse" on every poll.
    let mut online_announced = false;
    loop {
        // `||` short-circuits: reconcile only runs after we've confirmed we're
        // actually offline (several failed probes), not on a single blip.
        let online = !confirmed_offline().await;
        if !online && online_announced {
            info!("connection dropped — signing back in");
            online_announced = false;
        }

        let delay = if online || reconcile(cfg).await {
            if !online_announced {
                info!("you're good to browse — still watching");
                online_announced = true;
            }
            consecutive_failures = 0;
            cfg.interval
        } else {
            consecutive_failures += 1;
            let backoff = backoff_delay(cfg.interval, consecutive_failures);
            debug!(failures = consecutive_failures, "reconcile failed");
            warn!(
                "couldn't get you online — retrying in {}s",
                backoff.as_secs()
            );
            backoff
        };

        if sleep_or_shutdown(delay, sigterm.as_mut()).await {
            info!("stopping — no longer watching");
            break;
        }
    }
}

#[tokio::main]
async fn main() {
    let cfg = match parse_args_from(env::args().skip(1)) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("error: {e}\n");
            print_help();
            std::process::exit(2);
        }
    };

    init_logging(cfg.quiet);

    if cfg.once {
        if reconcile(&cfg).await {
            info!("you're good to browse");
            std::process::exit(0);
        }
        std::process::exit(1);
    }

    // Held for the lifetime of the watch loop; dropping it stops the hotspot so
    // the radio isn't left beaconing after we exit. `--once` is rejected at parse
    // time, and that path uses process::exit, which would skip this anyway.
    let _hotspot = match &cfg.hotspot {
        None => None,
        Some(hs) => match Backend::detect() {
            Backend::GlRepeater => {
                warn!("--hotspot ignored: this router already serves its own Wi-Fi");
                None
            }
            Backend::NetworkManager => match hotspot_start(hs) {
                Some(guard) => Some(guard),
                None => {
                    error!("could not start the hotspot; continuing without it");
                    None
                }
            },
        },
    };

    run_watch(&cfg).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_from(args: &[&str]) -> Result<Config, String> {
        parse_args_from(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn defaults_when_no_args() {
        let cfg = cfg_from(&[]).unwrap();
        assert_eq!(cfg.ssid, DEFAULT_SSID);
        assert!(!cfg.once);
        assert!(!cfg.quiet);
        assert_eq!(cfg.interval, Duration::from_secs(60));
    }

    #[test]
    fn parses_ssid_and_flags() {
        let cfg = cfg_from(&["--once", "-q", "-i", "30", "My Cafe"]).unwrap();
        assert_eq!(cfg.ssid, "My Cafe");
        assert!(cfg.once);
        assert!(cfg.quiet);
        assert_eq!(cfg.interval, Duration::from_secs(30));
    }

    #[test]
    fn rejects_bad_interval() {
        assert!(cfg_from(&["-i", "0"]).is_err());
        assert!(cfg_from(&["-i", "abc"]).is_err());
        assert!(cfg_from(&["--interval"]).is_err());
    }

    #[test]
    fn rejects_unknown_flag() {
        assert!(cfg_from(&["--nope"]).is_err());
    }

    #[test]
    fn active_ssid_keeps_colons() {
        let out = "no:HomeNet\nyes:My:Weird:SSID\nno:Other\n";
        assert_eq!(parse_active_ssid(out), Some("My:Weird:SSID".to_string()));
    }

    #[test]
    fn active_ssid_none_when_disconnected() {
        assert_eq!(parse_active_ssid("no:A\nno:B\n"), None);
    }

    #[test]
    fn ssid_in_scan_matches_exact_line() {
        let scan = "HomeNet\nStarbucks Customer\nCoffeeGuest\n";
        assert!(ssid_in_scan(scan, "Starbucks Customer"));
        assert!(!ssid_in_scan(scan, "Starbucks"));
        assert!(!ssid_in_scan("", "Starbucks Customer"));
    }

    #[test]
    fn wifi_device_skips_p2p_pseudo_devices() {
        let status = "lo:loopback\np2p-dev-wlp3s0:wifi-p2p\nwlp3s0:wifi\nenp0s31f6:ethernet\n";
        assert_eq!(parse_wifi_device(status), Some("wlp3s0".to_string()));
    }

    #[test]
    fn wifi_device_none_without_wifi() {
        assert_eq!(parse_wifi_device("lo:loopback\nenp0s31f6:ethernet\n"), None);
    }

    #[test]
    fn terse_mac_loses_escaping() {
        assert_eq!(
            unescape_terse("AA\\:BB\\:CC\\:DD\\:EE\\:FF\n"),
            "AA:BB:CC:DD:EE:FF"
        );
    }

    #[test]
    fn unescape_recovers_action_url() {
        let s = "action=\"https:&#x2F;&#x2F;n747.network-auth.com&#x2F;splash&#x2F;billing_pick\"";
        assert!(html_unescape(s).contains("https://n747.network-auth.com/splash/billing_pick"));
    }

    // Two forms, each with its own token; we must read the free form's token
    // even though the prepaid form appears first.
    const SAMPLE_SPLASH: &str = r#"
        <form action="https://n747.network-auth.com/splash/billing_prepaid" method="post">
        <input type="hidden" name="authenticity_token" value="PREPAID_TOKEN" />
        <input type="text" name="prepaid_card" />
        </form>
        <form action="https://n747.network-auth.com/splash/billing_pick" method="post">
        <input name="utf8" type="hidden" value="✓" />
        <input type="hidden" name="authenticity_token" value="FREE+tok/123==" />
        <input type="radio" name="pricing_plan" id="option_free" value="free" />
        <input type="hidden" name="continue_url" id="continue_url" value="https%3A%2F%2Fwww.starbucks.ph%2F" />
        </form>
    "#;

    #[test]
    fn billing_pick_form_scopes_to_free_form() {
        let form = billing_pick_form(SAMPLE_SPLASH).unwrap();
        assert!(form.contains("billing_pick"));
        assert!(!form.contains("PREPAID_TOKEN"));
        assert_eq!(
            capture(form, r#"name="authenticity_token"\s+value="([^"]+)""#),
            Some("FREE+tok/123==".to_string())
        );
        assert_eq!(
            capture(form, r#"name="continue_url"[^>]*value="([^"]*)""#),
            Some("https%3A%2F%2Fwww.starbucks.ph%2F".to_string())
        );
    }

    // create_ap usually names it ap0, but picks another index if that's taken —
    // so it has to be found by mode, not by name.
    #[test]
    fn finds_ap_interface_by_mode() {
        let iw = "phy#0\n\tInterface ap0\n\t\tifindex 5\n\t\ttype AP\n\tInterface wlp2s0\n\t\tifindex 3\n\t\ttype managed\n";
        assert_eq!(parse_ap_interface(iw).as_deref(), Some("ap0"));

        let renamed = "phy#0\n\tInterface wlp2s0\n\t\ttype managed\n\tInterface ap1\n\t\ttype AP\n";
        assert_eq!(parse_ap_interface(renamed).as_deref(), Some("ap1"));
    }

    #[test]
    fn no_ap_interface_when_none_in_ap_mode() {
        let iw = "phy#0\n\tInterface wlp2s0\n\t\tifindex 3\n\t\ttype managed\n";
        assert_eq!(parse_ap_interface(iw), None);
        assert_eq!(parse_ap_interface(""), None);
    }

    #[test]
    fn hotspot_requires_a_passphrase() {
        let err = cfg_from(&["--hotspot", "myap"]).err().unwrap();
        assert!(err.contains("passphrase"), "got: {err}");
    }

    #[test]
    fn hotspot_rejects_short_passphrase() {
        let err = cfg_from(&["--hotspot", "myap", "--hotspot-pass", "short"])
            .err()
            .unwrap();
        assert!(err.contains("8 characters"), "got: {err}");
    }

    // --once exits immediately, so a hotspot would be torn down the moment it
    // came up. Better to say so than to silently do nothing useful.
    #[test]
    fn hotspot_conflicts_with_once() {
        let err = cfg_from(&["--hotspot", "myap", "--hotspot-pass", "goodpass1", "--once"])
            .err()
            .unwrap();
        assert!(err.contains("watch mode"), "got: {err}");
    }

    #[test]
    fn hotspot_parses_with_defaults() {
        let cfg = cfg_from(&["--hotspot", "myap", "--hotspot-pass", "goodpass1"]).unwrap();
        let hs = cfg.hotspot.expect("hotspot configured");
        assert_eq!(hs.ssid, "myap");
        assert_eq!(hs.channel, 1);
    }

    #[test]
    fn backoff_grows_then_caps() {
        let base = Duration::from_secs(60);
        assert_eq!(backoff_delay(base, 1), Duration::from_secs(60));
        assert_eq!(backoff_delay(base, 2), Duration::from_secs(120));
        assert_eq!(backoff_delay(base, 3), Duration::from_secs(240));
        assert_eq!(backoff_delay(base, 4), Duration::from_secs(480));
        // 60 * 2^4 = 960 -> capped at 900
        assert_eq!(backoff_delay(base, 5), MAX_BACKOFF);
        assert_eq!(backoff_delay(base, 99), MAX_BACKOFF);
    }

    // End-to-end exercise of the HTTP fast path against a mock Meraki splash:
    // captive-detect -> 307 -> splash HTML -> scrape free-form token -> POST.
    #[tokio::test]
    async fn http_login_drives_full_flow() {
        use wiremock::matchers::{body_string_contains, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let base = server.uri();

        // Captive detect redirects to the splash page.
        Mock::given(method("GET"))
            .and(path("/generate_204"))
            .respond_with(
                ResponseTemplate::new(307).insert_header("location", format!("{base}/splash")),
            )
            .mount(&server)
            .await;

        // Splash: prepaid form first (its own token), then the free form.
        let html = format!(
            r#"<form action="{base}/splash/billing_prepaid" method="post">
                 <input type="hidden" name="authenticity_token" value="PREPAID_TOKEN" />
               </form>
               <form action="{base}/splash/billing_pick" method="post">
                 <input name="utf8" type="hidden" value="✓" />
                 <input type="hidden" name="authenticity_token" value="FREE_TOKEN" />
                 <input type="radio" name="pricing_plan" id="option_free" value="free" />
                 <input type="hidden" name="continue_url" value="https%3A%2F%2Fexample%2F" />
               </form>"#
        );
        Mock::given(method("GET"))
            .and(path("/splash"))
            .respond_with(ResponseTemplate::new(200).set_body_string(html))
            .mount(&server)
            .await;

        // POST must carry the FREE form's token and the free plan.
        Mock::given(method("POST"))
            .and(path("/splash/billing_pick"))
            .and(body_string_contains("authenticity_token=FREE_TOKEN"))
            .and(body_string_contains("pricing_plan=free"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let ok = http_login(&format!("{base}/generate_204")).await;
        assert!(ok, "http_login should complete the mock portal flow");
        // MockServer's Drop verifies the POST expectation (exactly 1 hit with
        // the right token + plan), so a wrong token would fail the test.
    }

    // The driver (and GL.iNet's own UI) reject anything that isn't a
    // locally-administered unicast address, so the bit fiddling has to be right.
    #[test]
    fn generated_mac_is_locally_administered_unicast() {
        for _ in 0..20 {
            let mac = random_laa_mac().expect("should read /dev/urandom");
            let octets: Vec<u8> = mac
                .split(':')
                .map(|o| u8::from_str_radix(o, 16).expect("hex octet"))
                .collect();
            assert_eq!(octets.len(), 6, "six octets: {mac}");
            assert_eq!(
                octets[0] & 0x02,
                0x02,
                "locally administered bit set: {mac}"
            );
            assert_eq!(
                octets[0] & 0x01,
                0x00,
                "unicast (multicast bit clear): {mac}"
            );
        }
    }

    #[test]
    fn reads_mac_from_ip_br_link() {
        let line =
            "sta1             UP             02:6a:4c:3c:b1:79 <BROADCAST,MULTICAST,UP,LOWER_UP>";
        assert_eq!(parse_link_mac(line).as_deref(), Some("02:6a:4c:3c:b1:79"));
        assert_eq!(parse_link_mac("").as_deref(), None);
    }

    // The daemon reports the SSID nested in a `config` object; we want the first
    // occurrence and nothing else.
    #[test]
    fn reads_ssid_from_repeater_status() {
        let json = r#"{
            "state": 1,
            "state_s": "connected",
            "config": {
                "disguise": false,
                "ssid": "Starbucks Customer",
                "macaddr": { "mode": "random" }
            }
        }"#;
        assert_eq!(
            parse_repeater_ssid(json).as_deref(),
            Some("Starbucks Customer")
        );
    }

    // The daemon's state is what we wait on instead of timing the reconnect.
    #[test]
    fn reads_state_from_repeater_status() {
        let connecting = r#"{"state": 1, "state_s": "connecting", "running": true}"#;
        let connected = r#"{"state": 2, "state_s": "connected", "config": {}}"#;
        let failed = r#"{"state_s": "failed", "fail_type": "auth"}"#;
        assert_eq!(
            parse_repeater_state(connecting).as_deref(),
            Some("connecting")
        );
        assert_eq!(
            parse_repeater_state(connected).as_deref(),
            Some("connected")
        );
        assert_eq!(parse_repeater_state(failed).as_deref(), Some("failed"));
        assert_eq!(parse_repeater_state("{}"), None);
    }

    #[test]
    fn repeater_ssid_absent_or_empty_is_none() {
        assert_eq!(parse_repeater_ssid("{}"), None);
        assert_eq!(parse_repeater_ssid(r#"{"ssid": ""}"#), None);
    }

    #[test]
    fn classifies_204_as_online() {
        assert_eq!(
            classify_response("HTTP/1.1 204 No Content\r\n"),
            Reach::Online
        );
        assert_eq!(
            classify_response("HTTP/1.0 204 No Content\r\n"),
            Reach::Online
        );
    }

    // The distinction the whole guard rests on: a portal answers, a dead uplink
    // does not. Both mean "no internet", but only one is ours to act on.
    #[test]
    fn classifies_other_http_replies_as_intercepted() {
        assert_eq!(classify_response("HTTP/1.1 200 OK\r\n"), Reach::Intercepted);
        assert_eq!(
            classify_response("HTTP/1.1 302 Found\r\n"),
            Reach::Intercepted
        );
        assert_eq!(
            classify_response("HTTP/1.0 511 Network Authentication Required\r\n"),
            Reach::Intercepted
        );
    }

    #[test]
    fn classifies_non_http_as_down() {
        assert_eq!(classify_response(""), Reach::Down);
        assert_eq!(classify_response("\0\0garbage"), Reach::Down);
    }

    #[test]
    fn known_portal_matches_meraki_front_ends() {
        assert!(is_known_portal("n143.network-auth.com"));
        assert!(is_known_portal("n747.network-auth.com"));
        assert!(is_known_portal("network-auth.com"));
    }

    // A lookalike host must not be mistaken for ours, or we would cycle a
    // stranger's network on their say-so.
    #[test]
    fn known_portal_rejects_lookalikes() {
        assert!(!is_known_portal("network-auth.com.example.org"));
        assert!(!is_known_portal("evil-network-auth.com"));
        assert!(!is_known_portal("example.org"));
        assert!(!is_known_portal(""));
    }

    #[tokio::test]
    async fn portal_host_reports_who_intercepted_us() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let base = server.uri();

        Mock::given(method("GET"))
            .and(path("/generate_204"))
            .respond_with(
                ResponseTemplate::new(307).insert_header("location", format!("{base}/splash")),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/splash"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html/>"))
            .mount(&server)
            .await;

        // The host we report is the one that served the splash after redirects,
        // which is what identifies the portal.
        let host = portal_host(&format!("{base}/generate_204")).await;
        assert_eq!(host.as_deref(), Some("127.0.0.1"));
        assert!(!is_known_portal(&host.unwrap()));
    }

    // A real 204 means nobody intercepted us, so there is no portal to name.
    #[tokio::test]
    async fn portal_host_is_none_when_actually_online() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let base = server.uri();

        Mock::given(method("GET"))
            .and(path("/generate_204"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        assert_eq!(portal_host(&format!("{base}/generate_204")).await, None);
    }
}
