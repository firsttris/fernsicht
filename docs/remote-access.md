# Remote access

Fernsicht is built for the LAN. Over the internet there are two ways:

- **Built in: your own rendezvous service** (a free Cloudflare Worker, or a small service on your
  own server). The host registers there; a paired device that does not find the host at home
  connects through it, directly where the networks allow (hole punching, IPv6) and through a relay
  where they do not. No VPN, no port forwarding. How to set it up and how it works:
  [over the internet](internet.md). Not seen live between two homes yet.
- **A VPN** that makes your devices look like one network (below). It works for the web viewer
  too; without one, the web viewer goes through a tunnel with HTTPS
  ([over the internet](internet.md#the-web-viewer)).

Either way the app's connection is end-to-end encrypted (Noise) and only lets paired devices in,
and FEC, bitrate adaptation and HEVC/AV1 help on mobile and foreign Wi-Fi.

## Tailscale (recommended)

[Tailscale](https://tailscale.com) is a VPN without servers or port forwarding to set up. Every
device runs the Tailscale app and signs in to the same account; each gets a fixed address
(`100.x.y.z`) and a name (`zentrale`) that work wherever the device is. Devices connect **directly**
to each other – Tailscale only helps them through their routers (NAT traversal) and relays encrypted
traffic when that fails. It is built on WireGuard and free for personal use (3 users, 100 devices).

On Bazzite it is usually installed already:

```sh
sudo systemctl enable --now tailscaled
sudo tailscale up        # prints a link to sign in
```

On a phone, install the Tailscale app and sign in with the same account.

With Fernsicht:

- **The app:** pair once with the host's Tailscale name or address (`zentrale` or `100.x.y.z`).
  Discovery does not pass through Tailscale (no broadcasts), but the app asks paired hosts directly,
  so it is listed from then on.
- **The web viewer:** open `http://zentrale:47800` or `http://100.x.y.z:47800`. The host answers the
  browser with the address it reaches it by – its Tailscale address – so WebRTC should connect. Not
  tried live yet.

!!! note "Packet size"
    Tailscale carries 1,280 bytes per packet. The host asks the route's MTU towards the client and
    sends video in datagrams that fit (1,252 bytes over IPv4, 1,232 over IPv6), so none is split in
    two; the log says so when the session starts.

## Cloudflare

A normal **Cloudflare Tunnel** with a public hostname (`fernsicht.example.com`) does not work:

- the web viewer's **page** would get through, even with HTTPS, but not its picture: WebRTC needs its
  own UDP connection to the host;
- the **app** does not get through at all: such tunnels do not forward UDP to port 47800.

**Cloudflare Zero Trust with WARP** (a private network) does work: `cloudflared` on the host shares
the home network (e.g. `192.168.178.0/24`), the **WARP client** runs on the laptop or phone. That
behaves like a VPN and carries UDP, so the app and the web viewer work as in the LAN (connect to the
host's LAN address; discovery does not pass, paired hosts are asked directly). Free for up to 50
users; a Cloudflare account is needed, an own domain is not.

The catch is latency: every packet goes through a Cloudflare server (phone → Cloudflare → host),
10–40 ms more depending on where you are, against 10–16 ms glass-to-glass in the LAN.

| | Tailscale | Cloudflare WARP + Tunnel |
|---|---|---|
| UDP (app, web viewer) | ✅ | ✅ |
| Path of the data | mostly direct | always through Cloudflare |
| Extra latency | ≈ 0–5 ms (direct) | ≈ 10–40 ms |
| Setup | an app on each device, sign in | `cloudflared` + Zero Trust rules + WARP client |

For streaming, Tailscale is the better fit; Cloudflare makes sense if you use it anyway or a network
blocks direct connections (some company Wi-Fi). The overlay's glass-to-glass and loss figures show
the difference.

## Port forwarding

Forward UDP 47800 on the router to the host and pair the app with your public address (or a DynDNS
name). That should work for the app – the connection is encrypted and only paired devices get in –
but it has not been tested. It is not enough for the web viewer. A VPN is the safer choice.
