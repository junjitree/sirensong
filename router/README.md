# Running sirensong on a GL.iNet travel router

Files for running sirensong **on** the router, so it stays signed in and
everything behind it rides one authenticated connection.

Verified on a **GL.iNet Slate 7 (GL-BE3600), firmware 4.9.0**, against a Cisco
Meraki free-plan splash. Written against GL's `gl-repeater` daemon rather than
anything model-specific, so other GL.iNet models plausibly work — untested.

| file                  | goes to                         | what it does                   |
| --------------------- | ------------------------------- | ------------------------------ |
| `sirensong.init`      | `/etc/init.d/sirensong`         | procd service, `START=95`      |
| `sirensong.config`    | `/etc/config/sirensong`         | UCI: enabled / ssid / interval |
| `sirensong-switch.sh` | `/etc/gl-switch.d/sirensong.sh` | hardware slider on/off         |

## Install

Cross-compile (the router is aarch64 musl), then:

```sh
scp target/aarch64-unknown-linux-musl/release/sirensong root@ROUTER:/usr/bin/
scp router/sirensong.init                root@ROUTER:/etc/init.d/sirensong
scp router/sirensong.config              root@ROUTER:/etc/config/sirensong
scp router/sirensong-switch.sh           root@ROUTER:/etc/gl-switch.d/sirensong.sh
ssh root@ROUTER 'chmod +x /usr/bin/sirensong /etc/init.d/sirensong /etc/gl-switch.d/sirensong.sh
                 /etc/init.d/sirensong enable && /etc/init.d/sirensong start'
```

Dropbear has no sftp-server, so if `scp` fails use
`ssh root@ROUTER 'cat > /path' < localfile`.

Logs go to syslog: `logread -e sirensong`.

## The hardware slider

Optional. Claims the physical slider to toggle sirensong, and puts a label on
the screen when you flip it:

```sh
uci set switch-button.@main[0].func='sirensong'
uci commit switch-button
```

Follows GL's own convention — the `pressed` position is on, which on the Slate 7
is the side **away from the dimple**. The dimple appears to be a moulded grip
rather than a marker, and there is no LED there (`/sys/class/leds` is empty), so
read the state off the screen rather than the case.

The slider's previous job was turning the display on and off; claiming it gives
that up. `uci delete switch-button.@main[0].func` puts it back.

## Firmware settings this depends on

Four settings outside these files hold the setup together. **All are restored to
defaults by a firmware upgrade or factory reset**, and each fails silently.

1. **`auto_portal='0'`** on the network with the portal, in
   `/etc/config/repeater`. GL's own captive-portal helper drops Tailscale around
   a portal login and does not reliably bring it back, leaving the router online
   but unreachable — `logread` shows `disable tailscale` / `enable tailscale`
   and then a `tailscale0` with no address. Sirensong does that login itself, so
   the helper is pure downside. Index the network by SSID; do not hardcode it.

2. **`--login-server` in `/usr/bin/gl_tailscale`** if you use Headscale. The
   firmware runs `tailscale up --reset` on every `ifup`, which wipes an
   unspecified login server back to the SaaS default.

3. **`tailscale.settings.enabled='1'` and `lan_enabled='1'`** — the init script
   exits silently when disabled, so `/etc/init.d/tailscale start` appears to do
   nothing.

4. **A firewall zone for `tailscale0`.** Without it `defaults.input=REJECT`
   sends an RST, which looks like `Connection refused` while Headscale still
   reports the node online.

## Notes

- The backend is chosen at runtime by `/etc/init.d/repeater` existing — no flag,
  no build feature. That file ships in every GL mode, so being on this backend
  says nothing about the uplink; sirensong checks the default route before
  rotating and declines when the station is not carrying traffic.
- Rotation restarts `gl-repeater` (~30s, no device reboot). It is a last resort:
  the portal is tried on the current MAC first, which usually works and takes
  ~2s.
- Measured cold boot to authenticated: ~30s. To reachable over Tailscale: ~112s.
