//! Playback through PipeWire's own stream API (libpipewire-0.3, loaded at
//! runtime like libpulse-simple: no build dependency, and without it the
//! client falls back to [`crate::pulse`]).
//!
//! Through pipewire-pulse the sound server held about 21 ms whatever the
//! client asked for: four quanta of 256 samples, its own buffer per
//! stream. A native stream asks for one 5 ms quantum (`node.latency`) and
//! is fed from a ring the client fills one frame ahead, so the server
//! holds about one quantum.
//!
//! Struct layouts and constants are PipeWire's (`src/pipewire/stream.h`,
//! `spa/include/spa/buffer/buffer.h`, `spa/include/spa/pod/pod.h`,
//! `spa/include/spa/param/{format.h,audio/raw.h}`). The format parameter is
//! a SPA pod; PipeWire builds those with inline header functions, so it
//! is serialized here ([`format_pod`]).

use std::collections::VecDeque;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::{CHANNELS, FRAME_SAMPLES, SAMPLE_RATE};

// spa/include/spa/utils/type.h, param/param.h, param/format.h, param/audio/raw.h
const SPA_TYPE_ID: u32 = 3;
const SPA_TYPE_INT: u32 = 4;
const SPA_TYPE_ARRAY: u32 = 13;
const SPA_TYPE_OBJECT: u32 = 15;
const SPA_TYPE_OBJECT_FORMAT: u32 = 0x40003;
const SPA_PARAM_ENUM_FORMAT: u32 = 3;
const SPA_FORMAT_MEDIA_TYPE: u32 = 1;
const SPA_FORMAT_MEDIA_SUBTYPE: u32 = 2;
const SPA_FORMAT_AUDIO_FORMAT: u32 = 0x10001;
const SPA_FORMAT_AUDIO_RATE: u32 = 0x10003;
const SPA_FORMAT_AUDIO_CHANNELS: u32 = 0x10004;
const SPA_FORMAT_AUDIO_POSITION: u32 = 0x10005;
const SPA_MEDIA_TYPE_AUDIO: u32 = 1;
const SPA_MEDIA_SUBTYPE_RAW: u32 = 1;
const SPA_AUDIO_FORMAT_S16_LE: u32 = 0x103;
const SPA_AUDIO_CHANNEL_FL: u32 = 3;
const SPA_AUDIO_CHANNEL_FR: u32 = 4;

// src/pipewire/stream.h, spa/include/spa/utils/defs.h
const SPA_DIRECTION_OUTPUT: c_int = 1;
const PW_ID_ANY: u32 = u32::MAX;
const PW_STREAM_FLAG_AUTOCONNECT: c_int = 1 << 0;
const PW_STREAM_FLAG_MAP_BUFFERS: c_int = 1 << 2;
const PW_STREAM_FLAG_RT_PROCESS: c_int = 1 << 4;
const PW_STREAM_STATE_ERROR: c_int = -1;
const PW_STREAM_STATE_PAUSED: c_int = 2;

/// Bytes per sample frame (all channels).
const STRIDE: usize = CHANNELS * 2;

