//! Opus through libopus, loaded at runtime (no build dependency; without
//! the library there is simply no audio).

use std::ffi::{c_int, c_void};
use std::sync::OnceLock;

use crate::{CHANNELS, FRAME_SAMPLES, SAMPLE_RATE};

// opus_defines.h
const OPUS_APPLICATION_RESTRICTED_LOWDELAY: c_int = 2051;
const OPUS_SET_BITRATE_REQUEST: c_int = 4002;
const OPUS_SET_VBR_REQUEST: c_int = 4006;
const OPUS_SET_COMPLEXITY_REQUEST: c_int = 4010;

/// Largest encoded frame accepted on the wire.
pub const MAX_PACKET: usize = 400;

type EncoderCreate = unsafe extern "C" fn(i32, c_int, c_int, *mut c_int) -> *mut c_void;
type Encode = unsafe extern "C" fn(*mut c_void, *const i16, c_int, *mut u8, i32) -> i32;
type EncoderCtl = unsafe extern "C" fn(*mut c_void, c_int, ...) -> c_int;
type Destroy = unsafe extern "C" fn(*mut c_void);
type DecoderCreate = unsafe extern "C" fn(i32, c_int, *mut c_int) -> *mut c_void;
type Decode = unsafe extern "C" fn(*mut c_void, *const u8, i32, *mut i16, c_int, c_int) -> c_int;

struct Lib {
    _lib: libloading::Library,
    encoder_create: EncoderCreate,
    encode: Encode,
    encoder_ctl: EncoderCtl,
    encoder_destroy: Destroy,
    decoder_create: DecoderCreate,
    decode: Decode,
    decoder_destroy: Destroy,
}

fn lib() -> Result<&'static Lib, String> {
    static LIB: OnceLock<Result<Lib, String>> = OnceLock::new();
    LIB.get_or_init(|| {
        // SAFETY: libopus has no load-time side effects; the symbols are
        // used with their C signatures from opus.h.
        unsafe {
            let lib = libloading::Library::new("libopus.so.0")
                .map_err(|e| format!("libopus not found: {e}"))?;
            macro_rules! sym {
                ($name:literal) => {
                    *lib.get($name)
                        .map_err(|e| format!("libopus lacks a function: {e}"))?
                };
            }
            Ok(Lib {
                encoder_create: sym!(b"opus_encoder_create"),
                encode: sym!(b"opus_encode"),
                encoder_ctl: sym!(b"opus_encoder_ctl"),
                encoder_destroy: sym!(b"opus_encoder_destroy"),
                decoder_create: sym!(b"opus_decoder_create"),
                decode: sym!(b"opus_decode"),
                decoder_destroy: sym!(b"opus_decoder_destroy"),
                _lib: lib,
            })
        }
    })
    .as_ref()
    .map_err(Clone::clone)
}

/// Whether libopus can be loaded.
pub fn available() -> Result<(), String> {
    lib().map(|_| ())
}

/// 48 kHz stereo, 5 ms frames, CELT only (lowest delay), constant bitrate.
pub struct Encoder {
    lib: &'static Lib,
    st: *mut c_void,
}

// SAFETY: the encoder state is only used through &mut self.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(bitrate: u32) -> Result<Self, String> {
        let lib = lib()?;
        let mut err = 0;
        // SAFETY: valid arguments; the state is checked and owned.
        let st = unsafe {
            (lib.encoder_create)(
                SAMPLE_RATE as i32,
                CHANNELS as c_int,
                OPUS_APPLICATION_RESTRICTED_LOWDELAY,
                &mut err,
            )
        };
        if st.is_null() || err != 0 {
            return Err(format!("opus_encoder_create failed ({err})"));
        }
        let enc = Self { lib, st };
        // SAFETY: ctl requests with an opus_int32 argument each.
        unsafe {
            (lib.encoder_ctl)(st, OPUS_SET_BITRATE_REQUEST, bitrate as i32);
            (lib.encoder_ctl)(st, OPUS_SET_VBR_REQUEST, 0i32);
            (lib.encoder_ctl)(st, OPUS_SET_COMPLEXITY_REQUEST, 5i32);
        }
        Ok(enc)
    }

    /// Encodes one frame of interleaved PCM into `out`; returns its length.
    pub fn encode(
        &mut self,
        pcm: &[i16; FRAME_SAMPLES * CHANNELS],
        out: &mut [u8],
    ) -> Result<usize, String> {
        // SAFETY: pcm holds a full frame, out its length.
        let n = unsafe {
            (self.lib.encode)(
                self.st,
                pcm.as_ptr(),
                FRAME_SAMPLES as c_int,
                out.as_mut_ptr(),
                out.len().min(i32::MAX as usize) as i32,
            )
        };
        usize::try_from(n).map_err(|_| format!("opus_encode failed ({n})"))
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: created in new(), destroyed once.
        unsafe { (self.lib.encoder_destroy)(self.st) };
    }
}

pub struct Decoder {
    lib: &'static Lib,
    st: *mut c_void,
}

// SAFETY: as for Encoder.
unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new() -> Result<Self, String> {
        let lib = lib()?;
        let mut err = 0;
        // SAFETY: valid arguments; the state is checked and owned.
        let st = unsafe { (lib.decoder_create)(SAMPLE_RATE as i32, CHANNELS as c_int, &mut err) };
        if st.is_null() || err != 0 {
            return Err(format!("opus_decoder_create failed ({err})"));
        }
        Ok(Self { lib, st })
    }

    /// Decodes `data` into one frame; `None` conceals a lost frame
    /// (packet loss concealment).
    pub fn decode(
        &mut self,
        data: Option<&[u8]>,
        pcm: &mut [i16; FRAME_SAMPLES * CHANNELS],
    ) -> Result<(), String> {
        let (ptr, len) = match data {
            Some(d) if !d.is_empty() => (d.as_ptr(), d.len().min(i32::MAX as usize) as i32),
            _ => (std::ptr::null(), 0),
        };
        // SAFETY: pcm holds a full frame.
        let n = unsafe {
            (self.lib.decode)(
                self.st,
                ptr,
                len,
                pcm.as_mut_ptr(),
                FRAME_SAMPLES as c_int,
                0,
            )
        };
        if n != FRAME_SAMPLES as c_int {
            return Err(format!("opus_decode failed ({n})"));
        }
        Ok(())
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: created in new(), destroyed once.
        unsafe { (self.lib.decoder_destroy)(self.st) };
    }
}
