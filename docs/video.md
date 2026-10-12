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

**10 bit:** a 10-bit desktop is streamed in 10 bit where the client decodes the codec in 10 bit
(HEVC Main 10, AV1 Main at 10 bit; the Hello says which, for the decoder the client would pick):
the conversions write P010 instead of NV12, the encoders take it as it is, and gradients keep
their 1,024 steps instead of banding at 256. H.264 stays 8 bit (no hardware decodes High 10), and so
does a client whose decoder cannot; the web viewer always gets 8 bit. Each frame carries the
screen's colour, and a screen that changes it (another depth, HDR switched on) gets a new encoder
and a keyframe, like a new size. The stream says its colour (depth, primaries, transfer) in its
VUI or sequence header; the client reads it from every decoded frame, and the overlay names it
(“HEVC 10 Bit HDR · Vulkan Video”).

**HDR:** an HDR desktop (KWin, gamescope or GNOME with HDR on) scans out the signal its monitor
gets: BT.2020, PQ-encoded. The compositor says so in the connector's KMS properties
(`HDR_OUTPUT_METADATA`'s EOTF, `Colorspace`), which the capture reads at the start and once a
second after (switching HDR on is a modeset). The host converts such a screen with the BT.2020
matrix and tags the stream BT.2020 and PQ, in 10 bit where the client takes it. The client shows
it on an SDR display tone-mapped: PQ to light relative to SDR white (203 nits, ITU-R BT.2408), the
highlights above 80 % of it compressed up to a peak of 1,000 nits, BT.2020 converted to BT.709
(ITU-R BT.2087), sRGB encoded; so an HDR desktop no longer looks washed out. Not done yet: showing
HDR on an HDR display (an HDR10 swapchain), HLG, and the stream's mastering metadata. Compositors
that move part of the colour pipeline into the display hardware (plane colour pipelines) scan out
values that are not the signal yet; capture cannot undo that.

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
    GPU's video processor (`scale_vaapi`) converts RGB to NV12 (P010 for a 10-bit stream) in limited
    range with the stream's matrix and scales to the stream size. Imports are cached for the last four buffers, keyed by the buffer's inode and
    layout (as on the NVIDIA path). Measured on the RX 7800 XT: import, conversion and encode of a
    1080p frame in about 2 ms.

=== "NVIDIA (NVENC)"

    ```text
    DMA-BUF (RGB) ──import──► Vulkan image ──compute shader──► NV12 buffer ══CUDA import══► NVENC
    ```

    NVENC takes neither foreign DMA-BUFs nor RGB in the format KMS hands out. The `gpu` crate imports
    the DMA-BUF into Vulkan (with its tiling modifier), a compute shader converts RGB to NV12 or
    P010 (the stream's matrix, scaled to the stream size) into a buffer whose memory is exported as an opaque fd (rows
    on 256 bytes); CUDA imports that memory once, and NVENC reads it as its input frame, without a
    copy. Measured on the GTX 1080: 2.26 ms per 1080p frame for import, conversion and encode.

=== "Any GPU (Vulkan Video)"

    ```text
    DMA-BUF (RGB) ──import──► Vulkan image ──compute shader──► NV12 buffer ──copy──► encoder image ──► h264/hevc/av1_vulkan
    ```

    `--encoder vulkan`: the same import and compute shader as on NVIDIA, but the buffer stays on the
    device: in the same submission it is copied into an image of the pool of FFmpeg's Vulkan encoder
    (both planes of one multi-planar image), which waits for and signals the image's timeline
    semaphore. No CUDA, no second API. FFmpeg 7.1 or newer at runtime. Measured on the GTX 1080:
    1.7 ms per 1080p H.264 frame for import, conversion, copy and encode (NVENC through CUDA: 2.25
    ms), 2.1 ms for HEVC; on the RX 7800 XT (RADV) 1.7 ms for both. NVIDIA's rate control keeps less
    strictly to one frame: a keyframe at 20 Mbit/s is about 1.6 frames' worth (NVENC: one); RADV's
    about 1.2 for H.264 and less than one for HEVC. H.264 and HEVC only: FFmpeg 8.1's Vulkan AV1 encoder writes
    the encoder's aligned size as the frame size (1920×1088 for 1080p on RADV, `vulkan_encode_av1.c`),
    and AV1 has no cropping, so a client asking for AV1 gets HEVC from it. Not chosen by `--encoder auto` yet; intra refresh is
    NVENC's only.

    Tuning: the lowest-latency mode the driver takes (ultra-low latency, else low latency; NVIDIA's
    driver on the GTX 1080 refuses the first for the whole profile). IDR frames come on request, and
    else every 2^15 frames (9 minutes at 60 fps): FFmpeg sizes HEVC's picture order count to the
    GOP, which has at most 16 bits (`vulkan_encode_h265.c`).

=== "No GPU (synthetic)"

    The synthetic codec makes frames of realistic size for the bitrate, with a checksum instead of a
    picture. Transport, FEC and latency are measured for real on any machine.

Encoder settings follow what game streamers use for low latency: no B-frames, an endless GOP with
keyframes only on request (stream start, loss), CBR with a buffer of one frame, one packet per input
frame without queueing (`async_depth = 1` on VAAPI, `delay = 0`, preset p1 and tune `ull` on NVENC).

**Intra refresh** (NVENC, when the client asks: `--smooth`, in the app “Nach Verlusten: Flüssig”):
instead of one keyframe, each frame refreshes a band of the picture, all of it every 0.5 s (the GOP
is the refresh period, `ldkfs` keeps keyframes out of it). A lost frame then costs no keyframe: the
client decodes on, the damage shows as a smear for at most half a second, and no keyframe burst
loads the link right after a loss. The default stays keyframes, because they repair the picture
sharp in one round trip. A bitrate change still starts NVENC anew with an IDR frame (FFmpeg resets
the session for it); the rate controller spaces those out.
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
AV1 is never picked). It tries, in this order:

