//! The few CUDA driver calls the NVENC encoder needs to take frames from
//! Vulkan: import Vulkan memory, copy on the GPU, check which GPU it is.
//!
//! `libcuda.so.1` is loaded at runtime (as FFmpeg does), so builds and
//! machines without the NVIDIA driver are unaffected.

use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::os::fd::{IntoRawFd, OwnedFd};
use std::sync::OnceLock;

use crate::CodecError;

pub type CuResult = c_int;
pub type CuContext = *mut c_void;
pub type CuDevice = c_int;
pub type CuDevicePtr = u64;
type CuExternalMemory = *mut c_void;

const CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD: c_int = 1;
const CU_MEMORYTYPE_DEVICE: c_uint = 2;

#[repr(C)]
#[derive(Clone, Copy)]
union HandleUnion {
    fd: c_int,
    win32: [*const c_void; 2],
}

#[repr(C)]
struct ExternalMemoryHandleDesc {
    kind: c_int,
    handle: HandleUnion,
    size: u64,
    flags: c_uint,
    reserved: [c_uint; 16],
}

#[repr(C)]
struct ExternalMemoryBufferDesc {
    offset: u64,
    size: u64,
    flags: c_uint,
    reserved: [c_uint; 16],
}

#[repr(C)]
#[derive(Default)]
struct Memcpy2D {
    src_x_in_bytes: usize,
    src_y: usize,
    src_memory_type: c_uint,
    src_host: usize,
    src_device: CuDevicePtr,
    src_array: usize,
    src_pitch: usize,
    dst_x_in_bytes: usize,
    dst_y: usize,
    dst_memory_type: c_uint,
    dst_host: usize,
    dst_device: CuDevicePtr,
    dst_array: usize,
    dst_pitch: usize,
    width_in_bytes: usize,
    height: usize,
}

#[allow(non_snake_case)]
struct Api {
    _lib: libloading::Library,
    cuGetErrorString: unsafe extern "C" fn(CuResult, *mut *const c_char) -> CuResult,
    cuCtxPushCurrent: unsafe extern "C" fn(CuContext) -> CuResult,
    cuCtxPopCurrent: unsafe extern "C" fn(*mut CuContext) -> CuResult,
    cuCtxGetDevice: unsafe extern "C" fn(*mut CuDevice) -> CuResult,
    cuDeviceGetUuid: unsafe extern "C" fn(*mut [u8; 16], CuDevice) -> CuResult,
    cuImportExternalMemory:
        unsafe extern "C" fn(*mut CuExternalMemory, *const ExternalMemoryHandleDesc) -> CuResult,
    cuExternalMemoryGetMappedBuffer: unsafe extern "C" fn(
        *mut CuDevicePtr,
        CuExternalMemory,
        *const ExternalMemoryBufferDesc,
    ) -> CuResult,
    cuDestroyExternalMemory: unsafe extern "C" fn(CuExternalMemory) -> CuResult,
    cuMemFree: unsafe extern "C" fn(CuDevicePtr) -> CuResult,
    cuMemcpy2DAsync: unsafe extern "C" fn(*const Memcpy2D, *mut c_void) -> CuResult,
    cuStreamSynchronize: unsafe extern "C" fn(*mut c_void) -> CuResult,
}

fn api() -> Result<&'static Api, CodecError> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| {
        // SAFETY: loading the NVIDIA driver library and looking up symbols
        // with the signatures of cuda.h (versioned names where CUDA has
        // them, as the runtime would pick).
        unsafe {
            let lib = libloading::Library::new("libcuda.so.1")
                .map_err(|e| format!("no NVIDIA driver (libcuda.so.1): {e}"))?;
            macro_rules! sym {
                ($name:literal) => {
                    *lib.get(concat!($name, "\0").as_bytes())
                        .map_err(|e| format!("libcuda has no {}: {e}", $name))?
                };
            }
            Ok(Api {
                cuGetErrorString: sym!("cuGetErrorString"),
                cuCtxPushCurrent: sym!("cuCtxPushCurrent_v2"),
                cuCtxPopCurrent: sym!("cuCtxPopCurrent_v2"),
                cuCtxGetDevice: sym!("cuCtxGetDevice"),
                cuDeviceGetUuid: sym!("cuDeviceGetUuid_v2"),
                cuImportExternalMemory: sym!("cuImportExternalMemory"),
                cuExternalMemoryGetMappedBuffer: sym!("cuExternalMemoryGetMappedBuffer"),
                cuDestroyExternalMemory: sym!("cuDestroyExternalMemory"),
                cuMemFree: sym!("cuMemFree_v2"),
                cuMemcpy2DAsync: sym!("cuMemcpy2DAsync_v2"),
                cuStreamSynchronize: sym!("cuStreamSynchronize"),
                _lib: lib,
            })
        }
    })
    .as_ref()
    .map_err(|e| CodecError::Backend(e.clone()))
}

