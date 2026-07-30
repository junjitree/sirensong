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

struct Config {
    ssid: String,
    once: bool,
    interval: Duration,
    quiet: bool,
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
         Log verbosity is otherwise controlled by RUST_LOG (e.g. RUST_LOG=debug)."
    );
}

fn parse_args_from<I: Iterator<Item = String>>(args: I) -> Result<Config, String> {
    let mut ssid = None;
    let mut once = false;
    let mut interval = Duration::from_secs(60);
    let mut quiet = false;
    let mut args = args.peekable();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-o" | "--once" => once = true,
            "-q" | "--quiet" => quiet = true,
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

    Ok(Config {
        ssid: ssid.unwrap_or_else(|| DEFAULT_SSID.to_string()),
        once,
        interval,
        quiet,
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

/// Lightweight captive-portal check: a raw HTTP GET to a well-known
/// `generate_204` endpoint. Real internet returns `204`; a captive portal
/// intercepts with a `200`/redirect.
fn is_online_blocking() -> bool {
    use std::io::{Read, Write};
    use std::net::{TcpStream, ToSocketAddrs};

    let addr = match (CONNECTIVITY_HOST, 80).to_socket_addrs() {
        Ok(mut it) => match it.next() {
            Some(a) => a,
            None => return false,
        },
        Err(_) => return false,
    };
    let mut stream = match TcpStream::connect_timeout(&addr, Duration::from_secs(5)) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    let req = format!(
        "GET /generate_204 HTTP/1.0\r\nHost: {CONNECTIVITY_HOST}\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }

    let mut buf = [0u8; 64];
    match stream.read(&mut buf) {
        Ok(n) => {
            let head = String::from_utf8_lossy(&buf[..n]);
            head.starts_with("HTTP/1.0 204") || head.starts_with("HTTP/1.1 204")
        }
        Err(_) => false,
    }
}

async fn is_online() -> bool {
    tokio::task::spawn_blocking(is_online_blocking)
        .await
        .unwrap_or(false)
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

/// (Re)connect to the Wi-Fi, forcing a fresh activation so NetworkManager rolls
/// a new randomized MAC (`wifi.cloned-mac-address=random`). The new MAC is what
/// lets us re-authenticate past the captive portal's per-device usage cap — the
/// portal treats each MAC as a brand-new visitor.
///
/// Tries an existing saved profile first (`connection down` then `up`), then
/// falls back to associating with an open network (`dev wifi connect`).
fn connect_to_wifi(cfg: &Config) -> bool {
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
        warn!("no billing_pick form on splash; portal markup may have changed");
        return false;
    };

    let Some(token) = capture(form, r#"name="authenticity_token"\s+value="([^"]+)""#) else {
        warn!("authenticity_token missing in free-plan form; portal markup may have changed");
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

/// One reconciliation pass: if already online, do nothing. Otherwise reconnect
/// — which rolls a fresh MAC (see `connect_to_wifi`) so the portal treats us as
/// a new device — then authenticate over HTTP against the captive portal.
async fn reconcile(cfg: &Config) -> bool {
    if is_online().await {
        debug!("already online");
        return true;
    }

    // Don't hijack whatever network we're on: only reconnect if the target
    // SSID is actually in range. Off-site (e.g. home Wi-Fi) this makes us a
    // no-op instead of dropping the current connection to hunt for Starbucks.
    if !ssid_in_range(&cfg.ssid) {
        debug!(ssid = %cfg.ssid, "offline but target SSID not in range; leaving current network alone");
        return false;
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
    } else {
        run_watch(&cfg).await;
    }
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
}
