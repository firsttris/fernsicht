//! Capture and playback through the PulseAudio "simple" API, which
//! PipeWire provides (pipewire-pulse). libpulse-simple is loaded at
//! runtime: no build dependency, and without it there is no audio.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::sync::OnceLock;

use crate::{CHANNELS, FRAME_SAMPLES, Frame, SAMPLE_RATE};

const PA_STREAM_PLAYBACK: c_int = 1;
const PA_STREAM_RECORD: c_int = 2;
const PA_SAMPLE_S16LE: c_int = 3;
/// "Default" in pa_buffer_attr.
const DEFAULT: u32 = u32::MAX;

#[repr(C)]
struct SampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}

#[repr(C)]
struct BufferAttr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}

type New = unsafe extern "C" fn(
    *const c_char,
    *const c_char,
    c_int,
    *const c_char,
    *const c_char,
    *const SampleSpec,
    *const c_void,
    *const BufferAttr,
    *mut c_int,
) -> *mut c_void;
type Read = unsafe extern "C" fn(*mut c_void, *mut c_void, usize, *mut c_int) -> c_int;
type Write = unsafe extern "C" fn(*mut c_void, *const c_void, usize, *mut c_int) -> c_int;
type Latency = unsafe extern "C" fn(*mut c_void, *mut c_int) -> u64;
type Free = unsafe extern "C" fn(*mut c_void);
type StrError = unsafe extern "C" fn(c_int) -> *const c_char;

struct Lib {
    _lib: libloading::Library,
    new: New,
    read: Read,
    write: Write,
    latency: Latency,
    free: Free,
    strerror: StrError,
}

fn lib() -> Result<&'static Lib, String> {
    static LIB: OnceLock<Result<Lib, String>> = OnceLock::new();
    LIB.get_or_init(|| {
        // SAFETY: plain C library; symbols used with pulse/simple.h's
        // signatures. pa_strerror comes from libpulse, which
        // libpulse-simple depends on (dlsym searches dependencies).
        unsafe {
            let lib = libloading::Library::new("libpulse-simple.so.0")
                .map_err(|e| format!("libpulse-simple not found: {e}"))?;
            macro_rules! sym {
                ($name:literal) => {
                    *lib.get($name)
                        .map_err(|e| format!("libpulse-simple lacks a function: {e}"))?
                };
            }
            Ok(Lib {
                new: sym!(b"pa_simple_new"),
                read: sym!(b"pa_simple_read"),
                write: sym!(b"pa_simple_write"),
                latency: sym!(b"pa_simple_get_latency"),
                free: sym!(b"pa_simple_free"),
                strerror: sym!(b"pa_strerror"),
                _lib: lib,
            })
        }
    })
    .as_ref()
    .map_err(Clone::clone)
}

fn error(lib: &Lib, what: &str, code: c_int) -> String {
    // SAFETY: pa_strerror returns a static string.
    let msg = unsafe {
        let p = (lib.strerror)(code);
        if p.is_null() {
            String::new()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    };
    format!("{what}: {msg} ({code})")
}

/// The sound server to use. As root (sudo for KMS capture, or a system
/// service) the sound still belongs to the desktop's user: connect to that
/// user's server.
fn server() -> Option<CString> {
    if let Ok(s) = std::env::var("PULSE_SERVER") {
        return CString::new(s).ok();
    }
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return None; // our own session's default
    }
    let user = fernsicht_core::desktop::desktop_user()?;
    CString::new(format!(
        "unix:{}/pulse/native",
        user.runtime_dir().display()
    ))
    .ok()
}

struct Stream {
    lib: &'static Lib,
    s: *mut c_void,
}

// SAFETY: a pa_simple handle is used by one thread at a time (&mut self).
unsafe impl Send for Stream {}