/// The format offered: S16LE, 48 kHz, stereo (FL, FR), as an
/// `SPA_TYPE_OBJECT_Format` pod for `SPA_PARAM_EnumFormat`. Words of 64 bit
/// keep it 8-byte aligned, as pods must be.
pub fn format_pod() -> Vec<u64> {
    /// One property: key, flags, then a value pod (header and body padded
    /// to 8 bytes).
    fn prop(out: &mut Vec<u32>, key: u32, ty: u32, body: &[u32]) {
        out.extend([key, 0, (body.len() * 4) as u32, ty]);
        out.extend(body);
        if body.len() % 2 == 1 {
            out.push(0);
        }
    }
    let mut props = Vec::new();
    prop(
        &mut props,
        SPA_FORMAT_MEDIA_TYPE,
        SPA_TYPE_ID,
        &[SPA_MEDIA_TYPE_AUDIO],
    );
    prop(
        &mut props,
        SPA_FORMAT_MEDIA_SUBTYPE,
        SPA_TYPE_ID,
        &[SPA_MEDIA_SUBTYPE_RAW],
    );
    prop(
        &mut props,
        SPA_FORMAT_AUDIO_FORMAT,
        SPA_TYPE_ID,
        &[SPA_AUDIO_FORMAT_S16_LE],
    );
    prop(
        &mut props,
        SPA_FORMAT_AUDIO_RATE,
        SPA_TYPE_INT,
        &[SAMPLE_RATE],
    );
    prop(
        &mut props,
        SPA_FORMAT_AUDIO_CHANNELS,
        SPA_TYPE_INT,
        &[CHANNELS as u32],
    );
    // An array: its child pod's header (element size and type), then the
    // elements.
    prop(
        &mut props,
        SPA_FORMAT_AUDIO_POSITION,
        SPA_TYPE_ARRAY,
        &[4, SPA_TYPE_ID, SPA_AUDIO_CHANNEL_FL, SPA_AUDIO_CHANNEL_FR],
    );
    let body_bytes = (2 + props.len()) * 4;
    let mut words = vec![
        body_bytes as u32,
        SPA_TYPE_OBJECT,
        SPA_TYPE_OBJECT_FORMAT,
        SPA_PARAM_ENUM_FORMAT,
    ];
    words.extend(props);
    words
        .chunks(2)
        .map(|w| u64::from(w[0]) | u64::from(w.get(1).copied().unwrap_or(0)) << 32)
        .collect()
}

#[repr(C)]
struct SpaChunk {
    offset: u32,
    size: u32,
    stride: i32,
    flags: i32,
}

#[repr(C)]
struct SpaData {
    ty: u32,
    flags: u32,
    fd: i64,
    mapoffset: u32,
    maxsize: u32,
    data: *mut c_void,
    chunk: *mut SpaChunk,
}

#[repr(C)]
struct SpaBuffer {
    n_metas: u32,
    n_datas: u32,
    metas: *mut c_void,
    datas: *mut SpaData,
}

/// `struct pw_buffer`; only the fields read here (it may grow).
#[repr(C)]
struct PwBuffer {
    buffer: *mut SpaBuffer,
    user_data: *mut c_void,
    size: u64,
    /// For playback: the sample frames the graph asks for (0: unknown).
    requested: u64,
}

#[repr(C)]
#[derive(Default)]
struct PwTime {
    now: i64,
    rate_num: u32,
    rate_denom: u32,
    ticks: u64,
    delay: i64,
    queued: u64,
    buffered: u64,
    queued_buffers: u32,
    avail_buffers: u32,
    size: u64,
}

/// `struct pw_stream_events` at version 0 (callbacks up to `drained`).
#[repr(C)]
struct PwStreamEvents {
    version: u32,
    destroy: Option<unsafe extern "C" fn(*mut c_void)>,
    state_changed: Option<unsafe extern "C" fn(*mut c_void, c_int, c_int, *const c_char)>,
    control_info: Option<unsafe extern "C" fn(*mut c_void, u32, *const c_void)>,
    io_changed: Option<unsafe extern "C" fn(*mut c_void, u32, *mut c_void, u32)>,
    param_changed: Option<unsafe extern "C" fn(*mut c_void, u32, *const c_void)>,
    add_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut PwBuffer)>,
    remove_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut PwBuffer)>,
    process: Option<unsafe extern "C" fn(*mut c_void)>,
    drained: Option<unsafe extern "C" fn(*mut c_void)>,
}

static EVENTS: PwStreamEvents = PwStreamEvents {
    version: 0,
    destroy: None,
    state_changed: Some(on_state),
    control_info: None,
    io_changed: None,
    param_changed: None,
    add_buffer: None,
    remove_buffer: None,
    process: Some(on_process),
    drained: None,
};