fn check(api: &Api, what: &str, r: CuResult) -> Result<(), CodecError> {
    if r == 0 {
        return Ok(());
    }
    let mut msg: *const c_char = std::ptr::null();
    // SAFETY: cuGetErrorString sets a static string or leaves it null.
    let text = unsafe {
        (api.cuGetErrorString)(r, &mut msg);
        if msg.is_null() {
            "unknown error".into()
        } else {
            CStr::from_ptr(msg).to_string_lossy().into_owned()
        }
    };
    Err(CodecError::Backend(format!("CUDA {what}: {text} ({r})")))
}

/// Makes `ctx` current on this thread until dropped.
pub struct Current<'a> {
    api: &'a Api,
}

impl Current<'_> {
    pub fn push(ctx: CuContext) -> Result<Self, CodecError> {
        let api = api()?;
        // SAFETY: ctx is a live context (FFmpeg's CUDA device).
        check(api, "make context current", unsafe {
            (api.cuCtxPushCurrent)(ctx)
        })?;
        Ok(Current { api })
    }

    /// UUID of the GPU behind the current context.
    pub fn device_uuid(&self) -> Result<[u8; 16], CodecError> {
        let mut dev = 0;
        let mut uuid = [0u8; 16];
        // SAFETY: valid out-pointers; a context is current.
        unsafe {
            check(self.api, "get device", (self.api.cuCtxGetDevice)(&mut dev))?;
            check(
                self.api,
                "get device UUID",
                (self.api.cuDeviceGetUuid)(&mut uuid, dev),
            )?;
        }
        Ok(uuid)
    }
}

impl Drop for Current<'_> {
    fn drop(&mut self) {
        let mut ctx = std::ptr::null_mut();
        // SAFETY: pops what push() pushed.
        unsafe { (self.api.cuCtxPopCurrent)(&mut ctx) };
    }
}

/// Memory another API exported, mapped as a linear CUDA buffer.
pub struct ImportedBuffer {
    api: &'static Api,
    ctx: CuContext,
    memory: CuExternalMemory,
    pub ptr: CuDevicePtr,
}

impl ImportedBuffer {
    /// Imports `size` bytes from an opaque fd (Vulkan
    /// `VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT`); `allocation` is
    /// the size of the whole exported allocation. CUDA owns `fd` afterwards.
    pub fn import(
        ctx: CuContext,
        fd: OwnedFd,
        allocation: u64,
        size: u64,
    ) -> Result<Self, CodecError> {
        let api = api()?;
        let _current = Current::push(ctx)?;
        let raw = fd.into_raw_fd();
        let desc = ExternalMemoryHandleDesc {
            kind: CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD,
            handle: HandleUnion { fd: raw },
            size: allocation,
            flags: 0,
            reserved: [0; 16],
        };
        let mut memory = std::ptr::null_mut();
        // SAFETY: valid descriptor; on success CUDA owns the fd.
        let r = unsafe { (api.cuImportExternalMemory)(&mut memory, &desc) };
        if r != 0 {
            // SAFETY: not consumed on failure.
            drop(unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(raw) });
            check(api, "import Vulkan memory", r)?;
        }
        let mut buf = ImportedBuffer {
            api,
            ctx,
            memory,
            ptr: 0,
        };
        let map = ExternalMemoryBufferDesc {
            offset: 0,
            size,
            flags: 0,
            reserved: [0; 16],
        };
        // SAFETY: memory was just imported; Drop releases both.
        check(api, "map Vulkan memory", unsafe {
            (api.cuExternalMemoryGetMappedBuffer)(&mut buf.ptr, memory, &map)
        })?;
        Ok(buf)
    }
}

impl Drop for ImportedBuffer {
    fn drop(&mut self) {
        let Ok(_current) = Current::push(self.ctx) else {
            return;
        };
        // SAFETY: both were created in import() under this context.
        unsafe {
            if self.ptr != 0 {
                (self.api.cuMemFree)(self.ptr);
            }
            (self.api.cuDestroyExternalMemory)(self.memory);
        }
    }
}

/// A pitched 2D copy on the GPU, device to device.
#[derive(Clone, Copy, Debug)]
pub struct Copy2D {
    pub src: CuDevicePtr,
    pub src_pitch: usize,
    pub dst: CuDevicePtr,
    pub dst_pitch: usize,
    pub width_bytes: usize,
    pub rows: usize,
}

