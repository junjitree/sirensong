use std::env;
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

/// Set once SIGTERM or Ctrl-C arrives.
///
/// Signals used to be observed only in `sleep_or_shutdown`, i.e. in the gaps
/// *between* watch ticks — nothing was listening during a reconcile. That is
/// fine when reconciles are quick, but a repeater rotation blocks for as long as
/// the association takes, which on a congested network is minutes. Ctrl-C was
/// ignored for the whole of it, and because tokio installs a handler the default
/// "Ctrl-C kills the process" behaviour is gone, so it looked hung. The blocking
/// loops poll this flag so they can abandon what they are doing instead.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// The Wi-Fi interface a hotspot was started on, if any.
///
/// The `HotspotGuard` `Drop` impl is what normally stops `create_ap`, but the
/// force-exit below calls `process::exit`, which runs no destructors — so the
/// escape hatch for an unresponsive first Ctrl-C would otherwise leave the radio
/// beaconing, which is the exact battery drain the guard exists to prevent. One
/// fix cancelling the other.
static HOTSPOT_IFACE: OnceLock<String> = OnceLock::new();

fn shutdown_requested() -> bool {
    SHUTDOWN.load(Ordering::Relaxed)
}

/// Watch for termination signals for the whole life of the process, so a signal
/// is recorded the moment it arrives rather than whenever we next happen to be
/// sitting in an await.
fn spawn_shutdown_listener() {
    tokio::spawn(async {
        let mut sigterm = signal(SignalKind::terminate()).ok();
        loop {
            match sigterm.as_mut() {
                Some(sig) => {
                    tokio::select! {
                        _ = sig.recv() => {}
                        _ = tokio::signal::ctrl_c() => {}
                    }
                }
                None => {
                    let _ = tokio::signal::ctrl_c().await;
                }
            }
            // Keep listening rather than returning after the first signal.
            // Installing a handler removes the kernel's default "Ctrl-C kills
            // it" disposition process-wide, so if we stopped here a second
            // Ctrl-C would do nothing at all and the only way out would be
            // SIGKILL. Any blocking stretch that does not poll the flag —
            // `nmcli connection up`, a `create_ap` start — would be
            // uninterruptible. The second signal restores the escape.
            if SHUTDOWN.swap(true, Ordering::Relaxed) {
                eprintln!("sirensong: second signal received — exiting immediately");
                if let Some(iface) = HOTSPOT_IFACE.get() {
                    eprintln!("sirensong: stopping the hotspot on {iface}");
                    let _ = Command::new("sudo")
                        .args(["create_ap", "--stop", iface])
                        .status();
                }
                std::process::exit(130);
            }
        }
    });
}

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
/// Poll interval before the first successful sign-in of a run.
///
/// The dominant use case is powering the travel router on while already sitting
/// in the cafe: it boots, the repeater daemon associates, and the portal starts
/// intercepting — and until we notice, nothing works. At the normal cadence that
/// wait is most of the downtime, far more than the sign-in itself. So poll hard
/// until we get online once, then relax.
const STARTUP_POLL: Duration = Duration::from_secs(5);
/// Cap on the fast phase, so a router that never gets online (wrong venue, out
/// of range) does not probe every 5s indefinitely. Only reached when signing in
/// never succeeds; the normal exit from the fast phase is success, not the clock.
const STARTUP_WINDOW: Duration = Duration::from_secs(180);
/// Registrable domain of the Cisco Meraki splash portals we can log into (hosts
/// look like `n143.network-auth.com`). This is how we recognize *our* portal
/// without ever asking which network we are on.
const PORTAL_HOST: &str = "network-auth.com";
/// Poll interval while waiting for a rotated repeater to come back. Success is
/// noticed on average half an interval late, so this is pure added latency on
/// every rotation; 1s halves that for one extra `ubus`/`ip` call per second,
/// which is nothing next to a ~31s rotation.
const ROTATE_POLL: Duration = Duration::from_secs(1);
/// Pause between stopping and restarting `create_ap`, giving the old hostapd and
/// dnsmasq time to release the interface. Unrelated to `ROTATE_POLL`, which it
/// used to borrow — tuning the poll rate should not change teardown behaviour.
const AP_RESTART_SETTLE: Duration = Duration::from_secs(2);
/// Give up after this long with *nothing* moving — no change in the daemon's
/// state, the live MAC, or the default route. A stall detector, not a duration
/// budget: a reconnect takes 30s on a quiet network and minutes on a busy café,
/// so we wait on evidence rather than guessing how long is reasonable.
///
/// Generous on purpose. A working rotation moves one of the three within ~30s
/// (MAC at 18s, association at 26s, lease at 30s), and a daemon cycling between
/// `connecting` and `failed` is itself movement — so five minutes frozen is not
/// a slow network, it is stuck. Erring long matters more than erring short:
/// giving up does not cancel the daemon's own reconnect, it only stops us
/// watching one that may be about to succeed.
const ROTATE_STALL_AFTER: Duration = Duration::from_secs(300);
/// Absolute backstop so a wedged daemon can't block the watch loop forever.
/// Should never be reached; the stall detector is the real mechanism.
///
/// This was 10 minutes and that was actively harmful. Giving up does not cancel
/// anything — `gl-repeater` keeps trying in the background — so a cap that
/// expires mid-association only stops us *watching* a rotation that then
/// succeeds without us. Observed on a congested café network: the cap fired at
/// 10:20, and the rotated MAC associated and reached the portal around 22
/// minutes in. Worse, reporting failure hands control back to the watch loop,
/// which can rotate again and restart a nearly-complete association from zero.
/// So the bound is now well past any association we have measured, and exists
/// only to stop an infinite block.
const ROTATE_GIVE_UP_AFTER: Duration = Duration::from_secs(3600);

/// Both limits above are wall-clock, derived from whatever `ROTATE_POLL` happens
/// to be. They used to be counts of polls, which meant changing the poll rate
/// silently rescaled every timeout with it — halving the interval would have
/// quietly turned the 60-minute backstop into 30.
fn polls_for(d: Duration) -> u32 {
    (d.as_millis() / ROTATE_POLL.as_millis().max(1)).max(1) as u32
}
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
    /// Explicitly supplied passphrase. `None` means "use the remembered one, or
    /// generate and remember a fresh one" — resolved at start rather than parse
    /// time so that argument parsing stays free of side effects.
    pass: Option<String>,
    /// `None` means follow the station's channel, resolved at start.
    channel: Option<u32>,
}

/// Trim an SSID to the 802.11 limit of 32 *bytes*, without splitting a character.
fn truncate_ssid(name: &str) -> String {
    if name.len() <= 32 {
        return name.to_string();
    }
    let mut end = 32;
    while end > 0 && !name.is_char_boundary(end) {
        end -= 1;
    }
    name[..end].to_string()
}

/// Default hotspot name: `<hostname>-sirensong`, so it's recognisable among the
/// dozen other networks in a café. Falls back to a bare `sirensong` if the
/// hostname can't be read.
fn default_hotspot_ssid() -> String {
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|h| h.trim().to_string())
        .unwrap_or_default();
    if host.is_empty() {
        "sirensong".to_string()
    } else {
        truncate_ssid(&format!("{host}-sirensong"))
    }
}

fn print_help() {
    println!(
        "sirensong - automate Starbucks Wi-Fi captive portal login\n\n\
         USAGE:\n    sirensong [OPTIONS] [SSID]\n\n\
         ARGS:\n    <SSID>    Wi-Fi network name (default: \"{DEFAULT_SSID}\")\n\n\
         OPTIONS:\n\
         \x20   -o, --once             Authenticate once and exit (default: watch and re-auth on drop)\n\
         \x20   -i, --interval <SECS>  Watch poll interval in seconds (default: 30)\n\
         \x20   -q, --quiet            Only log errors (overrides RUST_LOG)\n\
         \x20   -h, --help             Print this help\n\
         \x20   -V, --version          Print version\n\n\
         HOTSPOT (watch mode only, needs create_ap and root):\n\
         \x20       --hotspot              Share this connection over a Wi-Fi hotspot\n\
         \x20       --hotspot-ssid <NAME>  Network name (default: <hostname>-sirensong)\n\
         \x20       --hotspot-pass <PASS>  Passphrase (or set SIRENSONG_HOTSPOT_PASS;\n\
         \x20                              otherwise one is generated and remembered)\n\
         \x20       --hotspot-channel <N>  AP channel (default: same as the Wi-Fi station)\n\n\
         The hotspot is stopped when sirensong exits, so the radio doesn't keep\n\
         draining battery. A generated passphrase is saved to\n\
         ~/.config/sirensong/hotspot.pass so devices only pair once, and the\n\
         credentials print as a QR code you can scan to join.\n\
         Note that either way the passphrase reaches create_ap as an argument,\n\
         so it is visible to other local users via ps for as long as the AP runs.\n\n\
         Log verbosity is otherwise controlled by RUST_LOG (e.g. RUST_LOG=debug)."
    );
}