impl Stream {
    fn open(
        dir: c_int,
        device: Option<&str>,
        name: &str,
        attr: &BufferAttr,
    ) -> Result<Self, String> {
        let lib = lib()?;
        let spec = SampleSpec {
            format: PA_SAMPLE_S16LE,
            rate: SAMPLE_RATE,
            channels: CHANNELS as u8,
        };
        let server = server();
        let device = device.map(|d| CString::new(d).expect("no NUL"));
        let name = CString::new(name).expect("no NUL");
        let mut err = 0;
        // SAFETY: every pointer is valid or null as the API allows.
        let s = unsafe {
            (lib.new)(
                server.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
                c"Fernsicht".as_ptr(),
                dir,
                device.as_ref().map_or(std::ptr::null(), |d| d.as_ptr()),
                name.as_ptr(),
                &spec,
                std::ptr::null(),
                attr,
                &mut err,
            )
        };
        if s.is_null() {
            return Err(error(lib, "connect to the sound server", err));
        }
        Ok(Self { lib, s })
    }

    /// Buffered audio in the server, µs.
    fn latency_us(&mut self) -> u64 {
        let mut err = 0;
        // SAFETY: valid handle.
        unsafe { (self.lib.latency)(self.s, &mut err) }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: valid handle, freed once.
        unsafe { (self.lib.free)(self.s) };
    }
}

const FRAME_BYTES: u32 = (FRAME_SAMPLES * CHANNELS * 2) as u32;

/// What the computer plays: the monitor of the default output.
pub struct Capture(Stream);

impl Capture {
    pub fn open() -> Result<Self, String> {
        Stream::open(
            PA_STREAM_RECORD,
            Some("@DEFAULT_MONITOR@"),
            "screen sound",
            &BufferAttr {
                maxlength: DEFAULT,
                tlength: DEFAULT,
                prebuf: DEFAULT,
                minreq: DEFAULT,
                // Hand out every frame as soon as it is complete.
                fragsize: FRAME_BYTES,
            },
        )
        .map(Capture)
    }
}

impl crate::AudioSource for Capture {
    fn next_frame(&mut self, pcm: &mut Frame) -> Result<u64, String> {
        let mut err = 0;
        // SAFETY: pcm is FRAME_BYTES long.
        let r = unsafe {
            (self.0.lib.read)(
                self.0.s,
                pcm.as_mut_ptr().cast(),
                std::mem::size_of_val(pcm),
                &mut err,
            )
        };
        if r < 0 {
            return Err(error(self.0.lib, "record", err));
        }
        // When the newest sample was played, roughly: now minus what still
        // sits in the server's buffer.
        let buffered = self.0.latency_us();
        Ok(fernsicht_core::now_us().saturating_sub(buffered))
    }
}

/// Playback on the default output with a small server buffer.
pub struct Playback(Stream);

impl Playback {
    /// `buffer_frames`: how much the server holds (the rest of the delay
    /// is the client's jitter buffer).
    pub fn open(buffer_frames: u32) -> Result<Self, String> {
        let target = FRAME_BYTES * buffer_frames.max(1);
        Stream::open(
            PA_STREAM_PLAYBACK,
            None,
            "remote sound",
            &BufferAttr {
                maxlength: target * 4,
                tlength: target,
                prebuf: FRAME_BYTES,
                minreq: FRAME_BYTES,
                fragsize: DEFAULT,
            },
        )
        .map(Playback)
    }
}

impl crate::AudioSink for Playback {
    fn play(&mut self, pcm: &Frame) -> Result<(), String> {
        let mut err = 0;
        // SAFETY: pcm is FRAME_BYTES long; blocks until there is room.
        let r = unsafe {
            (self.0.lib.write)(
                self.0.s,
                pcm.as_ptr().cast(),
                std::mem::size_of_val(pcm),
                &mut err,
            )
        };
        if r < 0 {
            return Err(error(self.0.lib, "play", err));
        }
        Ok(())
    }

    fn buffered_us(&mut self) -> u64 {
        self.0.latency_us()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_layouts_match_pulse() {
        assert_eq!(std::mem::size_of::<SampleSpec>(), 12);
        assert_eq!(std::mem::size_of::<BufferAttr>(), 20);
        assert_eq!(FRAME_BYTES, 960);
    }
}