struct Lib {
    _lib: libloading::Library,
    init: unsafe extern "C" fn(*mut c_int, *mut *mut *mut c_char),
    loop_new: unsafe extern "C" fn(*const c_char, *const c_void) -> *mut c_void,
    loop_get: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    loop_start: unsafe extern "C" fn(*mut c_void) -> c_int,
    loop_stop: unsafe extern "C" fn(*mut c_void),
    loop_destroy: unsafe extern "C" fn(*mut c_void),
    loop_lock: unsafe extern "C" fn(*mut c_void),
    loop_unlock: unsafe extern "C" fn(*mut c_void),
    props_new: unsafe extern "C" fn(*const c_char) -> *mut c_void,
    stream_new: unsafe extern "C" fn(
        *mut c_void,
        *const c_char,
        *mut c_void,
        *const PwStreamEvents,
        *mut c_void,
    ) -> *mut c_void,
    stream_connect:
        unsafe extern "C" fn(*mut c_void, c_int, u32, c_int, *const *const c_void, u32) -> c_int,
    stream_destroy: unsafe extern "C" fn(*mut c_void),
    dequeue: unsafe extern "C" fn(*mut c_void) -> *mut PwBuffer,
    queue: unsafe extern "C" fn(*mut c_void, *mut PwBuffer) -> c_int,
    get_time: unsafe extern "C" fn(*mut c_void, *mut PwTime, usize) -> c_int,
}

fn lib() -> Result<&'static Lib, String> {
    static LIB: OnceLock<Result<Lib, String>> = OnceLock::new();
    LIB.get_or_init(|| {
        // SAFETY: plain C library; symbols used with their signatures in
        // PipeWire's headers. pw_init runs once, before anything else.
        unsafe {
            let lib = libloading::Library::new("libpipewire-0.3.so.0")
                .map_err(|e| format!("libpipewire not found: {e}"))?;
            macro_rules! sym {
                ($name:literal) => {
                    *lib.get($name)
                        .map_err(|e| format!("libpipewire lacks a function: {e}"))?
                };
            }
            let l = Lib {
                init: sym!(b"pw_init"),
                loop_new: sym!(b"pw_thread_loop_new"),
                loop_get: sym!(b"pw_thread_loop_get_loop"),
                loop_start: sym!(b"pw_thread_loop_start"),
                loop_stop: sym!(b"pw_thread_loop_stop"),
                loop_destroy: sym!(b"pw_thread_loop_destroy"),
                loop_lock: sym!(b"pw_thread_loop_lock"),
                loop_unlock: sym!(b"pw_thread_loop_unlock"),
                props_new: sym!(b"pw_properties_new_string"),
                stream_new: sym!(b"pw_stream_new_simple"),
                stream_connect: sym!(b"pw_stream_connect"),
                stream_destroy: sym!(b"pw_stream_destroy"),
                dequeue: sym!(b"pw_stream_dequeue_buffer"),
                queue: sym!(b"pw_stream_queue_buffer"),
                get_time: sym!(b"pw_stream_get_time_n"),
                _lib: lib,
            };
            (l.init)(std::ptr::null_mut(), std::ptr::null_mut());
            Ok(l)
        }
    })
    .as_ref()
    .map_err(Clone::clone)
}

/// What the client fills and PipeWire's realtime thread drains.
struct Ring {
    lib: &'static Lib,
    samples: Mutex<VecDeque<i16>>,
    room: Condvar,
    /// `enum pw_stream_state`, from the state callback.
    state: AtomicI32,
    /// Sample frames the graph asked for in its last cycle.
    requested: AtomicU64,
    /// Sample frames played as silence because the ring ran dry.
    starved: AtomicU64,
    stream: std::sync::atomic::AtomicPtr<c_void>,
}

