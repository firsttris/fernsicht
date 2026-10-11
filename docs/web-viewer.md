# Web viewer

The web viewer lets you use a Fernsicht host from a browser, without
installing anything, from any computer, tablet or phone on the same
network. The host serves the page itself.

![The web viewer's connect page](screenshot-viewer-connect.png)

## Connect

1. In the browser, open the host's address: `http://192.168.178.87:47800`
   (the install script prints it at the end).
2. On the host, choose “Gerät koppeln” (pair device), either in the app or
   with `fernsicht-host-agent pair`.
3. Enter the PIN in the browser and click **“Verbinden”** (connect).

## How it works

- **Access:** The PIN is valid for one session. The browser is not
  paired and needs a new PIN next time.
- **Transport:** Video (AV1 if the browser and the host's graphics card
  support it, otherwise H.264), audio and input are sent encrypted over
  WebRTC.
- **Input:** In **Desktop** mode the mouse controls the host directly. In
  **Gaming** mode a click into the video captures the mouse pointer, and
  Esc releases it. Controllers work through the Gamepad API; some browsers
  only expose them on HTTPS pages.
- **Monitors:** with two or more monitors on the host, “Bildschirm wählen”
  (choose screen) in the toolbar switches the one shown.
- **System keys:** Ctrl+Alt+Del, the Windows key and similar keys are in
  the “Tasten senden” (send keys) menu. In full screen (button in the
  toolbar), Chrome and Edge also pass through the Windows key, Alt+Tab and
  Esc; to leave full screen, hold Esc.
- **What does not work:** Keys that the browser keeps for itself (for
  example Ctrl+W) do not reach the host. Use the app for that. On the
  iPhone there is no full screen for web pages.

## Phones and tablets

<table>
  <tr>
    <td width="62%"><img src="screenshot-viewer-phone-landscape.png" alt="The web viewer on a phone in landscape"></td>
    <td width="38%"><img src="screenshot-viewer-phone.png" alt="The web viewer on a phone in portrait"></td>
  </tr>
</table>

| Gesture | Desktop mode | Touchpad mode |
|---|---|---|
| Tap | left click where the finger is | left click where the pointer is |
| Move one finger | drag with the left button | move the pointer |
| Long press | right click | then move: drag |
| Two fingers together | scroll | scroll |
| Short tap with two fingers | right click | right click |
| Spread or pinch two fingers | zoom into the picture | zoom into the picture |

- **Zoom** happens only on the phone; the host does not notice. While
  zoomed in, two fingers move the visible area; the magnifier in the
  toolbar resets it.
- **Touchpad mode** (pointer icon in the toolbar, shown on touch screens):
  your finger moves the pointer like on a touchpad, which is easier for
  small targets. In Gaming mode this always applies (phones have no
  pointer capture).
- **On-screen keyboard:** keyboard icon in the toolbar. What you type is
  sent to the host as key presses, word suggestions included. For this the
  viewer assumes that the host uses the keyboard layout of the browser
  language (German or US).
- Portrait and landscape both work; the video adapts when you rotate the
  device, and taps keep hitting the right place.

## Limitations

!!! warning "Unencrypted on the LAN"
    The page and the PIN travel unencrypted through the LAN (`http://`).
    The PIN is valid only once and only for 5 minutes.

- **Firewall:** The host needs TCP 47800 for the page and a free UDP port
  above 1024 for WebRTC. Fedora and Bazzite allow both;
  `install-host.sh` opens 47800.
- **Browser:** Tested with Chrome and Firefox and on a phone. Firefox plays
  H.264 only with a suitable H.264 decoder installed.
- **Over the internet:** only through a VPN for now, see
  [remote access](remote-access.md).
