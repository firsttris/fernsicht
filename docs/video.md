# Video path

From the framebuffer on the host to the window on the client, the picture stays in GPU memory. This
page follows it through each step and each GPU vendor.

## Capture

The host reads the screen over **KMS** (kernel mode setting): at each vblank of the monitor's CRTC it
looks up the framebuffer the primary plane shows and exports it as a **DMA-BUF**. The compositor is
not involved, so this works the same on KDE, GNOME, Hyprland, gamescope, X11 and the login screen,
without a portal dialog. It needs `CAP_SYS_ADMIN` (the system service runs as root). The cursor plane
is read separately, once the frame has gone on to the encoder, and sent as its own packets. Details,
options and limits: [KMS capture](kms-capture.md).

Desktops scan out in one of eight RGB formats: 8 or 10 bit, RGB or BGR order, with or without alpha
(KDE on AMD uses the 10-bit `AB30`). All eight are supported on every path below.

With several monitors the host lists them to the client; a switch opens the other monitor on the same
card, and the stream takes that monitor's shape (a new encoder at the fitted size, starting with a
keyframe; see [protocol](protocol.md#monitors)).

The session's **frame rate** is what the client asked for, but at most the monitor's refresh rate:
a client asking for 144 fps from a 60 Hz monitor gets 60, and the encoder sizes each frame for 60.

A **still screen** is not encoded at every refresh. Compositors draw into two or three buffers and
flip between them; the same framebuffer as before means nothing new. Once a buffer has been seen to
come back after others (some X11 desktops draw into the shown buffer and never flip: there every
frame counts), frames keep coming for half a second after the last change, so the picture settles,
then one a second. A change or a keyframe request goes out at once. The pointer is its own plane
and keeps moving meanwhile.

A **test pattern** (a moving bar and a circling pointer) replaces the screen where there is no
display or no root, e.g. in CI.

## Conversion and encoding

=== "AMD / Intel (VAAPI)"

    ```text
    DMA-BUF (RGB) ──import──► VA surface ──scale_vaapi──► NV12 (BT.709) ──► h264/hevc/av1_vaapi
    ```

    The DMA-BUF is imported as a VA surface with `vaCreateSurfaces` (DRM PRIME), without a copy. The
    GPU's video processor (`scale_vaapi`) converts RGB to NV12 in BT.709 limited range and scales to
    the stream size. Imports are cached for the last four buffers, keyed by the buffer's inode and
    layout (as on the NVIDIA path). Measured on the RX 7800 XT: import, conversion and encode of a
    1080p frame in about 2 ms.

=== "NVIDIA (NVENC)"

    ```text
    DMA-BUF (RGB) ──import──► Vulkan image ──compute shader──► NV12 buffer ══CUDA import══► NVENC
    ```

    NVENC takes neither foreign DMA-BUFs nor RGB in the format KMS hands out. The `gpu` crate imports
    the DMA-BUF into Vulkan (with its tiling modifier), a compute shader converts RGB to NV12
    (BT.709, scaled to the stream size) into a buffer whose memory is exported as an opaque fd (rows
    on 256 bytes); CUDA imports that memory once, and NVENC reads it as its input frame, without a
    copy. Measured on the GTX 1080: 2.26 ms per 1080p frame for import, conversion and encode.

=== "No GPU (synthetic)"

    The synthetic codec makes frames of realistic size for the bitrate, with a checksum instead of a
    picture. Transport, FEC and latency are measured for real on any machine.

Encoder settings follow what game streamers use for low latency: no B-frames, an endless GOP with
keyframes only on request (stream start, loss), CBR with a buffer of one frame, one packet per input
frame without queueing (`async_depth = 1` on VAAPI, `delay = 0`, preset p1 and tune `ull` on NVENC).
The GPU's clocks are kept up during sessions on AMD/Intel, because a GPU that clocks down between
frames encodes the next one slower.

**Chroma siting:** both conversions put the colour samples on the even columns, between two rows
("left", H.264's and HEVC's default; AV1 calls it "vertical"), and the VAAPI encoders write that
into the stream (NVENC streams leave it at the default, which is the same). Browsers place the
colour where the stream says, so text keeps its colours at the edges; the client's renderer reads
it the same way.

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

With several GPUs, Vulkan runs on the one that holds the pictures: the client's window on the VAAPI
decoder's GPU (matched by render node, `VK_EXT_physical_device_drm`), the host's NVENC converter on
the CUDA device (matched by UUID). `FERNSICHT_VULKAN_DEVICE` (part of a device name) overrides it.

The **renderer** (`render`, Vulkan) draws NV12 as RGB with a shader (BT.709), letterboxed into the
window, and the host's pointer over it:

- VAAPI surfaces are exported as DMA-BUFs and imported into Vulkan without a copy (0.15 ms to convert
  and draw at 1080p).
- NVDEC pictures are copied on the GPU into memory CUDA exports as a file descriptor, which Vulkan
  imports (CUDA's virtual memory API; `Picture::Exported`): 0.3 ms to copy, convert and draw at
  1080p on the GTX 1080, against 1.1 ms through the CPU before.

The window presents in mailbox mode: no tearing, and a new frame replaces one that has not been shown
yet. Presenting immediately (with tearing, for games) is a possible next step
([performance](performance.md)).

## Recording

`fernsicht-client --record <file>` writes the received bitstream as it arrives: `ffplay file.h264`,
`file.h265` for HEVC, raw AV1 OBUs with `ffplay -f obu file.obu`.