/// Runs the copies on `stream` and waits for them. A context must be
/// current.
pub fn copy_2d(
    _current: &Current<'_>,
    stream: *mut c_void,
    copies: &[Copy2D],
) -> Result<(), CodecError> {
    let api = api()?;
    for c in copies {
        let p = Memcpy2D {
            src_memory_type: CU_MEMORYTYPE_DEVICE,
            src_device: c.src,
            src_pitch: c.src_pitch,
            dst_memory_type: CU_MEMORYTYPE_DEVICE,
            dst_device: c.dst,
            dst_pitch: c.dst_pitch,
            width_in_bytes: c.width_bytes,
            height: c.rows,
            ..Default::default()
        };
        // SAFETY: both ranges are device memory of at least pitch × rows.
        check(api, "copy", unsafe { (api.cuMemcpy2DAsync)(&p, stream) })?;
    }
    // SAFETY: stream belongs to the current context.
    check(api, "wait for the copy", unsafe {
        (api.cuStreamSynchronize)(stream)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_layouts_match_cuda_h() {
        // Sizes from cuda.h on x86_64.
        assert_eq!(std::mem::size_of::<ExternalMemoryHandleDesc>(), 104);
        assert_eq!(std::mem::size_of::<ExternalMemoryBufferDesc>(), 88);
        assert_eq!(std::mem::size_of::<Memcpy2D>(), 128);
        assert_eq!(std::mem::offset_of!(ExternalMemoryHandleDesc, size), 24);
        assert_eq!(std::mem::offset_of!(Memcpy2D, src_host), 24);
        assert_eq!(std::mem::offset_of!(Memcpy2D, dst_memory_type), 72);
        assert_eq!(std::mem::size_of::<DecodeCaps>(), 88);
        assert_eq!(std::mem::offset_of!(DecodeCaps, supported), 24);
        assert_eq!(std::mem::offset_of!(DecodeCaps, max_width), 28);
    }
}

/// `CUVIDDECODECAPS` from nv-codec-headers (dynlink_cuviddec.h).
#[repr(C)]
#[derive(Default)]
struct DecodeCaps {
    codec: c_uint,
    chroma: c_uint,
    bit_depth_minus8: c_uint,
    reserved1: [c_uint; 3],
    supported: u8,
    nvdecs: u8,
    output_format_mask: u16,
    max_width: c_uint,
    max_height: c_uint,
    max_mb_count: c_uint,
    min_width: u16,
    min_height: u16,
    histogram: u8,
    counter_bit_depth: u8,
    max_histogram_bins: u16,
    decode_stats: u8,
    reserved4: [u8; 3],
    reserved3: [c_uint; 9],
}

/// cudaVideoCodec values.
pub const CUVID_H264: c_uint = 4;
pub const CUVID_HEVC: c_uint = 8;
pub const CUVID_AV1: c_uint = 11;

/// Whether NVDEC on the GPU of the current context decodes `cuvid_codec`
/// in 8-bit 4:2:0 up to at least 1920×1080 (pre-Pascal GPUs lack HEVC).
pub fn nvdec_supports(_current: &Current<'_>, cuvid_codec: c_uint) -> Result<bool, CodecError> {
    static LIB: OnceLock<Result<libloading::Library, String>> = OnceLock::new();
    let lib = LIB
        .get_or_init(|| {
            // SAFETY: loading the NVIDIA video decode library.
            unsafe { libloading::Library::new("libnvcuvid.so.1") }
                .map_err(|e| format!("no NVDEC (libnvcuvid.so.1): {e}"))
        })
        .as_ref()
        .map_err(|e| CodecError::Backend(e.clone()))?;
    // SAFETY: the signature of cuvidGetDecoderCaps; a context is current.
    unsafe {
        let caps_fn: libloading::Symbol<unsafe extern "C" fn(*mut DecodeCaps) -> CuResult> =
            lib.get(b"cuvidGetDecoderCaps\0").map_err(|e| {
                CodecError::Backend(format!("libnvcuvid has no cuvidGetDecoderCaps: {e}"))
            })?;
        let mut caps = DecodeCaps {
            codec: cuvid_codec,
            chroma: 1, // 4:2:0
            ..Default::default()
        };
        check(api()?, "query NVDEC", caps_fn(&mut caps))?;
        Ok(caps.supported != 0 && caps.max_width >= 1920 && caps.max_height >= 1080)
    }
}
