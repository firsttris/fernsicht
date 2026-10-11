# Video path

From the framebuffer on the host to the window on the client, the picture stays in GPU memory. This
page follows it through each step and each GPU vendor.

## Capture

The host reads the screen over **KMS** (kernel mode setting): at each vblank of the monitor's CRTC it
looks up the framebuffer the primary plane shows and exports it as a **DMA-BUF**. The compositor is
not involved, so this works the same on KDE, GNOME, Hyprland, gamescope, X11 and the login screen,
without a portal dialog. It needs `CAP_SYS_ADMIN` (the system service runs as root). The cursor plane
is read separately and sent as its own packets. Details, options and limits: [KMS capture](kms-capture.md).

Desktops scan out in one of eight RGB formats: 8 or 10 bit, RGB or BGR order, with or without alpha
(KDE on AMD uses the 10-bit `AB30`). All eight are supported on every path below.

With several monitors the host lists them to the client; a switch opens the other monitor on the same
card and sends a keyframe (see [protocol](protocol.md#monitors)).

A **test pattern** (a moving bar and a circling pointer) replaces the screen where there is no
display or no root, e.g. in CI.

## Conversion and encoding

=== "AMD / Intel (VAAPI)"

    ```text
    DMA-BUF (RGB) ──import──► VA surface ──scale_vaapi──► NV12 (BT.709) ──► h264/hevc/av1_vaapi
    ```

    The DMA-BUF is imported as a VA surface with `vaCreateSurfaces` (DRM PRIME), without a copy. The
    GPU's video processor (`scale_vaapi`) converts RGB to NV12 in BT.709 limited range and scales to
    the stream size. Imports are cached per buffer. Measured on the RX 7800 XT: import, conversion and
    encode of a 1080p frame in about 2 ms.

=== "NVIDIA (NVENC)"

    ```text
    DMA-BUF (RGB) ──import──► Vulkan image ──compute shader──► NV12 buffer ══CUDA import══► NVENC
    ```

    NVENC takes neither foreign DMA-BUFs nor RGB in the format KMS hands out. The `gpu` crate imports
    the DMA-BUF into Vulkan (with its tiling modifier), a compute shader converts RGB to NV12
    (BT.709, scaled to the stream size) into a buffer whose memory is exported as an opaque fd; CUDA
    imports that memory once and copies each frame device to device into NVENC's input. Measured on
    the GTX 1080: 2.46 ms per 1080p frame for import, conversion, copy and encode.

=== "No GPU (synthetic)"

    The synthetic codec makes frames of realistic size for the bitrate, with a checksum instead of a
    picture. Transport, FEC and latency are measured for real on any machine.

Encoder settings follow what game streamers use for low latency: no B-frames, an endless GOP with
keyframes only on request (stream start, loss), CBR with a buffer of one frame, one packet per input
frame without queueing (`async_depth = 1` on VAAPI, `delay = 0`, preset p1 and tune `ull` on NVENC).
The GPU's clocks are kept up during sessions on AMD/Intel, because a GPU that clocks down between
frames encodes the next one slower.

## Codecs

Fernsicht encodes **AV1**, **HEVC** (Main) and **H.264** (High), in hardware only.

| GPU | Encodes (host) | Decodes (client) |
|---|---|---|
| AMD RX 7800 XT (VCN 4) | H.264, HEVC, **AV1** | H.264, HEVC, AV1 |
| NVIDIA GTX 1080 (Pascal) | H.264, HEVC | H.264, HEVC (**no AV1**) |
| NVIDIA from RTX 30, AMD from RX 6000, Intel Arc | – | AV1 |
| NVIDIA from RTX 40, AMD from RX 7000, Intel Arc | AV1 | – |

How a session picks one:

1. The **client** asks its hardware what it decodes (VAAPI: `vaQueryConfigProfiles` and entry points;
   NVIDIA: `cuvidGetDecoderCaps`) and lists AV1 and HEVC only where they are decoded in hardware;
   H.264 always. The list travels in the Hello packet.
2. The **host** tries AV1, then HEVC, then H.264 among the client's codecs and takes the first its
   encoder opens; a GPU without AV1 encoding falls back by itself.
3. A **browser** lists what it decodes in its WebRTC offer; the host offers AV1 and H.264 there
   (Chrome and Firefox decode AV1 even without hardware). HEVC is not offered to browsers.

Older clients and hosts, from before this negotiation, speak H.264 with newer ones. The choice can be
fixed in the app's settings (“Videoformat”) or with `fernsicht-client --codec`.

What it gives: at the same bitrate HEVC is clearly sharper than H.264 (GTX 1080, 4 Mbit/s 1080p:
32.4 dB instead of 29.1 dB PSNR, with a third of the bytes), and AV1 another 10–20 % better than
HEVC. Latency stays the same, and smaller frames even shorten the network stage (HEVC 4.1 ms versus
H.264 5.0 ms glass-to-glass on the GTX 1080).

## Decoding and display

The client decodes with FFmpeg's own decoders on the GPU (chosen by name, so FFmpeg's CPU decoder for
AV1 is never picked): VAAPI on AMD/Intel, NVDEC on NVIDIA. Decoder output is a frame per packet, no
frame threading.

The **renderer** (`render`, Vulkan) draws NV12 as RGB with a shader (BT.709), letterboxed into the
window, and the host's pointer over it:

- VAAPI surfaces are exported as DMA-BUFs and imported into Vulkan without a copy (0.15 ms to convert
  and draw at 1080p).
- NVDEC pictures are copied to the CPU and uploaded for now (about 0.8 ms at 1080p); importing them
  directly is on the [roadmap](next-steps.md).

The window presents in mailbox mode: no tearing, and a new frame replaces one that has not been shown
yet. Presenting immediately (with tearing, for games) is a possible next step
([performance](performance.md)).

## Recording

`fernsicht-client --record <file>` writes the received bitstream as it arrives: `ffplay file.h264`,
`file.h265` for HEVC, raw AV1 OBUs with `ffplay -f obu file.obu`.