1. **Vulkan Video**, on any GPU whose driver has it (NVIDIA from Pascal, AMD and Intel with Mesa) and
   FFmpeg 7.1 or newer. FFmpeg decodes on the window's own Vulkan device: it gets our instance,
   device and queues (one lock per queue, shared with the renderer) and decodes into its pool of NV12
   images, which the renderer samples where they are (`Picture::Vulkan`).
2. VAAPI on AMD/Intel.
3. NVDEC on NVIDIA.

`--decoder` picks one. Decoder output is a frame per packet, no frame threading.

With several GPUs, Vulkan runs on the one that holds the pictures: the client's window on the VAAPI
decoder's GPU (matched by render node, `VK_EXT_physical_device_drm`), the host's NVENC converter on
the CUDA device (matched by UUID). `FERNSICHT_VULKAN_DEVICE` (part of a device name) overrides it.

The **renderer** (`render`, Vulkan) draws NV12 or P010 as RGB with a shader (the picture's
limited range and matrix, BT.709 or BT.2020), letterboxed into the window, and the host's pointer
over it. 10-bit planes are sampled as 16-bit words, which fits P010 and the Vulkan decoder's
`10X6` images alike (the 10 bits on top):

- VAAPI surfaces are exported as DMA-BUFs and imported into Vulkan without a copy (0.15 ms to convert
  and draw at 1080p).
- NVDEC pictures are copied on the GPU into memory CUDA exports as a file descriptor, which Vulkan
  imports (CUDA's virtual memory API; `Picture::Exported`): 0.3 ms to copy, convert and draw at
  1080p on the GTX 1080, against 1.1 ms through the CPU before.
- Vulkan Video pictures need neither: the renderer views the two planes of the decoded image (it is
  created with `MUTABLE_FORMAT`) and draws them, 0.07 ms at 1080p on the GTX 1080. The image is
  coded larger than the picture (1088 rows for 1080); the shader shows only the picture's part. Each
  submission waits for the image's timeline semaphore and signals the next value, FFmpeg's contract
  for its frames. Decoding itself takes as long as NVDEC (the same engine), and `decode` waits for
  it, so the overlay's Decode stays the decoder's time.

The window presents in mailbox mode: no tearing, and a new frame replaces one that has not been shown
yet. In gaming mode it presents immediately where the driver offers it: a frame shows the moment it
is drawn instead of at the next refresh (up to one refresh sooner), and may tear. On Wayland the
compositor has to allow tearing (KWin does for full-screen windows); where it does not, mailbox
stays. Switching the mode rebuilds the swapchain; the log names the mode (“swapchain … Immediate”).

## When the GPU stops responding

A GPU can stop finishing work: a hung or faulted GPU (on NVIDIA the kernel log shows an Xid), or a
lost device. Work given to it then never finishes, and whoever waits for it waits for ever; FFmpeg's
own waits have no time limit. So every wait for the GPU in Fernsicht has one (1 s; 5 s for a
decoded frame, as a new decoder session can be slow to start; 30 s on a software rasterizer such as
llvmpipe, which is slow, not hung), and work that does not finish in it
**gives the GPU up** (`Gpu::give_up`): from then on submissions fail at once, nothing waits for the
GPU any more, and objects it may still use are left to the process's end instead of destroyed. The
Vulkan codecs no longer call FFmpeg on it (which would wait for ever while freeing its frames).

The session then ends with a reason instead of living on without a picture: the client's present
thread stops and the session fails with `gpu-lost` ("Die Grafikkarte dieses Computers reagiert nicht
mehr"); on the host the encode loop ends, and with it the session. Tested with a hang made on
purpose (a submission that waits for a semaphore nobody signals), on llvmpipe as on the GPUs.

## Recording

`fernsicht-client --record <file>` writes the received bitstream as it arrives: `ffplay file.h264`,
`file.h265` for HEVC, raw AV1 OBUs with `ffplay -f obu file.obu`.