/// The graph asks for sound: hand over what the ring has, silence for the
/// rest (an underrun the client's drift control makes up for).
unsafe extern "C" fn on_process(data: *mut c_void) {
    // SAFETY: data is the Ring, alive while the stream is; the buffer and
    // its memory come from PipeWire and are ours until queued.
    unsafe {
        let ring = &*data.cast::<Ring>();
        let stream = ring.stream.load(Ordering::Acquire);
        if stream.is_null() {
            return;
        }
        let b = (ring.lib.dequeue)(stream);
        if b.is_null() {
            return;
        }
        let sb = &*(*b).buffer;
        if sb.n_datas == 0 || (*sb.datas).data.is_null() {
            (ring.lib.queue)(stream, b);
            return;
        }
        let d = &mut *sb.datas;
        let room = d.maxsize as usize / STRIDE;
        let frames = match (*b).requested as usize {
            0 => room.min(FRAME_SAMPLES),
            n => n.min(room),
        };
        ring.requested.store(frames as u64, Ordering::Relaxed);
        let out = std::slice::from_raw_parts_mut(d.data.cast::<i16>(), frames * CHANNELS);
        let have = match ring.samples.lock() {
            Ok(mut samples) => {
                let have = samples.len().min(out.len());
                for (o, s) in out.iter_mut().zip(samples.drain(..have)) {
                    *o = s;
                }
                have
            }
            Err(_) => 0,
        };
        out[have..].fill(0);
        if have < out.len() {
            ring.starved
                .fetch_add(((out.len() - have) / CHANNELS) as u64, Ordering::Relaxed);
        }
        ring.room.notify_one();
        let chunk = &mut *d.chunk;
        chunk.offset = 0;
        chunk.size = (frames * STRIDE) as u32;
        chunk.stride = STRIDE as i32;
        (ring.lib.queue)(stream, b);
    }
}

unsafe extern "C" fn on_state(data: *mut c_void, _old: c_int, state: c_int, error: *const c_char) {
    // SAFETY: data is the Ring, alive while the stream is.
    let ring = unsafe { &*data.cast::<Ring>() };
    ring.state.store(state, Ordering::Release);
    if !error.is_null() {
        // SAFETY: a C string from PipeWire.
        let error = unsafe { CStr::from_ptr(error) };
        log::warn!("PipeWire playback: {}", error.to_string_lossy());
    }
    ring.room.notify_one();
}

/// How long `open` waits for the stream to connect.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// How long `play` waits for room before the stream counts as stuck.
const STALL: Duration = Duration::from_millis(500);

/// Playback on the default output through a native PipeWire stream.
pub struct Playback {
    lib: &'static Lib,
    thread_loop: *mut c_void,
    stream: *mut c_void,
    ring: Box<Ring>,
    /// Samples (all channels) the ring may hold before `play` waits.
    ahead: usize,
}

// SAFETY: the PipeWire objects are touched under the thread loop's lock
// (or by its own thread); the ring is shared through a mutex.
unsafe impl Send for Playback {}