fn parse_args_from<I: Iterator<Item = String>>(args: I) -> Result<Config, String> {
    let mut ssid = None;
    let mut once = false;
    // How long a drop can go unnoticed, so it lands directly in the user's
    // downtime: worst case is a full interval plus the ~2s `confirmed_offline`
    // takes, and only then does the ~34s reauth start. At 60s this dominated
    // everything else — the wait to notice cost more than the rotation itself.
    let mut interval = Duration::from_secs(30);
    let mut quiet = false;
    let mut hotspot = false;
    let mut hotspot_ssid = None;
    let mut hotspot_pass = None;
    let mut hotspot_channel: Option<u32> = None;
    let mut args = args.peekable();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-o" | "--once" => once = true,
            "-q" | "--quiet" => quiet = true,
            "--hotspot" => hotspot = true,
            "--hotspot-ssid" => {
                hotspot_ssid = Some(
                    args.next()
                        .ok_or_else(|| "--hotspot-ssid requires a name".to_string())?,
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
                hotspot_channel = Some(
                    val.parse()
                        .map_err(|_| format!("invalid --hotspot-channel value: {val}"))?,
                );
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
            // Last-wins silently swallowed the common `sirensong Starbucks
            // Customer` (missing quotes), which then watched a network called
            // "Customer" and never explained why nothing worked.
            other if ssid.is_some() => {
                return Err(format!(
                    "unexpected extra argument: {other}. An SSID with spaces needs quoting, \
                     e.g. sirensong \"Starbucks Customer\""
                ));
            }
            other => ssid = Some(other.to_string()),
        }
    }

    if hotspot_ssid.is_some() && !hotspot {
        return Err("--hotspot-ssid does nothing without --hotspot".to_string());
    }

    // Both sources end up in create_ap's argv, so neither hides the passphrase
    // from `ps`. The env var only keeps it out of *our* command line and the
    // shell history — worth having, but not the protection it was described as.
    let hotspot = if !hotspot {
        None
    } else {
        let pass = hotspot_pass.or_else(|| env::var("SIRENSONG_HOTSPOT_PASS").ok());
        if pass.as_deref().is_some_and(|p| !valid_passphrase(p)) {
            return Err(
                "hotspot passphrase must be 8-63 characters with no newlines (WPA2)".to_string(),
            );
        }
        // Same limits create_ap enforces, checked here so the error names the
        // real problem instead of surfacing as a failed launch.
        let ssid = hotspot_ssid.unwrap_or_else(default_hotspot_ssid);
        let ssid_chars = ssid.chars().count();
        if !(1..=32).contains(&ssid_chars) {
            return Err(format!(
                "hotspot SSID must be 1-32 characters (got {ssid_chars})"
            ));
        }
        Some(Hotspot {
            ssid,
            pass,
            channel: hotspot_channel,
        })
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

    // Keep reading until we have the whole status line. `read` may return any
    // number of bytes, and a single call could hand back just `"HTTP"` or
    // `"HTTP/1.1 20"` — the first classifies as `Down` (a false offline, which
    // costs a needless rotation) and the second as `Intercepted` (a false
    // portal). Both are indistinguishable from the real thing downstream.
    let mut buf = [0u8; 64];
    let mut have = 0usize;
    loop {
        match stream.read(&mut buf[have..]) {
            Ok(0) => break,
            Ok(n) => {
                have += n;
                // Enough for "HTTP/1.1 204", or a full line if it is shorter.
                if have >= 12 || buf[..have].contains(&b'\n') || have == buf.len() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    if have == 0 {
        return Reach::Down;
    }
    classify_response(&String::from_utf8_lossy(&buf[..have]))
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

/// The SSID we are actually associated to, or `None` if we are not.
///
/// The `ssid` in this payload is nested under `config` — it is the network the
/// repeater is *configured* to join, and `ubus` reports it whether or not we
/// ever associated. Returning it unconditionally made `current_ssid()` claim we
/// were attached to something while the radio was disassociated, which is the
/// opposite of what its callers ask it. Gate it on the daemon's own state so it
/// means the same thing the NetworkManager side does.
fn repeater_ssid() -> Option<String> {
    let status = repeater_status()?;
    if parse_repeater_state(&status).as_deref() != Some("connected") {
        return None;
    }
    parse_repeater_ssid(&status)
}

fn repeater_state() -> Option<String> {
    parse_repeater_state(&repeater_status()?)
}

/// Device the default route currently goes out of, from `ip route show default`.
fn default_route_device() -> Option<String> {
    let out = Command::new("ip")
        .args(["route", "show", "default"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_default_route_device(&String::from_utf8_lossy(&out.stdout))
}

/// `default via 192.168.23.254 dev sta1 proto static ...` -> `sta1`.
fn parse_default_route_device(routes: &str) -> Option<String> {
    let line = routes.lines().next()?;
    let mut it = line.split_whitespace();
    while let Some(tok) = it.next() {
        if tok == "dev" {
            return it.next().map(str::to_string);
        }
    }
    None
}

/// Is the Wi-Fi station actually carrying our traffic?
///
/// The backend is chosen by `/etc/init.d/repeater` existing, and that ships in
/// every GL.iNet mode — the device reports `mode='router'` even while repeating.
/// So being on this backend says nothing about where the uplink is. Rotating the
/// station MAC when traffic goes out over ethernet or a tether restarts
/// `gl-repeater` for nothing, and then waits for a route via the station that
/// will never appear, burning the full stall budget before failing and retrying.
///
/// Checking the route beats detecting the mode: it is what actually matters, and
/// it stays correct if a cable is plugged in mid-session.
fn station_is_uplink() -> bool {
    match (uci_get("wireless.sta.ifname"), default_route_device()) {
        (Some(sta), Some(dev)) => sta == dev,
        // No default route yet: mid-reconnect, so assume the station is ours —
        // that is the normal state during the rotation we are about to do.
        (Some(_), None) => true,
        _ => false,
    }
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

/// Does the *station* have a default route yet?
///
/// Scoped to the station device on purpose. Matching any default route meant a
/// second one — a wired WAN into the GL.iNet, or a leftover — satisfied this
/// from the first poll, so a rotation reported success at ~18s, before the radio
/// had associated and long before DHCP. Everything downstream then ran against
/// an unassociated link and failed for reasons that looked like a portal fault.
/// Takes the device rather than re-reading it: the caller already resolved the
/// station name this poll, and the name flips between `sta0` and `sta1` across a
/// restart. Two independent reads could straddle that flip and match the new
/// interface's MAC against the *old* interface's route — reintroducing the early
/// false success the scoping exists to prevent.
fn default_route_present(dev: &str) -> bool {
    match Command::new("ip")
        .args(["route", "show", "default", "dev", dev])
        .output()
    {
        // A missing device exits non-zero with empty stdout, which is otherwise
        // indistinguishable from "associated but not routed yet" — and silently
        // waiting out the backstop on a stale interface name is a long way to
        // fail for something worth saying out loud.
        Ok(o) if !o.status.success() => {
            debug!(dev, "ip route failed; is the station interface name stale?");
            false
        }
        Ok(o) => !o.stdout.is_empty(),
        Err(_) => false,
    }
}

/// Rotate the repeater's MAC and wait for the link to come back on it.
///
/// Writes the address to both places the daemon reads, then restarts the daemon
/// so it recreates the station vdev with the new address. Returns once the
/// station is up on that MAC with a default route — typically ~30s.
fn rotate_repeater(cfg: &Config) -> bool {
    if !station_is_uplink() {
        warn!(
            "the Wi-Fi station isn't carrying traffic (uplink is {}), so rotating its \
             MAC would change nothing — leaving it alone",
            default_route_device().unwrap_or_else(|| "unknown".into())
        );
        return false;
    }
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

    let stall_polls = polls_for(ROTATE_STALL_AFTER);
    for poll in 0..polls_for(ROTATE_GIVE_UP_AFTER) {
        std::thread::sleep(ROTATE_POLL);

        // A rotation can legitimately run for many minutes; without this, Ctrl-C
        // is not acted on until it finishes. The daemon keeps reconnecting on
        // its own after we let go, which is what we want on the way out anyway.
        if shutdown_requested() {
            info!("stopping — leaving the reconnect to the repeater daemon");
            return false;
        }

        // Resolve the station name once and use it for both reads. The name
        // flips sta0<->sta1 across a restart, so reading it twice could match the
        // new interface's MAC against the old interface's route.
        let dev = uci_get("wireless.sta.ifname").unwrap_or_default();
        let state = repeater_state().unwrap_or_default();
        let live = link_mac(&dev).unwrap_or_default();
        let route = !dev.is_empty() && default_route_present(&dev);

        // Every poll, because the interesting question is which of these
        // actually moves while the daemon sits in `connecting` — that is the
        // phase where stall detection currently cannot tell a slow association
        // apart from a wedged one.
        debug!(
            poll = poll + 1,
            state = %state,
            live = %live,
            want = %want,
            route,
            stalled,
            "rotate poll"
        );

        if live == want && route {
            debug!(mac = %want, polls = poll + 1, "repeater back up on the rotated MAC");
            return true;
        }

        // Give up only on evidence of a stall, never on a clock, and treat the
        // fingerprint as the sole signal.
        //
        // A rotation climbs through stages rather than flipping once. Measured on
        // a live cafe network: the vdev is destroyed and recreated with the new
        // MAC at 18s (`live` empty -> ours), associates at 26s (`connecting` ->
        // `connected`), and gets a lease at 30s (`route`). All three belong in
        // the fingerprint, because each phase is one where the others are still.
        //
        // No state is exempt. Earlier versions suppressed the counter for the
        // states the daemon reports, which sounded principled and left the
        // detector unable to fire at all — `state_s` is only ever `idle`,
        // `connecting`, `connected` or `failed` (the live payload pins the
        // mapping: `"state": 2` renders `"connected"`), so exempting them all
        // meant only a silent `ubus` could ever trip it, and the real bound
        // became ROTATE_GIVE_UP_AFTER an hour later. `idle` in particular is
        // what a freshly restarted daemon reports while sweeping bands, so it
        // must not be treated as wedged on its own — but a genuinely stuck
        // daemon also reports a state, so the state alone decides nothing.
        //
        // What actually distinguishes the two is whether *anything* moves. Hence
        // a generous budget: a working rotation changes one of the three within
        // ~30s, and `failed` oscillating with `connecting` is itself movement, so
        // a frozen fingerprint for minutes really does mean stuck.
        let seen = format!("{state}|{live}|{route}");
        if seen != last {
            stalled = 0;
            last = seen;
        } else {
            stalled += 1;
            if stalled >= stall_polls {
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
        // Same policy `http_login` uses, so both agree on where a chain ends.
        .redirect(portal_redirect_policy())
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
/// Follow redirects only while they stay on the portal.
///
/// The chain's last hop is a bounce to the venue's own website (`continue_url`,
/// e.g. starbucks.ph) — pure navigation for a human's browser, measured at 2.3s
/// of a 4s login spent loading a marketing page we discard. Everything before it
/// is followed, so any confirmation hop Meraki needs still happens.
///
/// Shared by both clients that fetch the captive-detect URL. They used to differ:
/// `portal_host` took reqwest's default policy and followed the bounce out, so
/// on any chain ending off-portal it would report `www.starbucks.ph`,
/// `is_known_portal` would reject it, and reconcile would decline to act on our
/// own portal — while `http_login` saw a different final host for the same URL.
fn portal_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        // `error`, not `stop`: `stop` hands the 30x back as a normal response, so
        // a redirect loop would surface as a *successful* POST at the call site.
        // reqwest's default errors here, and losing that made a runaway chain
        // look like a successful login.
        if attempt.previous().len() >= 10 {
            return attempt.error("too many redirects");
        }
        let on_portal = |u: Option<&str>| u.map(is_known_portal).unwrap_or(false);
        // Stop only on the hop that *leaves* a portal: the detect URL's redirect
        // onto the splash is followed (we aren't on a portal yet), as is every
        // hop within it.
        let leaving_portal = attempt
            .previous()
            .last()
            .map(|u| on_portal(u.host_str()))
            .unwrap_or(false)
            && !on_portal(attempt.url().host_str());
        if leaving_portal {
            attempt.stop()
        } else {
            attempt.follow()
        }
    })
}

/// HTTP-only captive-portal login (the fast path). Mirrors what the browser
/// does on the Cisco Meraki splash: GET a captive-detect URL (redirects to the
/// splash, setting a session cookie), scrape the free form's Rails
/// `authenticity_token`, then POST it. No browser, no chromedriver.
///
/// Returns whether the POST was accepted; the caller confirms real
/// connectivity. Any parsing/network failure returns `false` (and logs a
/// `warn!` if the markup looks like it changed, since that needs a code fix).
/// Why a sign-in attempt ended the way it did.
///
/// `Unusable` matters: a Meraki portal running vouchers, SMS or click-through
/// terms is recognised by host but has no free-plan form, so there is nothing to
/// submit. Reporting that as a failure made the caller rotate and try again on a
/// fresh MAC — which cannot possibly help, and rotated on networks the user
/// never asked to rotate.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Login {
    Submitted,
    /// Recognised portal, but not one we can drive.
    Unusable,
    Failed,
}

async fn http_login(detect_url: &str) -> Login {
    let redirect_policy = portal_redirect_policy();
    let client = match reqwest::Client::builder()
        .cookie_store(true)
        .user_agent(BROWSER_UA)
        .timeout(Duration::from_secs(15))
        .redirect(redirect_policy)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            debug!(error = %e, "could not build HTTP client");
            return Login::Failed;
        }
    };

    // Follows the 307 to the splash page and picks up the session cookie.
    let resp = match client.get(detect_url).send().await {
        Ok(r) => r,
        Err(e) => {
            debug!(error = %e, "captive-detect request failed");
            return Login::Failed;
        }
    };
    if resp.status().as_u16() == 204 {
        return Login::Submitted; // already online
    }

    let final_url = resp.url().clone();
    let html = match resp.text().await {
        Ok(body) => html_unescape(&body),
        Err(e) => {
            debug!(error = %e, "could not read splash page body");
            return Login::Failed;
        }
    };

    let Some(form) = billing_pick_form(&html) else {
        warn!(
            "no Meraki free-plan form on this splash page — sirensong only handles the Meraki \
             free-plan portal, so this may be a different vendor (or Meraki markup that changed)"
        );
        // Not a failure: there is no form to submit, so a fresh MAC changes
        // nothing. Saying "failed" here made the caller rotate at venues it
        // fundamentally cannot sign into.
        return Login::Unusable;
    };

    // `[^>]*` rather than `\s+`: Rails' `hidden_field_tag` emits
    // `name="authenticity_token" id="authenticity_token" value="…"`, putting an
    // attribute between the two. Requiring them adjacent meant a splash rendered
    // that way failed to parse and reported "markup has likely changed" forever,
    // on markup that had not meaningfully changed. The `continue_url` pattern
    // below already allowed for it; the two disagreed for no reason.
    let Some(token) = capture(form, r#"name="authenticity_token"[^>]*value="([^"]+)""#) else {
        warn!(
            "authenticity_token missing from the free-plan form — the Meraki splash markup \
             has likely changed"
        );
        return Login::Unusable;
    };
    let continue_url =
        capture(form, r#"name="continue_url"[^>]*value="([^"]*)""#).unwrap_or_default();
    // Resolve a relative action against the splash URL rather than discarding it.
    // The old `.filter(starts_with("http"))` dropped `/splash/billing_pick?mauth=…`
    // — a shape Meraki does emit — and rebuilt a bare path, throwing away the
    // query the portal needs and inventing a host when there wasn't one.
    let post_url = match capture(form, r#"action="([^"]*billing_pick[^"]*)""#) {
        Some(action) => final_url
            .join(&action)
            .map(|u| u.to_string())
            .unwrap_or_else(|_| action),
        None => format!(
            "{}://{}/splash/billing_pick",
            final_url.scheme(),
            final_url.host_str().unwrap_or("network-auth.com")
        ),
    };

    debug!(url = %post_url, "submitting free-plan portal form");
    let params = [
        ("utf8", "✓"),
        ("authenticity_token", token.as_str()),
        ("pricing_plan", "free"),
        ("commit", "Continue"),
        ("continue_url", continue_url.as_str()),
    ];
    // Check the status. `Ok(_) => true` treated 401/404/500 — and a splash
    // re-served saying the free allowance is used up — as a successful login,
    // so "the portal rejected us" was indistinguishable from "the portal let us
    // in", and the only thing catching it was `wait_online` timing out with a
    // message blaming something else.
    match client.post(&post_url).form(&params).send().await {
        Ok(resp) if resp.status().is_success() || resp.status().is_redirection() => {
            Login::Submitted
        }
        Ok(resp) => {
            warn!(status = %resp.status(), url = %post_url, "the portal rejected the sign-in form");
            Login::Failed
        }
        Err(e) => {
            debug!(error = %e, "portal POST failed");
            Login::Failed
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

/// What a reconciliation pass actually did.
///
/// `Declined` exists because "this isn't ours to touch" was previously reported
/// the same way as "we tried and failed", and the watch loop backs off
/// exponentially on failure. On the router that is the *normal* state for the
/// whole time the repeater daemon is associating, so a cold boot in a cafe spent
/// its association window escalating the poll interval — reaching 15 minutes by
/// the time the portal was finally reachable, which is the one moment we are
/// needed. Only a real failure should slow us down.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Outcome {
    /// Online, either already or because we just signed in.
    Online,
    /// Deliberately did nothing: not our network, or someone else is mid-repair.
    Declined,
    /// Tried and failed.
    Failed,
}

/// One reconciliation pass: if already online, do nothing. Otherwise decide
/// whether this network is ours to act on, and only then reconnect — which
/// rolls a fresh MAC (see `connect_to_wifi`) so the portal treats us as a new
/// device — and authenticate over HTTP against the captive portal.
///
/// The decision is made by asking *who answered*, never by asking which network
/// we are on. A portal that identifies itself is ours to log into; silence means
/// the uplink is simply dead and we should keep our hands off.
async fn reconcile(cfg: &Config) -> Outcome {
    // Signing in is harmless anywhere we recognise the portal; rotating is not.
    let mut may_rotate = true;
    match probe().await {
        Reach::Online => {
            debug!("already online");
            return Outcome::Online;
        }
        Reach::Intercepted => {
            // Something answered on our behalf. Touch the connection only if it
            // is a portal we can actually log into — otherwise we are a guest on
            // somebody else's network and have no business cycling it.
            match portal_host(CAPTIVE_DETECT_URL).await {
                Some(host) if is_known_portal(&host) => {
                    debug!(host, "captive portal recognized");
                    // Only the configured network is ours to re-identify.
                    let attached = current_ssid();
                    if attached.as_deref() != Some(cfg.ssid.as_str()) {
                        debug!(
                            attached = attached.as_deref().unwrap_or("?"),
                            target = %cfg.ssid,
                            "not the configured network — will sign in but not rotate"
                        );
                        may_rotate = false;
                    }
                }
                Some(host) => {
                    debug!(
                        host,
                        "unrecognized captive portal; leaving this network alone"
                    );
                    return Outcome::Declined;
                }
                None => {
                    debug!("captive portal did not identify itself; leaving this network alone");
                    return Outcome::Declined;
                }
            }
        }
        Reach::Down => {
            // On the router, associating is the repeater daemon's job, not ours,
            // and it retries by itself. Rotating here would restart a reconnect
            // that is already under way and buy nothing — there is no portal
            // answering to log into, which is the only thing we add. Explicit
            // rather than relying on the guard below, which used to reach the
            // same outcome by accident because `repeater_ssid` reported the
            // configured SSID even while disassociated.
            if matches!(Backend::detect(), Backend::GlRepeater) {
                debug!("offline with no portal answering; the repeater daemon owns reconnecting");
                return Outcome::Declined;
            }
            // Nothing answered at all. If we are associated to something, its
            // uplink is dead rather than gated — the case where a home outage
            // used to send us hunting for the café AP. Leave it alone.
            if current_ssid().is_some() {
                debug!("offline with no portal answering; leaving current network alone");
                return Outcome::Declined;
            }
            // Associated to nothing, so there is no connection to disrupt. Still
            // skip the join if the target is not even in range.
            if !ssid_in_range(&cfg.ssid) {
                debug!(ssid = %cfg.ssid, "not associated and target SSID not in range");
                return Outcome::Declined;
            }
        }
    }

    // Try signing in on the MAC we already have, before spending a rotation.
    //
    // Rotating costs ~31s of downtime and burns a MAC against a per-device cap,
    // and it is only *needed* when the portal refuses us. Arriving at a cafe, or
    // powering the router on there, the current MAC usually has quota — the cap
    // resets between visits — so the whole re-auth is a 4s form POST. Ordering it
    // this way is strictly better: ~4s wasted when the portal does refuse,
    // ~31s and a MAC saved every time it doesn't.
    debug!("trying the portal on the current MAC before rotating");
    match http_login(CAPTIVE_DETECT_URL).await {
        Login::Submitted if wait_online().await => {
            debug!(method = "http", "authenticated without rotating");
            return Outcome::Online;
        }
        // Recognised portal we cannot drive (vouchers, SMS, click-through). A
        // fresh MAC lands on the same unusable form, so stop here rather than
        // rotating a network the user never asked us to rotate.
        Login::Unusable => {
            debug!("this portal has no free-plan form; nothing to do here");
            return Outcome::Declined;
        }
        _ => {}
    }

    // Rotation is a target-network behaviour. Signing in anywhere we recognise
    // the portal is welcome — it is read-only — but changing the radio's
    // identity is not something to do on somebody else's network just because
    // their portal turned us down.
    if !may_rotate {
        debug!(
            attached = current_ssid().as_deref().unwrap_or("?"),
            target = %cfg.ssid,
            "portal refused us, but this isn't the configured network — not rotating"
        );
        return Outcome::Declined;
    }

    // Refused, so the cap has almost certainly been reached on this MAC. Roll a
    // fresh one and try again — this is the mechanism the whole program exists
    // for, just no longer the first resort.
    debug!("portal would not take the current MAC; rotating");
    if !connect_to_wifi(cfg) {
        if shutdown_requested() {
            debug!("rotation interrupted");
            return Outcome::Declined;
        }
        error!("couldn't join {}", cfg.ssid);
        return Outcome::Failed;
    }

    // A fresh association occasionally restores connectivity on its own
    // (e.g. an open network with no portal); skip auth if so.
    if is_online().await {
        debug!("online after associating");
        return Outcome::Online;
    }

    debug!("authenticating over HTTP");
    match http_login(CAPTIVE_DETECT_URL).await {
        Login::Submitted if wait_online().await => {
            debug!(method = "http", "authenticated");
            Outcome::Online
        }
        Login::Unusable => {
            debug!("this portal has no free-plan form; nothing to do here");
            Outcome::Declined
        }
        _ => {
            error!("could not sign in to the Wi-Fi portal");
            Outcome::Failed
        }
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
/// Wi-Fi channel for a frequency in MHz, per the 2.4GHz and 5GHz numbering.
fn channel_for_freq(mhz: u32) -> Option<u32> {
    match mhz {
        2484 => Some(14),
        2412..=2472 => Some((mhz - 2407) / 5),
        5000..=5895 => Some((mhz - 5000) / 5),
        _ => None,
    }
}

/// The channel the Wi-Fi station is currently associated on.
///
/// The hotspot defaults to this rather than a fixed channel. Cards commonly
/// advertise two interface combinations — a permissive one capped at a single
/// channel, and a narrow one allowing two — so putting the AP anywhere other
/// than the station's channel quietly demands the narrow combination and often
/// just fails. Asking the user to look up their channel and pass it is the tool
/// refusing to read something it can see.
fn station_channel(dev: &str) -> Option<u32> {
    station_freq(dev).and_then(channel_for_freq)
}

/// The frequency (MHz) the Wi-Fi station is currently associated on.
///
/// Frequency rather than channel is the honest unit here: channel numbers repeat
/// across bands, so `161` alone cannot say whether it means 5805 MHz or 6755 MHz.
fn station_freq(dev: &str) -> Option<u32> {
    let out = Command::new("iw")
        .args(["dev", dev, "link"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_link_freq(&String::from_utf8_lossy(&out.stdout))
}

/// `\tfreq: 5805.0` -> `5805`.
fn parse_link_freq(link: &str) -> Option<u32> {
    let line = link.lines().find(|l| l.trim_start().starts_with("freq:"))?;
    let raw = line.split(':').nth(1)?.trim();
    raw.split('.').next()?.trim().parse().ok()
}

/// Is this interface actually carrying traffic?
///
/// Reads `operstate` rather than scanning `ip -br link show` for "UP", which was
/// the old test and matched interfaces that are plainly down: a stopped device
/// prints `DOWN … <NO-CARRIER,BROADCAST,MULTICAST,UP>`, and `LOWER_UP` matches
/// too. Startup therefore declared success on an AP that never began beaconing,
/// while the watch loop's own check used `operstate` and disagreed — so the two
/// fought, restarting the hotspot on every tick forever.
fn iface_is_up(dev: &str) -> bool {
    std::fs::read_to_string(format!("/sys/class/net/{dev}/operstate"))
        .map(|s| {
            let s = s.trim();
            s == "up" || s == "unknown"
        })
        .unwrap_or(false)
}

fn ap_is_up() -> bool {
    ap_interface().map(|dev| iface_is_up(&dev)).unwrap_or(false)
}

/// Country code from an `iw reg get` dump. `00` is the world domain, under which
/// most channels are `no IR` — transmission forbidden.
fn parse_regdom_country(iw_reg: &str) -> Option<String> {
    iw_reg
        .lines()
        .find_map(|l| l.trim().strip_prefix("country "))
        .and_then(|rest| rest.split(':').next())
        .map(|c| c.trim().to_string())
}

/// Warn before we even try if the regulatory domain forbids beaconing. Without
/// this the failure surfaces 25 seconds later as hostapd's opaque "could not
/// determine operating frequency", which is a genuinely hard thing to diagnose.
fn warn_if_regdom_blocks_ap() {
    let Ok(out) = Command::new("iw").args(["reg", "get"]).output() else {
        return;
    };
    let country = parse_regdom_country(&String::from_utf8_lossy(&out.stdout));
    if country.as_deref() == Some("00") {
        warn!(
            "regulatory domain is `country 00` (world), which forbids transmitting on \
             most channels — the hotspot will probably fail to start. Set your country, \
             e.g. `sudo iw reg set PH`, and persist it in /etc/modprobe.d/cfg80211.conf \
             as `options cfg80211 ieee80211_regdom=PH`"
        );
    }
}

/// Stops the hotspot when dropped, so it doesn't outlive sirensong and sit
/// there draining battery. Covers clean exit, Ctrl-C and panics; nothing can
/// cover SIGKILL.
///
/// The force-exit on a second signal does *not* run this — `process::exit` skips
/// destructors — so that path stops the AP itself via `HOTSPOT_IFACE`. Keep the
/// two in step: an escape hatch that leaks a beaconing radio is worse than the
/// hang it escapes.
///
/// Also carries what's needed to bring the AP back if it dies mid-session —
/// otherwise sirensong would keep cheerfully watching the portal while the
/// phone behind it has no network and nothing says so.
struct HotspotGuard {
    /// The radio, as `create_ap --stop` wants it (e.g. `wlp2s0`).
    wifi_iface: String,
    /// The virtual AP interface it created (e.g. `ap0`), for health checks.
    ap_iface: String,
    ssid: String,
    pass: String,
    channel: u32,
}

impl HotspotGuard {
    /// Cheap liveness check — a single sysfs read, no process spawn and no
    /// output parsing, so it's fine to run on every watch tick.
    fn is_up(&self) -> bool {
        // Same test `ap_is_up` uses at startup — they must agree, or startup
        // declares success on an AP the watch loop then tries to restart forever.
        iface_is_up(&self.ap_iface)
    }

    /// Tear down whatever is left and start the AP again.
    fn restart(&self) -> bool {
        let _ = Command::new("sudo")
            .args(["create_ap", "--stop", &self.wifi_iface])
            .output();
        std::thread::sleep(AP_RESTART_SETTLE);
        let launched = Command::new("sudo")
            .args([
                "create_ap",
                "--daemon",
                "-c",
                &self.channel.to_string(),
                // `--` first: create_ap parses with GNU getopt, which permutes,
                // so options are recognised *after* the positionals too. Without
                // it an SSID or passphrase beginning with `-` is read as a flag
                // (`--hidden`, `--mkconfig <file>`), silently changing what gets
                // started or writing a file as root.
                "--",
                &self.wifi_iface,
                &self.wifi_iface,
                &self.ssid,
                &self.pass,
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !launched {
            return false;
        }
        for _ in 0..AP_START_POLLS {
            std::thread::sleep(AP_START_POLL);
            if self.is_up() {
                return true;
            }
            if shutdown_requested() {
                return false;
            }
        }
        false
    }
}

impl Drop for HotspotGuard {
    fn drop(&mut self) {
        info!("stopping hotspot");
        let stopped = Command::new("sudo")
            .args(["create_ap", "--stop", &self.wifi_iface])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !stopped {
            warn!(
                iface = %self.wifi_iface,
                "could not stop the hotspot; check with: sudo create_ap --stop {}",
                self.wifi_iface
            );
        }
    }
}

/// Where a generated passphrase is remembered, so the phone pairs once and
/// reconnects on its own from then on.
fn hotspot_pass_path() -> Option<std::path::PathBuf> {
    let home = env::var("HOME").ok()?;
    Some(std::path::PathBuf::from(home).join(".config/sirensong/hotspot.pass"))
}

/// A random passphrase drawn from an alphabet with the ambiguous glyphs removed
/// (no `0`/`O`, no `1`/`l`/`I`), so it survives being read off a screen. 20
/// characters of this is ~115 bits, well beyond anything WPA2 needs.
fn generate_passphrase() -> Option<String> {
    use std::io::Read;
    const ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyzACDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut buf = [0u8; 20];
    std::fs::File::open("/dev/urandom")
        .ok()?
        .read_exact(&mut buf)
        .ok()?;
    Some(
        buf.iter()
            .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
            .collect(),
    )
}

/// Is this usable as a WPA2 passphrase?
///
/// `create_ap` enforces 8..=63 *characters* (bash `${#var}`) and writes the value
/// unquoted into hostapd's config, so a newline injects arbitrary directives. We
/// used to check `len() >= 8` — bytes, no upper bound, no content check — and
/// only on the flag/env path, so a bad value reached create_ap, was rejected
/// there, and surfaced as our misleading "is it installed, and does sudo work?".
fn valid_passphrase(p: &str) -> bool {
    let chars = p.chars().count();
    (8..=63).contains(&chars) && !p.contains(['\n', '\r'])
}

/// Resolve the passphrase: an explicit one wins, else reuse what we generated
/// last time, else generate one and remember it (owner-readable only).
fn resolve_passphrase(explicit: Option<&String>) -> Option<String> {
    // Say which source won. Without this, a stale SIRENSONG_HOTSPOT_PASS in the
    // shell silently overrides the saved passphrase and looks like a bug.
    if let Some(p) = explicit {
        let from = if env::var("SIRENSONG_HOTSPOT_PASS").as_ref() == Ok(p) {
            "SIRENSONG_HOTSPOT_PASS"
        } else {
            "--hotspot-pass"
        };
        info!("using the passphrase from {from}");
        return Some(p.clone());
    }
    let path = hotspot_pass_path()?;

    if let Ok(stored) = std::fs::read_to_string(&path) {
        let stored = stored.trim().to_string();
        if valid_passphrase(&stored) {
            info!(path = %path.display(), "using the remembered passphrase");
            return Some(stored);
        }
        if !stored.is_empty() {
            warn!(
                path = %path.display(),
                "the saved passphrase isn't usable (needs 8-63 characters, no newlines); \
                 generating a new one"
            );
        }
    }

    let generated = generate_passphrase()?;
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Create with 0600 rather than writing then chmod-ing. `fs::write` creates
    // with 0666 & ~umask — measured 0644 on a default umask — so the passphrase
    // sat world-readable for the window between the two calls, and any file left
    // at 0644 by an earlier run stayed that way forever.
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let opened = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path);
    match opened.and_then(|mut f| f.write_all(format!("{generated}\n").as_bytes())) {
        Ok(()) => {
            // `mode` only applies when *creating*, so repair an existing file.
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            info!(path = %path.display(), "generated a hotspot passphrase and saved it");
        }
        Err(e) => warn!(error = %e, "could not save the passphrase; it will differ next run"),
    }
    Some(generated)
}

/// Escape the delimiters that matter in a `WIFI:` provisioning URI.
fn escape_wifi_uri(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | ';' | ',' | ':' | '"') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Print the credentials plus a QR code, so joining is "point the camera at it"
/// rather than typing 20 random characters on a phone keyboard.
fn print_join_details(ssid: &str, pass: &str) {
    use qrcode::QrCode;
    use qrcode::render::unicode;

    let uri = format!(
        "WIFI:S:{};T:WPA;P:{};;",
        escape_wifi_uri(ssid),
        escape_wifi_uri(pass)
    );
    println!("\n  network:  {ssid}\n  password: {pass}\n");
    match QrCode::new(&uri) {
        Ok(code) => {
            let art = code
                .render::<unicode::Dense1x2>()
                .quiet_zone(true)
                .module_dimensions(1, 1)
                .build();
            println!("{art}");
            println!("  scan with your phone's camera to join\n");
        }
        Err(e) => debug!(error = %e, "could not render the QR code"),
    }
}

/// Frequencies (MHz) this radio may *initiate* radiation on, i.e. host an AP on.
///
/// A frequency flagged `no IR` can be associated to as a client but never
/// beaconed on. Under some regulatory domains that covers the entire 5GHz band,
/// so a card happily connected on 5GHz cannot host a hotspot there at all — and
/// cannot host one on 2.4GHz either, because that puts the AP on a second
/// channel, which most cards only allow in a restricted mode that fails to start.
///
/// Frequencies, not channel numbers: channel numbering repeats across bands. On
/// a Wi-Fi 6E card, channel 161 is both 5805 MHz (`no IR` here) and 6755 MHz
/// (usable) — collecting bare channel numbers made an unusable station channel
/// look fine, and this check silently did nothing.
fn ap_capable_freqs() -> Vec<u32> {
    let Ok(out) = Command::new("iw").arg("phy").output() else {
        return Vec::new();
    };
    parse_ap_capable_freqs(&String::from_utf8_lossy(&out.stdout))
}

/// Frequencies from `iw phy` output, excluding anything marked `no IR`,
/// `disabled` or requiring radar detection.
fn parse_ap_capable_freqs(iw_phy: &str) -> Vec<u32> {
    let mut out = Vec::new();
    for line in iw_phy.lines() {
        let t = line.trim();
        if !t.starts_with('*') || !t.contains("MHz [") {
            continue;
        }
        if t.contains("no IR") || t.contains("disabled") || t.contains("radar detection") {
            continue;
        }
        // "* 2412.0 MHz [1] (20.0 dBm)" -> 2412
        if let Some(mhz) = t
            .trim_start_matches('*')
            .split_whitespace()
            .next()
            .and_then(|f| f.split('.').next())
            .and_then(|f| f.parse::<u32>().ok())
        {
            out.push(mhz);
        }
    }
    out
}

/// If the station sits on a band we cannot beacon on, move it to one we can.
///
/// Only ever called for `--hotspot`, and only when the alternative is no hotspot
/// at all. It costs the uplink's 5GHz throughput, so it says so rather than
/// quietly downgrading the connection. The change is `--temporary`: in memory
/// only, forgotten when NetworkManager restarts, so the saved profile is left
/// alone.
fn pin_to_ap_capable_band(iface: &str, ssid: &str) -> bool {
    let capable = ap_capable_freqs();
    if capable.is_empty() {
        debug!("could not read which frequencies may host an AP; leaving the link alone");
        return false;
    }
    if let Some(freq) = station_freq(iface)
        && capable.contains(&freq)
    {
        return true; // already somewhere we can beacon
    }
    // 2.4GHz is where the usable frequencies almost always are when 5GHz is no-IR.
    if !capable.iter().any(|f| *f < 2500) {
        warn!("no frequency on this radio may host an AP; the hotspot cannot start");
        return false;
    }

    warn!(
        "the Wi-Fi is on a frequency this adapter may not transmit on, so a hotspot \
         cannot run alongside it — switching the connection to 2.4GHz. This trades \
         the link's speed for the hotspot; the change is temporary and is forgotten \
         when NetworkManager restarts"
    );
    let modified = Command::new("nmcli")
        .args([
            "connection",
            "modify",
            "--temporary",
            ssid,
            "802-11-wireless.band",
            "bg",
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !modified {
        warn!("could not pin the connection to 2.4GHz; leaving it alone");
        return false;
    }
    let _ = Command::new("nmcli")
        .args(["connection", "down", ssid])
        .output();
    let _ = Command::new("nmcli")
        .args(["connection", "up", ssid])
        .output();

    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(500));
        if shutdown_requested() {
            return false;
        }
        if let Some(freq) = station_freq(iface)
            && capable.contains(&freq)
        {
            info!(
                freq,
                channel = channel_for_freq(freq).unwrap_or(0),
                "reconnected on a frequency that can host the hotspot"
            );
            return true;
        }
    }
    warn!("the connection did not come back on a frequency that can host an AP");
    false
}

/// Bring up the hotspot on the same radio as the client connection. Returns a
/// guard that tears it down on drop; `None` if it could not be started.
fn hotspot_start(hs: &Hotspot, ssid: &str) -> Option<HotspotGuard> {
    let iface = wifi_device()?;
    // A 5GHz-only-no-IR radio cannot host an AP at all; move the link first.
    pin_to_ap_capable_band(&iface, ssid);
    let pass = resolve_passphrase(hs.pass.as_ref())?;
    warn_if_regdom_blocks_ap();

    // Follow the station unless told otherwise: an AP on a different channel
    // needs the card's narrow interface combination and frequently just fails.
    let station = station_channel(&iface);
    let channel = match hs.channel {
        Some(c) => c,
        None => station.unwrap_or(1),
    };
    if let (Some(explicit), Some(sta)) = (hs.channel, station)
        && explicit != sta
    {
        warn!(
            requested = explicit,
            station = sta,
            "the hotspot channel differs from the one the Wi-Fi station is on; \
             most cards only allow that in a restricted mode and it often fails \
             to start — omit --hotspot-channel to follow the station"
        );
    }
    info!(
        ssid = %hs.ssid,
        channel,
        "starting hotspot on {}", iface
    );

    let status = Command::new("sudo")
        .args([
            "create_ap",
            "--daemon",
            "-c",
            &channel.to_string(),
            // See the note in `restart` — create_ap's getopt permutes, so the
            // positionals must be fenced off from option parsing.
            "--",
            &iface,
            &iface,
            &hs.ssid,
            &pass,
        ])
        .status();
    if !matches!(status, Ok(s) if s.success()) {
        error!("could not launch create_ap (is it installed, and does sudo work here?)");
        return None;
    }

    // Stop-on-drop from here on, so a half-started AP is cleaned up even if the
    // wait below gives up. The AP interface name is discovered rather than
    // assumed, then remembered so health checks are a plain sysfs read.
    let mut guard = HotspotGuard {
        wifi_iface: iface.clone(),
        ap_iface: String::new(),
        ssid: hs.ssid.clone(),
        pass: pass.clone(),
        channel,
    };
    // Reachable from the signal handler, which cannot run destructors.
    let _ = HOTSPOT_IFACE.set(iface.clone());

    // Wait for it to actually beacon rather than assuming a duration — the
    // interface can appear seconds before hostapd finishes, or never come up at
    // all if the regulatory domain blocks the channel.
    for _ in 0..AP_START_POLLS {
        std::thread::sleep(AP_START_POLL);
        if ap_is_up() {
            guard.ap_iface = ap_interface().unwrap_or_else(|| "ap0".to_string());
            info!(ssid = %hs.ssid, iface = %guard.ap_iface, "hotspot is up");
            print_join_details(&hs.ssid, &pass);
            return Some(guard);
        }
        if shutdown_requested() {
            info!("stopping before the hotspot finished starting");
            return None; // guard drops here, tearing down the half-started AP
        }
    }

    // Name the cause that actually applies. Blaming the regulatory domain
    // unconditionally sent a user hunting through `iw reg get` while their
    // domain was set correctly and the real problem was a channel the card
    // would not host alongside the station.
    match (station, hs.channel) {
        (Some(sta), Some(explicit)) if explicit != sta => error!(
            "hotspot interface never came up. The station is on channel {sta} but the \
             hotspot was asked for {explicit}; most cards will not host an AP on a \
             second channel. Drop --hotspot-channel to follow the station"
        ),
        _ => error!(
            "hotspot interface never came up. Most often the regulatory domain forbids \
             beaconing — check `iw reg get`; if it says `country 00`, set your country \
             (e.g. `sudo iw reg set PH`) and retry"
        ),
    }
    None // guard drops here, cleaning up the half-started AP
}

/// Exponential backoff for watch mode: `interval * 2^(fails-1)`, capped.
/// Poll interval to use, given how long we have been running and whether we have
/// ever been online this run.
///
/// Deliberately keyed on "have we succeeded yet" rather than only on elapsed
/// time: the fast phase exists to cover the gap between powering on and the
/// portal becoming reachable, and that gap ends when we get in, whenever that
/// is. The window is only a backstop so a router that never connects settles
/// down instead of probing forever.
fn poll_cadence(base: Duration, elapsed: Duration, been_online: bool) -> Duration {
    if been_online || elapsed >= STARTUP_WINDOW {
        base
    } else {
        base.min(STARTUP_POLL)
    }
}

fn backoff_delay(base: Duration, fails: u32) -> Duration {
    let shift = fails.saturating_sub(1).min(16);
    let secs = base.as_secs().saturating_mul(1u64 << shift);
    Duration::from_secs(secs).min(MAX_BACKOFF)
}

/// Sleep for `dur`, but wake early and return `true` if a shutdown signal
/// (SIGTERM / Ctrl-C) arrives — so `systemctl stop` exits promptly.
async fn sleep_or_shutdown(dur: Duration, sigterm: Option<&mut Signal>) -> bool {
    // A signal that landed while we were busy reconciling is already recorded;
    // don't sleep out the interval before acting on it.
    if shutdown_requested() {
        return true;
    }
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

async fn run_watch(cfg: &Config, hotspot: Option<&HotspotGuard>) {
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
    // Drives the startup cadence: poll hard until the first sign-in lands.
    let started = std::time::Instant::now();
    let mut been_online = false;
    loop {
        // A hotspot that quietly died leaves devices behind it with no network
        // and nothing to tell them why, so check it every tick and bring it back.
        if let Some(ap) = hotspot.filter(|ap| !ap.is_up()) {
            warn!(iface = %ap.ap_iface, "hotspot went down — restarting it");
            if ap.restart() {
                info!(ssid = %ap.ssid, "hotspot is back up");
            } else {
                error!("could not bring the hotspot back; devices behind it have no network");
            }
        }

        // `||` short-circuits: reconcile only runs after we've confirmed we're
        // actually offline (several failed probes), not on a single blip.
        let online = !confirmed_offline().await;
        if !online && online_announced {
            info!("connection dropped — signing back in");
            online_announced = false;
        }

        let outcome = if online {
            Outcome::Online
        } else {
            reconcile(cfg).await
        };

        let delay = match outcome {
            Outcome::Online => {
                if !online_announced {
                    info!("you're good to browse — still watching");
                    online_announced = true;
                }
                consecutive_failures = 0;
                been_online = true;
                cfg.interval
            }
            // Declining is not failing. Backing off here is what made a cold
            // boot slow: every tick spent waiting for the repeater daemon to
            // associate would have doubled the interval, so by the time the
            // portal was reachable we might not look for another 15 minutes.
            // Keep checking at the normal cadence instead.
            Outcome::Declined => {
                debug!("nothing to do on this network right now");
                poll_cadence(cfg.interval, started.elapsed(), been_online)
            }
            Outcome::Failed => {
                consecutive_failures += 1;
                let base = poll_cadence(cfg.interval, started.elapsed(), been_online);
                let backoff = backoff_delay(base, consecutive_failures);
                debug!(failures = consecutive_failures, "reconcile failed");
                warn!(
                    "couldn't get you online — retrying in {}s",
                    backoff.as_secs()
                );
                backoff
            }
        };

        if sleep_or_shutdown(delay, sigterm.as_mut()).await {
            info!("stopping — no longer watching");
            break;
        }
    }
}

#[tokio::main]
async fn main() {
    // `args_os`, not `args`: the latter panics on non-Unicode arguments, and an
    // SSID is a byte string — a latin-1 café name crashed with a backtrace
    // instead of an error.
    let argv = env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned());
    let cfg = match parse_args_from(argv) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("error: {e}\n");
            print_help();
            std::process::exit(2);
        }
    };

    init_logging(cfg.quiet);
    // Before any long-running work, so a signal during the very first reconcile
    // is recorded rather than missed. `--once` benefits too: its single pass can
    // include a multi-minute rotation.
    spawn_shutdown_listener();

    if cfg.once {
        let authenticated = reconcile(&cfg).await == Outcome::Online;
        // Success wins over a signal that arrived on the way out: we did the job,
        // so say so. Checking shutdown first reported 130 for a run that had
        // already authenticated, which is a lie to whatever reads the code.
        if authenticated {
            info!("you're good to browse");
            std::process::exit(0);
        }
        // A single pass can block for minutes on a rotation, so it may well have
        // been interrupted. Report that as 130 rather than as a failed login —
        // a supervisor should not treat "the operator stopped it" as "the portal
        // rejected us".
        if shutdown_requested() {
            info!("stopping — interrupted before finishing");
            std::process::exit(130);
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
            Backend::NetworkManager => match hotspot_start(hs, &cfg.ssid) {
                Some(guard) => Some(guard),
                None => {
                    error!("could not start the hotspot; continuing without it");
                    None
                }
            },
        },
    };

    run_watch(&cfg, _hotspot.as_ref()).await;
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
        assert_eq!(cfg.interval, Duration::from_secs(30));
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

    /// `sirensong Starbucks Customer` (missing quotes) used to silently watch a
    /// network called "Customer" — the default SSID has a space, so this is the
    /// easy mistake to make.
    /// The fast phase exists for "power the router on while already in the
    /// cafe": it should end when we actually get online, not on a timer, with
    /// the window only as a backstop for a router that never connects.
    #[test]
    fn startup_polls_fast_until_first_success() {
        let base = Duration::from_secs(30);
        let fast = Duration::from_secs(5);

        // Fresh start, never online: poll hard.
        assert_eq!(poll_cadence(base, Duration::ZERO, false), fast);
        assert_eq!(poll_cadence(base, Duration::from_secs(60), false), fast);

        // Success ends it immediately, however early.
        assert_eq!(poll_cadence(base, Duration::from_secs(1), true), base);

        // Never got online: settle down once the window elapses.
        assert_eq!(poll_cadence(base, STARTUP_WINDOW, false), base);
        assert_eq!(poll_cadence(base, Duration::from_secs(600), false), base);

        // An explicitly fast --interval is never slowed down by this.
        let user_fast = Duration::from_secs(2);
        assert_eq!(poll_cadence(user_fast, Duration::ZERO, false), user_fast);
    }

    /// Real `iw phy` output from a Wi-Fi 6E laptop. Every 5GHz frequency is
    /// `no IR`, so only 2.4GHz can host an AP — but channel *numbers* repeat
    /// across bands: 161 is both 5805 MHz (unusable) and 6755 MHz (usable).
    /// Matching on channel number made an unusable station look fine, and the
    /// band check silently did nothing.
    #[test]
    fn ap_capable_freqs_exclude_no_ir_and_do_not_confuse_bands() {
        let iw = "\t\t\t* 2412.0 MHz [1] (20.0 dBm)\n\
                  \t\t\t* 2437.0 MHz [6] (20.0 dBm)\n\
                  \t\t\t* 5180.0 MHz [36] (20.0 dBm) (no IR)\n\
                  \t\t\t* 5805.0 MHz [161] (20.0 dBm) (no IR)\n\
                  \t\t\t* 6755.0 MHz [161] (30.0 dBm)\n\
                  \t\t\t* 5260.0 MHz [52] (20.0 dBm) (radar detection)\n\
                  \t\t\t* 5320.0 MHz [64] (disabled)\n";
        let f = parse_ap_capable_freqs(iw);
        assert_eq!(f, vec![2412, 2437, 6755]);
        // The station's 5805 must NOT be considered capable just because a
        // different band also numbers a channel 161.
        assert!(!f.contains(&5805), "5GHz ch161 is no IR");
        assert!(
            f.contains(&6755),
            "6GHz ch161 is fine, and is a different freq"
        );
        assert!(!f.contains(&5260), "radar channels need DFS");
        assert!(!f.contains(&5320));
    }

    #[test]
    fn maps_frequencies_to_channels() {
        // The station this was written against.
        assert_eq!(channel_for_freq(5805), Some(161));
        assert_eq!(channel_for_freq(2412), Some(1));
        assert_eq!(channel_for_freq(2437), Some(6));
        assert_eq!(channel_for_freq(2462), Some(11));
        assert_eq!(channel_for_freq(2484), Some(14)); // the odd one out
        assert_eq!(channel_for_freq(5180), Some(36));
        assert_eq!(channel_for_freq(1000), None);
    }

    #[test]
    fn reads_freq_from_iw_link() {
        // Real `iw dev wlp2s0 link` output, including the trailing `.0`.
        let link = "Connected to e4:55:a8:b3:63:8d (on wlp2s0)\n\tSSID: Starbucks Customer\n\tfreq: 5805.0\n\tsignal: -61 dBm\n";
        assert_eq!(parse_link_freq(link), Some(5805));
        assert_eq!(parse_link_freq("Not connected.\n"), None);
        assert_eq!(parse_link_freq(""), None);
    }

    #[test]
    fn parses_the_default_route_device() {
        // Real output from the router while repeating.
        let r = "default via 192.168.23.254 dev sta1 proto static src 192.168.23.120 metric 20\n";
        assert_eq!(parse_default_route_device(r).as_deref(), Some("sta1"));
        // Ethernet uplink: the station is not carrying traffic.
        let eth = "default via 10.0.0.1 dev eth0 proto dhcp src 10.0.0.5 metric 10\n";
        assert_eq!(parse_default_route_device(eth).as_deref(), Some("eth0"));
        // Several routes: the first (lowest metric) is the one in use.
        let multi =
            "default via 10.0.0.1 dev eth0 metric 10\ndefault via 192.168.8.1 dev sta1 metric 20\n";
        assert_eq!(parse_default_route_device(multi).as_deref(), Some("eth0"));
        assert_eq!(parse_default_route_device(""), None);
        assert_eq!(parse_default_route_device("default via 10.0.0.1\n"), None);
    }

    #[test]
    fn extra_positionals_are_an_error_not_last_wins() {
        let err = cfg_from(&["Starbucks", "Customer"]).err().unwrap();
        assert!(err.contains("extra argument"), "got: {err}");
        assert_eq!(
            cfg_from(&["Starbucks Customer"]).unwrap().ssid,
            "Starbucks Customer"
        );
    }

    #[test]
    fn hotspot_rejects_short_passphrase() {
        let err = cfg_from(&["--hotspot", "--hotspot-pass", "short"])
            .err()
            .unwrap();
        assert!(err.contains("8-63"), "got: {err}");
    }

    /// The limits are create_ap's, and it counts characters. Checking bytes let
    /// a 5-character multibyte passphrase through to be rejected downstream with
    /// a misleading message, and nothing bounded the top end at all.
    #[test]
    fn hotspot_passphrase_limits_are_in_characters() {
        assert!(valid_passphrase("goodpass1"));
        assert!(!valid_passphrase("ééééé")); // 5 chars, 10 bytes
        assert!(valid_passphrase(&"é".repeat(8)));
        assert!(!valid_passphrase(&"x".repeat(64)));
        assert!(valid_passphrase(&"x".repeat(63)));
        // A newline would inject directives into create_ap's hostapd config.
        assert!(!valid_passphrase("abc\ndef1234"));
    }

    #[test]
    fn hotspot_ssid_length_is_bounded() {
        let err = cfg_from(&["--hotspot", "--hotspot-ssid", &"x".repeat(33)])
            .err()
            .unwrap();
        assert!(err.contains("1-32"), "got: {err}");
        assert!(cfg_from(&["--hotspot", "--hotspot-ssid", &"x".repeat(32)]).is_ok());
    }

    // Naming a hotspot you never asked to start is a typo, not an intent.
    #[test]
    fn hotspot_ssid_without_hotspot_is_an_error() {
        let err = cfg_from(&["--hotspot-ssid", "myap"]).err().unwrap();
        assert!(err.contains("without --hotspot"), "got: {err}");
    }

    // --once exits immediately, so a hotspot would be torn down the moment it
    // came up. Better to say so than to silently do nothing useful.
    #[test]
    fn hotspot_conflicts_with_once() {
        let err = cfg_from(&["--hotspot", "--hotspot-pass", "goodpass1", "--once"])
            .err()
            .unwrap();
        assert!(err.contains("watch mode"), "got: {err}");
    }

    // No passphrase is an error no longer: one gets generated and remembered.
    #[test]
    fn hotspot_defaults_to_hostname_name_and_channel_one() {
        let cfg = cfg_from(&["--hotspot"]).unwrap();
        let hs = cfg.hotspot.expect("hotspot configured");
        assert_eq!(hs.ssid, default_hotspot_ssid());
        assert!(hs.ssid.ends_with("sirensong"), "got: {}", hs.ssid);
        // None means "follow the station", resolved at start rather than parse.
        assert_eq!(hs.channel, None);
        // Deliberately not asserting `pass.is_none()`: parsing reads
        // SIRENSONG_HOTSPOT_PASS, so that assertion failed for anyone who had it
        // exported — a test that depends on the developer's shell. What actually
        // matters here is that no *file* was read or written at parse time, which
        // the surrounding assertions cover.
    }

    #[test]
    fn generated_passphrase_is_long_and_unambiguous() {
        let p = generate_passphrase().expect("should read /dev/urandom");
        assert_eq!(p.len(), 20);
        // The confusable pairs are removed by dropping one side of each:
        // 0/O, 1/l/I, 8/B. Lowercase `o` stays — nothing it can be mistaken for.
        for bad in ['0', 'O', '1', 'l', 'I', 'B'] {
            assert!(!p.contains(bad), "ambiguous glyph {bad} in {p}");
        }
        assert_ne!(p, generate_passphrase().unwrap(), "should not be constant");
    }

    // The delimiters in a WIFI: URI have to be escaped or a password containing
    // one would silently produce a QR code that joins the wrong thing.
    #[test]
    fn wifi_uri_escapes_delimiters() {
        assert_eq!(escape_wifi_uri("plain"), "plain");
        assert_eq!(escape_wifi_uri("a;b"), "a\\;b");
        assert_eq!(escape_wifi_uri("a:b,c"), "a\\:b\\,c");
        assert_eq!(escape_wifi_uri("back\\slash"), "back\\\\slash");
    }

    #[test]
    fn hotspot_ssid_can_be_overridden() {
        let cfg = cfg_from(&[
            "--hotspot",
            "--hotspot-ssid",
            "myap",
            "--hotspot-pass",
            "goodpass1",
        ])
        .unwrap();
        assert_eq!(cfg.hotspot.expect("configured").ssid, "myap");
    }

    // `country 00` is the world domain: transmitting is forbidden on most
    // channels, which is what makes create_ap fail with an opaque hostapd error.
    #[test]
    fn reads_country_from_iw_reg() {
        let world = "global\ncountry 00: DFS-UNSET\n\t(2402 - 2472 @ 40), (N/A, 20), (N/A)\n";
        assert_eq!(parse_regdom_country(world).as_deref(), Some("00"));

        let ph = "global\ncountry PH: DFS-FCC\n\t(2400 - 2483 @ 40), (N/A, 20), (N/A)\n";
        assert_eq!(parse_regdom_country(ph).as_deref(), Some("PH"));

        assert_eq!(parse_regdom_country("no country line here"), None);
    }

    #[test]
    fn ssid_truncation_respects_char_boundaries() {
        assert_eq!(truncate_ssid("short"), "short");
        assert_eq!(truncate_ssid(&"a".repeat(40)).len(), 32);
        // 2 bytes per char: must not be split mid-character
        let cut = truncate_ssid(&"é".repeat(20));
        assert!(cut.len() <= 32);
        assert!(cut.chars().all(|c| c == 'é'));
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
        assert_eq!(
            ok,
            Login::Submitted,
            "http_login should complete the mock portal flow"
        );
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
