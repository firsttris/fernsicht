# Code review

A review of the whole code base (2026-10-11, commit `eace1a2`): correctness, performance, security,
clean code and consistency. Six areas were read in full: transport, host, GPU and codecs, client,
security, frontend with CI. Nothing was built or measured for the review itself; timings marked
"estimate" are estimates.

Every finding has a fixed ID and a status. Work goes from top to bottom: bugs first (A), then
security (B), then performance (C), the strategic steps (D) and clean-up (E).

**Status:** `open` · `fixed` (with the commit) · `wontfix` (with the reason)

## Overall

The foundation is strong: a real zero-copy path from KMS to the encoder, parsers that never panic
(proptests, fuzzing), careful unsafe code with layout tests, sound cryptography and replay window,
encoder settings already tuned for latency. The weak spots are error paths (threads that die
silently), the control loops (bitrate, keyframes), the logic around pairing and the web PIN, and
the limits FFmpeg puts on the next latency step.

## A. Bugs

### A1 · A pipeline thread dies and the session stays with a frozen picture

- **Where:** `apps/host-agent/src/lib.rs:1520` (capture `break`), `lib.rs:2157` (send ends on any
  error but `WouldBlock`, which never happens on a blocking socket), `lib.rs:739-757` (only web
  sessions check `running`), `apps/client/src/lib.rs:969` (any packet resets the client's timeout).
- **Problem:** capture ends on the first `next_frame` error (monitor standby, a framebuffer removed
  during a mode change or when a game starts direct scan-out). The send thread ends on ENETUNREACH
  or EHOSTUNREACH during a Wi-Fi blip. Encode follows. Audio, pongs and the monitor list keep the
  client's timeout alive: the picture stays frozen and the GPU boosted until the user reconnects.
- **Fix:** treat capture errors as transient (retry, reopen after N failures); ignore transient send
  errors; if a pipeline thread ends anyway, clear `running` and let the main loop end native
  sessions too (with Bye). Client: no video for a while after video was seen → reconnect.
- **Status:** fixed. `apps/host-agent/src/health.rs`: send errors that are only loss (unreachable,
  no buffers, firewall) are ridden out and logged once a second; capture errors are retried
  (5 ms doubling to 100 ms); a guard in the capture, encode and send threads ends the session when
  one stops (also by a panic), and the control loop then sends Bye. The client-side watchdog was
  left out on purpose: a monitor in standby legitimately sends no video for a long time. Tests:
  `health.rs`, `a_monitor_in_standby_pauses_the_stream_and_keeps_the_session` (`protocol.rs`).

### A2 · Bitrate adaptation ratchets down and never recovers

- **Where:** `crates/net/src/rate.rs:79-95`, `apps/host-agent/src/lib.rs:2068-2078`.
- **Problem:** smoothed loss between `CLEAN` (2 %) and `SOME_LOSS` (15 %) holds the rate forever and
  resets the clean timer. The filter rises fast and falls slowly. Every frame lost despite FEC costs
  30 %. With steady 3 % Wi-Fi loss (FEC's job) the rate steps down to the floor and stays there.
  `kbps = target` is stored even when the encoder rebuild failed, so it is never retried.
- **Fix:** count reports with no lost frames and no overflow as clean when loss is below
  `SOME_LOSS`; symmetric filter; only store the new rate when it was applied. Test: one lost frame,
  then 300 reports at 3 % loss → the rate returns to the ceiling.
- **Status:** fixed. A report is clean when no frame was lost, the sender kept up and smoothed loss
  is below 15 %; the filter is symmetric; a frame lost now and then lowers the rate gently (×0.85),
  only overflow or heavy loss hard (×0.7). The test also found that the 5 % rule kept the last step
  from reaching the ceiling (19.55 of 20 Mbit/s for good); the limits are exempt now. The encode
  loop keeps the old rate when a rebuild fails and tries again when the target changes. Tests in
  `rate.rs` (`wifi_loss_that_fec_hides_does_not_hold_the_bitrate_down` and others).

### A3 · Web sessions are not ended cleanly

- **Where:** `web/viewer/src/routes/live.tsx:81-85, 94-103`, `web/viewer/src/routes/session.tsx:17-27`,
  `web/viewer/src/lib/host.ts:76-79, 128-133`; host `apps/host-agent/src/web.rs:254-284, 310-322`,
  `lib.rs:741, 980-984`.
- **Problem:** the viewer ends on `disconnected`, which is transient and often recovers; it never
  calls `session.close()` on the ended screen or on unmount (browser back), so the peer connection
  keeps receiving and the host stays busy. On the host, a session whose browser never connects
  (blocked UDP port, tab closed while connecting) lives forever: str0m's ICE agent without remote
  candidates never reaches `Disconnected`, and a session whose HTTP answer timed out is installed
  anyway.
- **Fix:** viewer: close on unmount and on the ended path; only `failed`/`closed` are final, show
  "Verbindung unterbrochen…" with a grace timer on `disconnected`. Host: no `Connected` within 10 s
  ends the session; a failed `reply.send` stops it.
- **Status:** fixed. Viewer: `connectionPhase` (pure) tells `disconnected` (interrupted, shown as
  “Verbindung unterbrochen …”) from the end; an interruption longer than 10 s ends the session; an
  ended session is closed, and `holdSession` closes it when the page goes away (deferred, so React's
  StrictMode trial unmount does not end it); `close` is idempotent. Host: a browser that has not
  connected within 10 s ends its session; a session whose HTTP answer could not be delivered is
  stopped. Tests: `host.test.ts`, `live.test.tsx`, `a_browser_that_never_connects_does_not_keep_the_host_busy`.

### A4 · The GPU stays clocked high after `systemctl stop` or `restart`

- **Where:** `apps/host-agent/src/main.rs:305` (no signal handler), `apps/host-agent/src/power.rs:96-98`,
  `packaging/fernsicht-host.service` (`RuntimeDirectory=` is removed on stop).
- **Problem:** SIGTERM kills the process, `GpuBoost::drop` never runs, and the crash record in
  `/run/fernsicht` is deleted by systemd. The next session reads `high` as the old value.
- **Fix:** handle SIGTERM/SIGINT by setting `stop`; keep the record in the state directory.
- **Status:** fixed. `fernsicht_core::signal::stop_on_signals` turns SIGINT and SIGTERM into the
  stop flag (a thread in `sigwait`; a second signal exits at once); the host ends its session with
  Bye and the GPU's values return. The record lives in the state directory now. Test:
  `sigterm_ends_the_host_cleanly_and_tells_the_client` (`binaries.rs`).

### A5 · Switching to a monitor of another size distorts the picture

- **Where:** `apps/host-agent/src/lib.rs:1540-1565`, `crates/codec/src/vaapi.rs:396-404, 652`.
- **Problem:** the converter scales the new monitor to the old session size without keeping the
  aspect ratio: a 21:9 monitor is squeezed into 16:9.
- **Fix:** fit the new monitor into the stream size with letterboxing in the converter (VAAPI and
  the Vulkan shader); later, renegotiate the size.
- **Status:** fixed, by renegotiating rather than letterboxing (`pad_vaapi` needs FFmpeg 7, the
  AppImage ships 6.1). The stream size is `fit(source frame size, session box)`, computed per frame
  in the encode loop; when it changes, the encoder is rebuilt at the new size and its keyframe
  carries it. Pointer and input stay right because the monitor fills the picture again; the native
  client maps clicks onto the decoded size, the browser already used `videoWidth`. Explicit client
  sizes are now fitted too (no more stretching at session start, no upscaling). Test:
  `the_stream_takes_the_shape_of_the_monitor_shown`. Makes A18 reachable (FFmpeg rebuilds its
  frame pool on a size change), so A18 comes next.

### A6 · Session frame rate is not limited to the monitor's refresh rate

- **Where:** `apps/host-agent/src/lib.rs:1188-1213, 1845`, `crates/capture/src/kms.rs:248`.
- **Problem:** the encoder gets `framerate = fps` and a one-frame buffer of `bitrate/fps`; KMS only
  delivers the refresh rate. 144 fps from a 60 Hz monitor gives each frame 42 % of the bitrate.
- **Fix:** expose the refresh rate on the source, clamp fps to it, report the result in HelloAck.
- **Status:** fixed. `FrameSource::refresh_hz` (KMS knows it); `frame_rate` clamps the session's
  fps before the default bitrate and the encoder are set up, and HelloAck reports it. Still open:
  rates that are no divisor of the refresh rate (60 fps from 165 Hz) alternate between 2 and 3
  vblanks; whether to round to a divisor is a judgement call left for measurements (D8). Test:
  `the_frame_rate_is_at_most_the_monitors`.

### A7 · A render error after acquiring a swapchain image loses the image

- **Where:** `crates/render/src/vulkan/window.rs:282-303, 343-349`,
  `crates/render/src/vulkan/renderer.rs:536-548`.
- **Problem:** `draw()` acquires before the fallible work (import, staging, pipeline). On error the
  image is never presented and the semaphore stays signalled; the DMA-BUF → CPU fallback hits this
  every time it triggers. The next acquire breaks the spec and can block forever (`u64::MAX`).
- **Fix:** prepare before acquiring; on an error after acquiring, still submit a clear pass and
  present; finite acquire timeout.
- **Status:** fixed. `Renderer::prepare` does the work that can fail (staging, import, pipelines)
  before the window takes an image; `submit` only records and submits. Acquire waits at most
  100 ms and skips the frame otherwise (a hidden window whose compositor holds its images no
  longer blocks the present thread). Left: a failed submit after acquire (device lost) still loses
  the image, but then the renderer is broken anyway.

### A8 · Use-after-free when the client exits on Wayland

- **Where:** `apps/client/src/main.rs:478-500`, `apps/client/src/shortcuts.rs:139-143, 189-197`.
- **Problem:** after `run_app` the event loop is gone; `App` drops `window` (the last reference to
  winit's Wayland connection) before `shortcuts`, whose drop then uses the display.
- **Fix:** drop the shortcut lock first (`app.shortcuts = None` after `run_app`, or field order).
- **Status:** fixed. `shortcuts` is the first field of `App`, so it is dropped before the window on
  every path (also the early return when `run_app` fails); the SAFETY comment says why.

### A9 · Keys and mouse buttons stay pressed on the host

- **Where:** `apps/client/src/main.rs:514-520, 583-589, 715-723`, `apps/client/src/lib.rs:857-860,
  1115-1122`, `crates/input/src/uinput.rs:430-435, 543-560`, `web/viewer/src/routes/live.tsx:267-304`.
- **Problem:** releases queued on quit are never sent (the loop stops first): Ctrl+Alt+Shift+Q
  leaves Ctrl, Alt and Shift pressed; Bye is a single datagram; Ctrl+C sends nothing. In gaming
  mode without capture, button releases are swallowed. The host releases buttons on the device that
  moved last, not the one that pressed them. The web viewer has no pointer capture, so a release
  over the toolbar is lost.
- **Fix:** flush input and send Bye a few times on stop; signal handler; forward releases of held
  buttons always; release on the pressing device; `setPointerCapture` in the viewer.
- **Status:** fixed. The client sends what is queued (the releases of closing) before its goodbye,
  sends Bye three times, and `stop` is set and read with Release/Acquire so the queued releases are
  seen; Ctrl+C and SIGTERM end the session through `stop_on_signals`. In gaming mode only the
  capturing press stays local, releases go on (native client and viewer). The host's uinput
  releases each button on the pointer it went down on (`button_pointer`), also in `release_all`.
  The viewer captures the pointer on a press. Tests: `keys_held_when_the_client_closes_are_released_on_the_host`
  (fails without the fix), `ctrl_c_ends_the_client_with_a_goodbye`, `a_button_goes_up_where_it_went_down`,
  `live.test.tsx`.

### A10 · "Zu viele falsche PINs" is never shown

- **Where:** `apps/client-ui/src/lib/api.ts:190-200`, `apps/client/src/lib.rs:278`.
- **Problem:** `"wrong PIN"` matches first as a substring of "too many wrong PINs".
- **Fix:** most specific first now; error codes later (E2).
- **Status:** fixed (order; codes with E2). Test in `api.test.ts`.

### A11 · The web viewer's mute button does nothing

- **Where:** `web/viewer/src/routes/live.tsx:106-135`, `web/ui/src/components/session-view.tsx:76, 177-188`.
- **Fix:** pass `onMuteChange` and mute the video element.
- **Status:** fixed. The page keeps the state and mutes the `<video>`. Test in `live.test.tsx`.

### A12 · One invalid input event blocks all input of the session

- **Where:** `apps/client/src/lib.rs:475-491` (`parse_keys_command`), `crates/proto/src/lib.rs:817-822,
  886-893`, `crates/input/src/queue.rs:177-199`.
- **Problem:** the keys command accepts the mouse button codes 0x110–0x117 as keys; the host rejects
  the whole packet, the client resends it forever and everything behind it waits.
- **Fix:** validate in `InputQueue::push` with the protocol's rules.
- **Status:** fixed. `InputEvent::validate` in `proto` holds the rules once; the parser applies it,
  `InputQueue::push` drops what it refuses, and the keys command checks with it. A property test
  (`validate_agrees_with_the_parser`) keeps parser and rule the same.

### A13 · Overlapping FEC groups make the client allocate gigabytes

- **Where:** `crates/net/src/reassembly.rs:235-244`, `Group::init` at `:66`.
- **Problem:** groups are checked to lie inside the frame, not to be disjoint or match their index.
  A hostile host (or any LAN machine in plain mode) can make thousands of groups each claim offset 0
  with full recovery buffers.
- **Fix:** enforce the packetizer's layout (`group_offset == index · D · shard`, equal D except the
  last) or cap recovery bytes per slot at the frame length; a test that counts allocations.
- **Status:** fixed, independent of the packetizer's layout (which C4 will change): a group's data
  shards must be shard-aligned and must not overlap another group's (a reused per-slot bitmap), and
  recovery stays bounded by the data it protects, so a slot holds at most about four times its
  frame. Test: `groups_that_claim_the_same_data_are_refused`.

### A14 · A failed session start locks the client out; replaced sessions get no Bye

- **Where:** `apps/host-agent/src/lib.rs:1057-1082, 824-827, 955-962`.
- **Problem:** the handshake timestamp is recorded and the old session stopped before
  `start_session`; if it fails, the client's retransmissions are dropped as replays and the status
  still shows the old session. A replaced client gets no Bye and sees a frozen picture for 5 s.
- **Fix:** record and replace only after a successful start; clear the status on every path; send
  Bye to the replaced client.
- **Status:** fixed. The old session still goes first (screen and encoder may not be had twice),
  but through `replace_session`: the old client gets Bye and the status is cleared. The handshake
  time is noted only after a successful start. A start that fails is answered with the new
  `RejectReason::Unavailable`, so the client stops asking and says why (older clients ignore the
  unknown reason). Tests: `new_peer_replaces_the_session`, `a_session_that_cannot_start_is_refused_with_a_reason`.

### A15 · Error paths in `start_session` leak running threads

- **Where:** `apps/host-agent/src/lib.rs:1325-1391, 1403, 1450`.
- **Problem:** a `?` after the first `spawn_hot` leaves started threads running with
  `running = true`. `overflows_seen` starts at 0 against a counter that spans sessions.
- **Fix:** a guard that stops and joins on error; take the counter's current value as the baseline.
- **Status:** fixed. `health::Starting` collects the threads as they start; dropped without
  `done()` (an early return), it clears `running`, closes the frame slot and joins them. The
  session's `overflows_seen` starts at the counter's current value. Tests in `health.rs`.

### A16 · KMS edge cases

- **Where:** `crates/capture/src/kms.rs:368-386, 157-160, 251-270`, `apps/host-agent/src/lib.rs:1891-1931`.
- **Problem:** one failed vblank wait (DPMS, modeset) switches to the free-running timer for the
  rest of the capture (up to one refresh more latency, judder). The encoder GPU is chosen
  independently of the card driving the monitor. The monitor list is never refreshed on hotplug.
- **Fix:** retry vblank periodically; pick the encoder on the capture card's device; refresh the
  list.
- **Status:** fixed. A failed vblank wait paces by timer for a second, then vblank is tried again.
  `--encoder auto` orders GPUs that drive a monitor first (a card with an enabled connector in
  sysfs). The capture thread rescans the outputs every 2 s (`FrameSource::rescan`, the KMS outputs
  read by `read_outputs`/`screen_info`), so hotplug reaches the client's list and the input
  mapping. Tests: `the_gpu_that_shows_the_monitor_comes_first`, `the_screen_lists_the_monitors_that_are_on`.

### A17 · The Vulkan device is not matched to the decoding or encoding GPU

- **Where:** `crates/gpu/src/device.rs:107-139`, `crates/render/src/vulkan/window.rs:80`,
  `apps/client/src/lib.rs:1218-1226`, `crates/codec/src/nvidia.rs:196-205`.
- **Problem:** Vulkan prefers the discrete GPU, VAAPI uses `renderD128` (often the iGPU): hybrid
  machines import across devices, which fails. NVENC on a host with two GPUs asks for an
  environment variable instead of picking the matching device.
- **Fix:** pick by `VK_EXT_physical_device_drm` (render node) and by device UUID for CUDA.
- **Status:** fixed. `fernsicht_gpu::Prefer` (best kind, a render node, or a UUID) and the pure
  `pick`: the client's window prefers the VAAPI decoder's render node, NVENC asks CUDA for its UUID
  first and takes the Vulkan device with it. Without a match the best kind is used, with a warning.
  Tests: `the_preferred_gpu_wins_over_the_better_kind`, `device_numbers_split_like_glibc`; the
  NVIDIA codec tests pass with it.

### A18 · The VAAPI decoder's export cache is keyed by surface ID only

- **Where:** `crates/codec/src/vaapi.rs:977-979, 1147-1158`, `crates/render/src/vulkan/renderer.rs:1094-1099`.
- **Problem:** if FFmpeg rebuilds its frame pool, surface IDs are reused and old DMA-BUFs (old
  size) are shown. `forget_imports` is never called when a decoder is replaced.
- **Fix:** key on (frames context, surface), clear on change; forget imports on decoder change.
- **Status:** fixed. The decoder holds a reference to the surface pool its exports belong to (so a
  new pool can never reuse the old one's address); a new pool clears the exports and gets a new id
  in the upper 32 bits of `Picture::DmaBuf`'s key. The renderer drops all imports when a key of
  another pool arrives, which also covers a replaced decoder. GPU test:
  `a_new_stream_size_gives_new_pictures` (`crates/codec/tests/vaapi.rs`, AMD runner).

### A19 · A failed DMA-BUF export shows black frames forever

- **Where:** `crates/codec/src/vaapi.rs:1151`, `apps/client/src/lib.rs:1328-1334`.
- **Fix:** fall back to NV12 in `picture()` or report an error that switches to the CPU path.
- **Status:** fixed. A failed export makes the decoder hand out NV12 from then on (with a warning);
  the renderer takes either. Only the GPU can make an export fail, so there is no test for it.

### A20 · The converter's fence is not reset after a timeout

- **Where:** `crates/gpu/src/convert.rs:564-571, 407-409`, `crates/render/src/vulkan/renderer.rs:907`.
- **Problem:** the next submit gets an already signalled fence; the wait returns at once and CUDA
  may read a half-written buffer. The renderer waits forever (`u64::MAX`).
- **Fix:** reset after `wait_idle` on the error path; one shared `submit_and_wait` with a limit.
- **Status:** fixed. `fernsicht_gpu::submit_with` (and `submit_and_wait` without semaphores) is the
  one place that submits and waits: at most `GPU_TIMEOUT` (1 s), and after a timeout it idles the
  device and resets the fence, so the next submit never gets a signalled fence. The renderer uses
  it too instead of waiting forever.

### A21 · A partly failed swapchain creation leaks the swapchain

- **Where:** `crates/render/src/vulkan/window.rs:211-243`.
- **Fix:** build into a local value, clean up on error, then store.
- **Status:** fixed. Views and semaphores are built in one step; if it fails, they and the new
  swapchain are destroyed before the error returns.

### A22 · Audio drift compensation is coarse; latency up to twice the documented value

- **Where:** `apps/client/src/lib.rs:1464-1515`, `crates/audio/src/jitter.rs:82-118`.
- **Problem:** the only corrections are dropping a 5 ms frame above depth 6 and 15 ms of
  concealment after running dry: clicks or dropouts every minute or so, 30–35 ms instead of 15 ms
  when the host's clock is faster. The sink is not prefilled, so the cushion is gone after start.
  `pa_simple_get_latency` returns `u64::MAX` on error and the addition overflows.
- **Fix:** steer a smoothed depth with slight resampling (or crossfaded drop/insert); prefill with
  silence; treat `u64::MAX` as unknown.
- **Status:** fixed. `fernsicht_audio::drift`: `DriftControl` steers the smoothed jitter depth to
  its target, and `stretch` plays a frame one sample shorter or longer (linear interpolation) when
  it strays more than a frame; the old drop-a-frame rule is gone. A simulation test holds the
  buffer within 5–25 ms for ten minutes at ±300 ppm. Prefilling turned out unnecessary: the
  controller restores the cushion the sound server's first buffer takes. `pa_simple_get_latency`'s
  error value counts as 0, and the delay sum saturates. Native PipeWire stays D10.

### A23 · Gamepad mapping misses common pads

- **Where:** `crates/input/src/gamepad.rs:74-99, 289-301`.
- **Problem:** `ABS_Z`/`ABS_RZ` always become triggers; many generic pads put the right stick there.
  The X/Y swap only applies to `xpad`.
- **Fix:** Z/RZ as right stick when RX/RY are missing; never merge two device axes; longer term SDL
  (D9).
- **Status:** fixed. Without RX/RY, Z/RZ are the right stick and BRAKE/GAS the triggers; each
  protocol axis takes the first device axis that has it. The X/Y swap stays `xpad`-only: the
  kernel's gamepad spec puts BTN_X north, and only `xpad` reports it west; there is no evidence of
  other drivers doing so. Tests: `a_generic_pad_has_its_right_stick_on_z`, `one_device_axis_per_protocol_axis`.

### A24 · Gamepad axis coalescing reorders and drops events

- **Where:** `crates/input/src/queue.rs:76-97` (the test at `348-370` locks it in).
- **Problem:** `[DpadX=-1, A↓] + DpadX=0` becomes `[DpadX=0, A↓]`: the d-pad tap vanishes.
- **Fix:** coalesce only when no button event of the same pad follows; never coalesce d-pads.
- **Status:** fixed. `InputQueue::coalesces`: only analog movement (no d-pad, no value at rest on
  either side, so taps of d-pad and digital triggers stay) and only when no button of the same pad
  follows. Test: `taps_and_the_order_of_a_pad_survive_coalescing`.

### A25 · Gamepad focus policy is inconsistent

- **Where:** `apps/client/src/main.rs:804-814`, `crates/input/src/gamepad.rs:320-445`,
  `crates/input/src/queue.rs:128-144`.
- **Problem:** pads drive the host while the window is unfocused (and the local desktop sees the
  same presses), but focus loss releases held pad buttons. Steam's virtual pad doubles input.
- **Fix:** one policy (pads follow focus); skip known virtual pads.
- **Status:** fixed (focus). Pads follow the focus: `pad_events_while_focused` drops their events
  while another window has it, and focus loss still releases them. Skipping Steam's virtual pad is
  left for D9 (SDL knows those devices; guessing their names here would be memory, not a source).
  The 2 s rescan for hotplug stays (inotify is a nicety). Test: `gamepads_follow_the_focus`.

### A26 · A panic in the client thread leaves the window open

- **Where:** `apps/client/src/main.rs:662-666, 841`, `apps/client/src/lib.rs:747-750`.
- **Fix:** send `Done` from a drop guard.
- **Status:** fixed. The client thread holds an `OnDrop` guard that sends `Done`, also while
  unwinding. Test: `a_panicking_session_still_closes_the_window`.

### A27 · IPv6 cannot work

- **Where:** `apps/client/src/lib.rs:218, 667, 171-183`.
- **Problem:** `pair()` and `run_with` bind `0.0.0.0:0`; a v6 address fails.
- **Fix:** bind the family of the resolved address.
- **Status:** fixed for sessions and pairing: `fernsicht_net::socket::connect_udp` resolves the host
  and binds a socket of its family. Discovery stays IPv4 (broadcast; v6 would need multicast).
  Tests: `the_local_side_takes_the_peers_family`, `talks_to_a_v6_host`.

### A28 · The receive-buffer warning is off by a factor of two

- **Where:** `crates/net/src/socket.rs:31-39`.
- **Fix:** compare against twice the requested size; as root use `SO_RCVBUFFORCE`/`SO_SNDBUFFORCE`.
- **Status:** fixed. `usable` halves what Linux reports before the comparison; with CAP_NET_ADMIN
  (the host service) the buffers are set with `SO_RCVBUFFORCE`/`SO_SNDBUFFORCE`, past `rmem_max`.
  Test: `the_usable_buffer_is_half_what_linux_reports`.

### A29 · Receiver statistics are skewed

- **Where:** `crates/net/src/reassembly.rs:206-210, 381-386`.
- **Problem:** `highest_frame_id` can go backwards; lost packets are counted ~8 frames late while
  received ones count at once; frames lost completely never count.
- **Fix:** count on eviction consistently; monotonic highest ID.
- **Status:** fixed. The highest frame ID only moves forward. Frames of a gap that never arrived
  count their packets as lost (like the last complete frame, at most `MAX_IN_FLIGHT` frames, so a
  blackout does not read as heavy random loss); partly received frames book theirs when retired,
  which happens when the next frame completes, so the skew is small. The duplicate bookkeeping
  (`needs_keyframe` vs `RefChain`, two loss filters) is clean-up for E1. Test:
  `frames_that_never_arrived_count_their_packets_as_lost`.

### A30 · The app polls processes and can freeze the UI

- **Where:** `apps/client-ui/src/lib/api.ts:97-103`, `apps/desktop/src/share.rs:132-150`,
  `apps/desktop/src/main.rs:82-85`, `apps/desktop/src/backend.rs:340-343`.
- **Problem:** every 3 s `systemctl` plus up to two `--version` processes; the synchronous `session`
  command waits on a lock held while the old client stops (up to 3 s).
- **Fix:** cache versions; make `session` async; stop the old client outside the lock.
- **Status:** fixed. `version_of` runs a program again only when its file changed; `connect` stops
  the old client outside the lock (and one that raced it after storing); `session` runs off the main
  thread like the other commands.

### A31 · Overlay values are mislabelled or dropped

- **Where:** `web/viewer/src/lib/host.ts:208-229`, `web/ui/src/components/latency-overlay.tsx:6, 50`.
- **Problem:** in the web viewer "Anzeige" is the jitter buffer delay, "Verlust (FEC)" comes from
  dropped frames (WebRTC has no FEC here), the codec defaults to "H.264". The app overlay drops the
  p95, max and RTT the client sends.
- **Fix:** `requestVideoFrameCallback` for decode and present times; correct labels; show p95 and RTT.
- **Status:** fixed. The web viewer takes decode and display from `requestVideoFrameCallback`
  (`frameTiming`, `meanTiming`; the WebRTC statistics remain the fallback), shows "Verlust" without
  "(FEC)" and no codec before the host names it. The overlay shows p95, max and RTT where known (the
  app's client already sent them). Tests in `host.test.ts` and `latency-overlay.test.tsx`.

### A32 · The session toolbar cannot be used with the keyboard

- **Where:** `web/viewer/src/routes/live.tsx:291-309`, `web/ui/src/components/menu-button.tsx:26-76`,
  `web/ui/src/components/segmented.tsx:141-160`.
- **Problem:** a window-level handler forwards Tab, Enter, Space and arrows to the host whatever has
  focus; menus lack focus handling and arrow keys.
- **Fix:** forward only when the screen has focus; APG menu and radio group patterns.
- **Status:** fixed. `keysGoToHost` (pure): keys go to the host from the picture, the page and the
  on-screen keyboard's field, not from toolbar, menus, dialogs or other controls. A mouse click on
  the toolbar keeps the focus where it was (typing right after still goes to the host);
  Strg+Alt+T takes the keyboard into the toolbar, Esc gives it back. The menu follows the WAI-ARIA
  menu button pattern (focus into the menu, arrows, Home/End, Esc back to the button); the radio
  group is one tab stop with arrow keys. Tests in `keys.test.ts`, `session-view.test.tsx`,
  `primitives.test.tsx`, `live.test.tsx`.

## B. Security

The cryptography is sound (SPAKE2, Noise IK with separate keys per direction, AEAD with explicit
counters, replay window checked before decryption). The gaps are in the logic around it.

### B1 · The web PIN is also a pairing PIN; a started pairing survives `close_pairing`

- **Where:** `apps/host-agent/src/web.rs:825-844`, `apps/host-agent/src/lib.rs:596, 1132, 1154-1176`.
- **Problem:** the browser sends the PIN in clear over HTTP; the same PIN allows SPAKE2 pairing.
  Step 3 never checks that pairing is still open, and `close_pairing` keeps the exchange. Someone
  who reads the PIN on the LAN can pair a device for good (root input).
- **Fix:** separate the browser session from device pairing; in step 3 require the window still
  open and this exchange to be the one that used the PIN; drop the exchange on close.
- **Status:** fixed for passive listeners; the active attacker remains for D7. Each pairing window
  has a generation; step 3 finishes only if the window it began in is still open, so a browser
  using the PIN (which closes the window) kills a pairing begun with it, and the control loop
  handles web requests before UDP packets, so a listener cannot get step 1 and 3 through first. An
  attacker who intercepts and holds back the browser's HTTP request could still use the PIN: only
  HTTPS or a PAKE in the page closes that (D7). Tests:
  `a_pairing_begun_before_a_browser_used_the_pin_is_refused`, `the_last_allowed_attempt_can_still_pair`.

### B2 · The control socket trusts a "desktop user" any local user can fake

- **Where:** `apps/host-agent/src/control.rs:155-162, 189`, `crates/core/src/desktop.rs:36-68, 97`.
- **Problem:** the desktop user is the lowest uid ≥ 1000 whose `/run/user/<uid>/wayland-0` exists;
  every user owns their own `/run/user/<uid>`. Another local or SSH user can become the allowed uid
  and pair a device.
- **Fix:** ask logind for the active session on seat0.
- **Status:** fixed. As root, the desktop's user is the user of the active session on seat0 from
  `sd_seat_get_active` (libsystemd, loaded at runtime; its state file says "do not parse"), and
  only a regular user (uid ≥ 1000) counts. Files under `/run/user` are not looked at any more.
  Without logind or an active session there is no desktop user (only root and the service may use
  the socket). Tests: `only_regular_users_sit_at_the_desktop`, `logind_names_the_active_user_if_there_is_one`.

### B3 · Weak handshake replay protection; the host streams before key confirmation

- **Where:** `apps/host-agent/src/lib.rs:718-719, 1054-1077, 1316-1382`.
- **Problem:** `sent_ms` is only compared with the last value in memory (lost on restart). A replay
  with a spoofed source replaces the live session and sends ~5 s of video at up to 80 Mbit/s to the
  victim from one 150-byte packet.
- **Fix:** reject `sent_ms` far from the host clock and persist the last value per key; replace the
  session and start sending only after the first authenticated packet from the client.
- **Status:** fixed, in part differently: the link of a secure session sends nothing until the
  client's first sealed packet (no amplification; the stream starts with a keyframe), and the last
  handshake time per device is kept in `handshakes.json` next to the paired list (reset by pairing
  again). A window against the host clock was left out: it would lock out clients whose clock is
  off, and the persisted times already cover restarts. A live session can still be replaced by a
  handshake captured undelivered and replayed before the device's next connection. Tests:
  `nothing_is_sent_before_the_client_proves_its_keys`, `handshake_times_outlive_a_restart_and_reset_on_pairing`.

### B4 · Unauthenticated discovery answers rewrite a paired host's address

- **Where:** `apps/client/src/lib.rs:198-210`, `apps/desktop/src/backend.rs:274-277`,
  `apps/client/src/discover.rs:4-6, 121`, `apps/host-agent/src/lib.rs:856-859`.
- **Problem:** any Announce with a paired host's (public) key is stored; the host's announce budget
  is global, so 20 Discover/s silence the real host.
- **Fix:** store an address only after a successful handshake; budget per source.
- **Status:** fixed. Discovery answers are shown and tried first (the app keeps them in memory),
  never stored; the client stores a host's address after a session with it went through
  (`remember_address`). The host's answers are limited per asking address (20/s) and in total
  (200/s) with `limits::PerSource`. Tests: `an_address_is_kept_only_after_a_session_with_it`,
  `one_flooding_source_does_not_silence_the_others` and the other `limits.rs` tests.

### B5 · Unpairing does not end a running session

- **Where:** `apps/host-agent/src/lib.rs:613-630, 1045`, `apps/host-agent/src/control.rs:123-127`.
- **Fix:** unpair ends the session of that key; a control command to end the current session.
- **Status:** open

### B6 · The web server can exhaust memory and threads of the root service

- **Where:** `apps/host-agent/src/web.rs:635-653`, tiny_http 0.12 (no release since 2022).
- **Problem:** unlimited header line length and count, no socket timeouts, a thread per connection.
- **Fix:** limits and timeouts (or a maintained server), bounded handlers.
- **Status:** open

### B7 · The client acts on unauthenticated packets

- **Where:** `apps/client/src/lib.rs:969, 991-1005`.
- **Problem:** a spoofed Reject ends a running session; the first Handshake reply consumes the
  initiator; any datagram resets the timeout.
- **Fix:** Reject only before the session; keep the initiator until a reply verifies; reset the
  timeout only on authenticated packets.
- **Status:** open

### B8 · Handshakes are processed without a rate limit

- **Where:** `apps/host-agent/src/lib.rs:1038-1052`.
- **Fix:** token buckets per source and global; rate-limited logging.
- **Status:** open

### B9 · Anyone can use up the pairing attempts; no CSRF protection

- **Where:** `apps/host-agent/src/lib.rs:1132-1151`, `apps/host-agent/src/web.rs:680-727`.
- **Problem:** an attempt is counted before message 1 is validated; the limit is global;
  `/api/connect` accepts any Content-Type, Origin and Host (any website can spend the attempts,
  DNS rebinding can read `/api/info`); no security headers.
- **Fix:** validate before counting; JSON content type and Origin/Host check; CSP, frame and
  nosniff headers.
- **Status:** open

### B10 · The service unit is barely hardened; the firewall opens in the default zone

- **Where:** `packaging/fernsicht-host.service`, `packaging/install-host.sh:62-73`,
  `apps/desktop/src/share.rs:79-88`.
- **Fix:** `CapabilityBoundingSet`, `DevicePolicy=closed` with `DeviceAllow`, `ProtectSystem=strict`,
  `ProtectHome=read-only`, `ProtectKernel*`, `RestrictNamespaces`, `SystemCallFilter`, `UMask=0077`.
- **Status:** open

### B11 · The privileged install from the app

- **Where:** `apps/desktop/src/share.rs:57-89, 154-179`.
- **Problem:** pkexec runs an opaque `/bin/sh -c`; root executes the user-writable AppImage; the
  extracted tree's permissions are not normalised; `pkexec` comes from `PATH`.
- **Fix:** `/usr/bin/pkexec`; normalise ownership and modes; extract without running the image.
- **Status:** open

### B12 · Names chosen by the other side are used as identifiers

- **Where:** `apps/client/src/lib.rs:186-194`, `apps/desktop/src/backend.rs:339`,
  `apps/client/src/main.rs:220, 266`, `apps/host-agent/src/lib.rs:613-619`, `apps/host-agent/src/main.rs:155`.
- **Problem:** a paired host named like another host's address receives that connection; `unpair
  <name>` removes the first match; control characters go straight to terminals.
- **Fix:** pass and match by key; reject control characters in names.
- **Status:** open

### B13 · Key file handling

- **Where:** `crates/secure/src/lib.rs:102-137`, `apps/host-agent/src/main.rs:199-205`.
- **Fix:** random temporary name with `O_EXCL|O_NOFOLLOW`, `fchmod` 0600, state directory 0700,
  warn on group/other access.
- **Status:** open

### B14 · Any local user can block the control socket

- **Where:** `apps/host-agent/src/control.rs:186-223, 52-55`.
- **Problem:** one connection at a time, uid checked after reading, timeout per read; falls back to
  `/tmp/fernsicht/`.
- **Fix:** check the uid first, total deadline, no `/tmp` fallback.
- **Status:** open

## C. Performance

### C1 · Input waits for the network loop

- **Where:** `apps/client/src/lib.rs:671, 891-901`, `RESEND_AFTER`.
- **Problem:** events go out only when the loop wakes (next packet or the 2 ms receive timeout):
  0–2 ms extra per input, 1 kHz mice arrive in 2 ms clumps. Resends wait a fixed 15 ms although the
  LAN round trip is ~0.5 ms.
- **Fix:** send from the input path directly (socket and transport take `&self`); resend after a
  multiple of the measured RTT.
- **Status:** open

### C2 · Keyframe requests wait for the 100 ms feedback tick

- **Where:** `apps/client/src/lib.rs:926-952`, `apps/host-agent/src/lib.rs:1467-1469, 2080-2082`,
  `apps/host-agent/src/web.rs:340-366`.
- **Problem:** ~50 ms on average before the host even knows; requests (and browser PLIs) are not
  de-duplicated, so lossy Wi-Fi can cause keyframe storms.
- **Fix:** send an immediate request on gap detection (rate-limited); ignore requests within
  RTT + 2 frames of the last keyframe; later NACK and retransmit for keyframes in the LAN.
- **Status:** open

### C3 · Fixed-rate pacing adds latency on cable and paces large frames least

- **Where:** `crates/net/src/pacer.rs:20-27, 46-48`, `apps/host-agent/src/lib.rs:1370-1373, 2132`.
- **Problem:** at 400 Mbit/s a 1080p P-frame takes ~0.5 ms longer than at line rate, 4K ~2 ms;
  frames above ~416 kB go out faster than the pacing rate. Recovery shards of earlier groups go
  before data of later groups.
- **Fix:** token bucket with a burst allowance; rate from measured capacity; data first, parity last.
- **Status:** open

### C4 · The FEC group layout is weak against bursts

- **Where:** `crates/net/src/fec.rs:25-27, 42, 185-234`.
- **Problem:** at most 64 data shards per group, filled in order: a burst of 8 losses in one group
  kills a 300 kB keyframe that one group with the same overhead would survive. The comment "smaller
  groups isolate burst loss" is backwards.
- **Fix:** one group per frame up to 512 shards (or interleaved shards); balanced groups.
- **Status:** open

### C5 · Every bitrate change rebuilds the hardware encoder

- **Where:** `apps/host-agent/src/lib.rs:2067-2079`, `crates/codec/src/vaapi.rs:261, 467-470`,
  `crates/codec/src/nvidia.rs:194-217, 365-367`.
- **Problem:** NVENC: new CUDA context, Vulkan device, shader compile and import (estimate: hundreds
  of ms without picture); VAAPI: new display, pool, context and filter graph. Plus a keyframe, up to
  once a second, exactly when the network is congested.
- **Fix:** NVENC: change `bit_rate`/`rc_max_rate`/`rc_buffer_size` on the open context (FFmpeg
  reconfigures); at least keep the GPU objects across a rebuild.
- **Status:** open

### C6 · NVDEC pictures go through the CPU

- **Where:** `crates/codec/src/nvidia.rs:527-540, 591-606`, `crates/codec/src/ff.rs:168-184`,
  `crates/render/src/vulkan/renderer.rs:423-486`.
- **Problem:** download, copy into a `Vec`, copy into staging, upload: ~0.8 ms at 1080p, ~3 ms at 4K
  (estimate).
- **Fix:** CUDA → Vulkan interop (the existing convert path in reverse); interim: download straight
  into the mapped staging buffer.
- **Status:** open

### C7 · The NVENC input path waits on the CPU twice

- **Where:** `crates/gpu/src/convert.rs:540, 564`, `crates/codec/src/cuda.rs:286-288`,
  `crates/codec/src/nvidia.rs:248-269`.
- **Fix:** external semaphores between Vulkan and CUDA; no `cuStreamSynchronize`; possibly no copy.
- **Status:** open

### C8 · The VAAPI encoder imports the screen buffer anew every frame

- **Where:** `crates/codec/src/vaapi.rs:675-817`, `docs/video.md` ("imports are cached").
- **Fix:** cache imported surfaces by (device, inode, layout) like `gpu::convert`.
- **Status:** open

### C9 · About 30 DRM ioctls per frame; pointer work before the frame is handed on

- **Where:** `crates/capture/src/kms.rs:303-361, 416-423, 471-530`, `apps/host-agent/src/lib.rs:1525-1533`.
- **Fix:** resolve property IDs once; read X and Y in one call; hand the frame on before the pointer.
- **Status:** open

### C10 · One syscall per packet, a spinning pacer, avoidable copies

- **Where:** `apps/host-agent/src/lib.rs:471-487`, `crates/net/src/pacer.rs:79-93`,
  `apps/client/src/lib.rs:1095-1096`.
- **Problem:** the send thread spins for the whole frame (6 % of a core at 1080p60, 23 % at 4K);
  `send_to` on an unconnected socket; full-frame copies on both sides.
- **Fix:** connected socket, UDP GSO (`UDP_SEGMENT`) or `SO_TXTIME`; `recvmmsg` on the client; seal
  in place; swap buffers instead of copying.
- **Status:** open

### C11 · No DSCP/WMM or ECN marking

- **Fix:** video AF41 (WMM video), input, audio, pointer and feedback EF (WMM voice); later ECN as
  the signal for C/D3.
- **Status:** open

### C12 · The web viewer sends input on a reliable, ordered channel

- **Where:** `web/viewer/src/lib/host.ts:94`, `apps/host-agent/src/web.rs:345-346`,
  `apps/host-agent/src/lib.rs:1651-1653, 1718-1728`.
- **Problem:** one lost SCTP packet stalls every later move, key and pointer position.
- **Fix:** a second unordered channel without retransmits for moves and pointer positions.
- **Status:** open

### C13 · The web viewer's pointer re-renders React every frame

- **Where:** `web/viewer/src/routes/live.tsx:183-196, 326-346`.
- **Fix:** skip unchanged positions; move the canvas through a ref; cache the rect; skip known shapes.
- **Status:** open

### C14 · Pointer lock is not tuned for games

- **Where:** `web/viewer/src/routes/live.tsx:255-279`.
- **Fix:** `requestPointerLock({ unadjustedMovement: true })` with fallback and `.catch()`;
  `pointerrawupdate` where available.
- **Status:** open

### C15 · Unchanged screens are encoded every refresh

- **Where:** `crates/capture/src/kms.rs:671-685`.
- **Fix:** skip when framebuffer and buffer are unchanged; a slow refresh frame for quality.
- **Status:** open

### C16 · Thread priorities are weak and miss the input path

- **Where:** `crates/core/src/thread.rs:8-24`, `apps/host-agent/src/lib.rs:712`, `apps/host-agent/src/web.rs:203`.
- **Fix:** SCHED_FIFO (low priority) or RR for capture, send and control as root; timer slack 1 ns;
  raise the WebRTC thread.
- **Status:** open

### C17 · "Glass-to-glass" ends at `vkQueuePresentKHR`

- **Where:** `apps/client/src/lib.rs:1336-1340`, `crates/core/src/latency.rs:68`,
  `crates/render/src/vulkan/renderer.rs:907`.
- **Problem:** the compositor and the wait for the refresh are missing, so "show immediately" can't
  be measured and the comparison with Sunshine is skewed. The fence wait delays the present.
- **Fix:** `VK_KHR_present_id` + `VK_KHR_present_wait`; present right after submit.
- **Status:** open

### C18 · Allocations on hot paths

- **Where:** `apps/client/src/lib.rs:1043-1048`, `crates/audio/src/jitter.rs:60`,
  `crates/input/src/gamepad.rs:135`, `crates/input/src/queue.rs:189-197`, `crates/input/src/uinput.rs:225`.
- **Problem:** audio copies four times per 5 ms packet (~800 allocations/s); `architecture.md` and
  `performance.md` say nothing allocates.
- **Fix:** fixed buffers moved into the jitter buffer; arrays elsewhere.
- **Status:** open

### C19 · Packet size on VPN tunnels

- **Problem:** 1,400-byte datagrams fragment on WireGuard (1420) and Tailscale (1280).
- **Fix:** shard size from `IP_MTU` (the reassembler already takes it from each packet).
- **Status:** open

### C20 · Clock sync without skew

- **Where:** `crates/net/src/clock_sync.rs`.
- **Fix:** kernel receive timestamps (`SO_TIMESTAMPNS`); fit offset and skew.
- **Status:** open

## D. Strategic steps

| ID | Step | Why |
|---|---|---|
| D1 | **Vulkan Video** encode and decode instead of FFmpeg | One path for AMD and NVIDIA, no CUDA hop, no NVDEC copy; the app manages references: reference frame invalidation instead of keyframes, intra refresh, slices |
| D2 | NVENC `intra-refresh` and `ldkfs` until then | No keyframe bursts after loss on NVIDIA hosts |
| D3 | Delay- or ECN-based congestion control (L4S) | Reacts before packets are lost |
| D4 | HDR and 10 bit (P010, HEVC Main10, AV1 10 bit) | AB30 is cut to 8 bit and tagged sRGB today: HDR desktops look washed out |
| D5 | Signal chroma siting | Colour fringes on text in browsers |
| D6 | Local hardware pointer in desktop mode | The pointer feels free of latency |
| D7 | HTTPS for the web viewer | Keyboard Lock, `pointerrawupdate`, clipboard and wake lock need a secure context; Keyboard Lock does **not** work over HTTP today |
| D8 | Capture on commit; align capture to the client's refresh | 1–3 ms (plausible) |
| D9 | SDL3 for gamepads | Mapping database, rumble, gyro, touchpad |
| D10 | Native PipeWire audio with adaptive resampling | Smaller buffers, no drift clicks |

Status of all D items: open.

## E. Clean code and consistency

| ID | Finding | Where | Status |
|---|---|---|---|
| E1 | Files too large: host `lib.rs` 2,450 lines, client `lib.rs` 1,800, a 410-line window module in client `main.rs`; the `Link` abstraction leaks; native and web send loops duplicate statistics | `apps/host-agent/src/lib.rs`, `apps/client/src` | open |
| E2 | Untyped contracts: overlay JSON by hand (`json.pop()`), errors matched by substring, hand-mirrored TypeScript types, data channel messages cast | `apps/client/src/lib.rs:1378`, `crates/render/src/overlay.rs:91`, `apps/client-ui/src/lib/api.ts`, `web/ui/src/types.ts` | open |
| E3 | Duplicated Rust code: `spirv()` 2× (and naga at runtime instead of `build.rs`), `err()` 3×, `hostname()` 2×, `Hw` built twice, audio summary twice, own submit-and-wait in the renderer | `crates/gpu`, `crates/render`, `apps/client` | open |
| E4 | Duplicated frontend code: PIN field, device ID field, mode list, "session ended" screen, mode search validator, two error tables, `formatDeviceId` | `web/viewer/src/routes/connect.tsx`, `apps/client-ui/src/routes/devices.tsx` | open |
| E5 | Stale comments: "phase 3" (tag, bitrate), "until the VAAPI backend lands", "H.264 only" in codec docs and features, NVENC "test pattern only", "i.e. VAAPI", `DecodedFrame` "will hand out a texture", "one lost frame per hour", rate test says 10 % for 30 %, `HostDescription` hard-codes "· H.264" | many | open |
| E6 | Docs vs code: `video.md` import cache, "nothing allocates", `app.md` connect dialog and search placeholder, `--app` help misses `keys` and `monitor`, `command-line.md` misses `discover --wait` and `pair --name`, web viewer Keyboard Lock over HTTP | `docs/` | open |
| E7 | Inert and dead UI and code: toolbar clipboard, files and settings buttons; favourites can't be set; `unpair_from_host` unused; `HostInfo.pairing`, `resetZoom`, `Slot::take_timeout`, `Slot::is_closed`, `CaptureError::Stopped`, `InputQueue::held` unused; fullscreen button on iPhone | various | open |
| E8 | Client terminal output still German; `--height`/`--fps` without help; `0.005` instead of `FRAME_MS` | `apps/client/src/main.rs` | open |
| E9 | Unknown flag bits ignored by some packets and rejected by others; `FecConfig.redundancy` not validated | `crates/proto`, `crates/net/src/fec.rs` | open |
| E10 | CI: actions on v4 (latest v7, Node 20 deprecated), not pinned by SHA, no `permissions` in `ci.yml`, `appimagetool` from `continuous` without checksum, runs twice for PRs, no path filter between Rust and web, coverage repeats the tests, UIs built three times, Playwright browsers not cached | `.github/workflows`, `packaging/appimage.sh` | open |
| E11 | Dependencies: Playwright 1.56 (1.64), Tauri crate 2.11 vs npm 2.12, `libloading` 0.8 (0.9), resolver 2 (3), MSRV not checked, no JS linter (react-hooks rules unchecked) | `package.json`, `Cargo.toml` | open |
| E12 | Naming: `fernsicht-host-agent` vs `fernsicht-host.service`; `apps/client-ui` vs `web/*`; "Windows-Taste" on a Linux host | various | open |
| E13 | Classes in the web viewer against the functional style (`CLAUDE.md`): `CursorAssembler`, `TouchController`, `PadTracker`, `WheelAccumulator` as pure step functions or closures; `ConnectError` as an error value | `web/viewer/src/lib/{cursor,touch,gamepad,input,host}.ts` | open |