impl Playback {
    /// `ahead_frames`: 5 ms frames the client keeps in the ring before the
    /// graph takes them (the rest of the delay is its jitter buffer).
    pub fn open(ahead_frames: usize) -> Result<Self, String> {
        let lib = lib()?;
        let props = format!(
            "media.type=Audio media.category=Playback media.role=Game \
             node.name=fernsicht node.description=Fernsicht application.name=Fernsicht \
             node.latency={FRAME_SAMPLES}/{SAMPLE_RATE}"
        );
        let props = std::ffi::CString::new(props).expect("no NUL");
        let pod = format_pod();
        // SAFETY: the calls follow PipeWire's API: objects are created on
        // the thread loop and changed under its lock; the ring outlives the
        // stream (destroyed in Drop first).
        unsafe {
            let thread_loop = (lib.loop_new)(c"fernsicht-sound".as_ptr(), std::ptr::null());
            if thread_loop.is_null() {
                return Err("PipeWire: no thread loop".into());
            }
            let mut p = Playback {
                lib,
                thread_loop,
                stream: std::ptr::null_mut(),
                ring: Box::new(Ring {
                    lib,
                    samples: Mutex::new(VecDeque::with_capacity(FRAME_SAMPLES * CHANNELS * 8)),
                    room: Condvar::new(),
                    state: AtomicI32::new(0),
                    requested: AtomicU64::new(0),
                    starved: AtomicU64::new(0),
                    stream: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
                }),
                ahead: ahead_frames.max(1) * FRAME_SAMPLES * CHANNELS,
            };
            if (lib.loop_start)(thread_loop) < 0 {
                return Err("PipeWire: thread loop did not start".into());
            }
            (lib.loop_lock)(thread_loop);
            let stream = (lib.stream_new)(
                (lib.loop_get)(thread_loop),
                c"remote sound".as_ptr(),
                (lib.props_new)(props.as_ptr()),
                &EVENTS,
                std::ptr::from_ref::<Ring>(&p.ring).cast_mut().cast(),
            );
            if stream.is_null() {
                (lib.loop_unlock)(thread_loop);
                return Err("PipeWire: no stream".into());
            }
            p.stream = stream;
            p.ring.stream.store(stream, Ordering::Release);
            let params = [pod.as_ptr().cast::<c_void>()];
            let r = (lib.stream_connect)(
                stream,
                SPA_DIRECTION_OUTPUT,
                PW_ID_ANY,
                PW_STREAM_FLAG_AUTOCONNECT | PW_STREAM_FLAG_MAP_BUFFERS | PW_STREAM_FLAG_RT_PROCESS,
                params.as_ptr(),
                1,
            );
            (lib.loop_unlock)(thread_loop);
            if r < 0 {
                return Err(format!("PipeWire: connect failed ({r})"));
            }
            p.wait_connected()?;
            Ok(p)
        }
    }

    /// Until the stream is linked (paused or streaming), or failed.
    fn wait_connected(&self) -> Result<(), String> {
        let until = Instant::now() + CONNECT_TIMEOUT;
        let mut samples = self.ring.samples.lock().map_err(|e| e.to_string())?;
        loop {
            match self.ring.state.load(Ordering::Acquire) {
                PW_STREAM_STATE_ERROR => return Err("PipeWire: the stream failed".into()),
                s if s >= PW_STREAM_STATE_PAUSED => return Ok(()),
                _ => {}
            }
            let now = Instant::now();
            if now >= until {
                return Err("PipeWire: the stream did not connect".into());
            }
            samples = self
                .ring
                .room
                .wait_timeout(samples, until - now)
                .map_err(|e| e.to_string())?
                .0;
        }
    }
}

impl Playback {
    /// Sample frames the graph asks for per cycle (its quantum), and the
    /// sample frames played as silence because the ring ran dry.
    pub fn graph_stats(&self) -> (u64, u64) {
        (
            self.ring.requested.load(Ordering::Relaxed),
            self.ring.starved.load(Ordering::Relaxed),
        )
    }
}

impl crate::AudioSink for Playback {
    fn play(&mut self, pcm: &[i16]) -> Result<(), String> {
        let mut samples = self.ring.samples.lock().map_err(|e| e.to_string())?;
        let stalled = Instant::now() + STALL;
        // The graph drains the ring: wait until it is below the target, so
        // the output paces the caller.
        while samples.len() >= self.ahead {
            if self.ring.state.load(Ordering::Acquire) == PW_STREAM_STATE_ERROR {
                return Err("PipeWire: the stream failed".into());
            }
            let now = Instant::now();
            if now >= stalled {
                return Err("PipeWire: the stream takes no sound".into());
            }
            samples = self
                .ring
                .room
                .wait_timeout(samples, stalled - now)
                .map_err(|e| e.to_string())?
                .0;
        }
        samples.extend(pcm);
        Ok(())
    }

