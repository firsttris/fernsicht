# Remote access

Fernsicht is built for the LAN. Over the internet it works today through a VPN that makes your
devices look like one network; built-in NAT traversal is on the [roadmap](next-steps.md).

## What is missing for the internet

- **Discovery** is a broadcast and only reaches the local network.
- **NAT:** the router in front of the host lets nothing in from outside. There is no rendezvous
  server yet that would set up a connection through both routers (hole punching), as Parsec or
  Moonlight with Sunshine do.
- **Web viewer:** the host only tells the browser its LAN address, so WebRTC only connects in the
  same network. Its page and the PIN travel over plain `http://`; fine in the LAN, not over the
  internet.

What already holds up: the app's connection is end-to-end encrypted (Noise) and only lets paired
devices in, and FEC, bitrate adaptation and HEVC/AV1 help on mobile and foreign Wi-Fi.

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
    Fernsicht sends datagrams of up to 1,400 bytes; Tailscale carries 1,280 per packet. Linux then
    splits a datagram in two. That works, but losing either half loses the whole datagram, which
    shows on mobile networks. If the overlay shows more loss over Tailscale than without it, a
    smaller packet size (an option such as `--mtu 1200`, or detecting it) is a small change; see
    [next steps](next-steps.md).

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