    fn buffered_us(&mut self) -> u64 {
        let queued = self.ring.samples.lock().map_or(0, |s| s.len()) / CHANNELS;
        let mut t = PwTime::default();
        // SAFETY: valid stream; the size tells PipeWire which fields we know.
        let r = unsafe { (self.lib.get_time)(self.stream, &mut t, size_of::<PwTime>()) };
        let graph_us = if r < 0 || t.rate_denom == 0 {
            0
        } else {
            // delay counts in rate units (rate_num / rate_denom seconds).
            (t.delay.max(0) as u64 * u64::from(t.rate_num) * 1_000_000) / u64::from(t.rate_denom)
                + t.buffered * 1_000_000 / u64::from(SAMPLE_RATE)
        };
        graph_us + queued as u64 * 1_000_000 / u64::from(SAMPLE_RATE)
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        // SAFETY: as in open; the stream goes before the ring (Box, dropped
        // after this) and the loop.
        unsafe {
            (self.lib.loop_lock)(self.thread_loop);
            self.ring
                .stream
                .store(std::ptr::null_mut(), Ordering::Release);
            if !self.stream.is_null() {
                (self.lib.stream_destroy)(self.stream);
            }
            (self.lib.loop_unlock)(self.thread_loop);
            (self.lib.loop_stop)(self.thread_loop);
            (self.lib.loop_destroy)(self.thread_loop);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_layouts_match_pipewire() {
        assert_eq!(size_of::<SpaChunk>(), 16);
        assert_eq!(size_of::<SpaData>(), 40);
        assert_eq!(size_of::<SpaBuffer>(), 24);
        assert_eq!(std::mem::offset_of!(PwBuffer, requested), 24);
        assert_eq!(size_of::<PwTime>(), 64);
        // version, padding, nine callbacks.
        assert_eq!(size_of::<PwStreamEvents>(), 8 + 9 * 8);
    }

    #[test]
    fn the_format_pod_is_an_audio_raw_object() {
        let words: Vec<u32> = format_pod()
            .iter()
            .flat_map(|w| [*w as u32, (*w >> 32) as u32])
            .collect();
        let bytes = words.len() * 4;
        // Header: body size (all but the 8-byte header), Object.
        assert_eq!(words[0] as usize, bytes - 8);
        assert_eq!(
            &words[1..4],
            &[
                SPA_TYPE_OBJECT,
                SPA_TYPE_OBJECT_FORMAT,
                SPA_PARAM_ENUM_FORMAT
            ]
        );
        // Properties as spa_format_audio_raw_build writes them: key, flags,
        // value pod (size, type) and the padded body.
        let props: Vec<(u32, u32, Vec<u32>)> = {
            let mut out = Vec::new();
            let mut i = 4;
            while i < words.len() {
                let (key, size, ty) = (words[i], words[i + 2], words[i + 3]);
                let n = size as usize / 4;
                out.push((key, ty, words[i + 4..i + 4 + n].to_vec()));
                i += 4 + n.next_multiple_of(2);
            }
            out
        };
        assert_eq!(
            props,
            vec![
                (SPA_FORMAT_MEDIA_TYPE, SPA_TYPE_ID, vec![1]),
                (SPA_FORMAT_MEDIA_SUBTYPE, SPA_TYPE_ID, vec![1]),
                (SPA_FORMAT_AUDIO_FORMAT, SPA_TYPE_ID, vec![0x103]),
                (SPA_FORMAT_AUDIO_RATE, SPA_TYPE_INT, vec![48_000]),
                (SPA_FORMAT_AUDIO_CHANNELS, SPA_TYPE_INT, vec![2]),
                (
                    SPA_FORMAT_AUDIO_POSITION,
                    SPA_TYPE_ARRAY,
                    vec![4, SPA_TYPE_ID, 3, 4]
                ),
            ]
        );
        // 8 + 8 + 5 × 24 + 32.
        assert_eq!(bytes, 168);
    }
}
