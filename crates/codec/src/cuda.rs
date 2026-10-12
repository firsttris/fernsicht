//! The few CUDA driver calls NVENC and NVDEC need to trade pictures with
//! Vulkan: import Vulkan memory (NVENC's input), export CUDA memory
//! (NVDEC's pictures, for the renderer), copy on the GPU, check which GPU
//! it is.
//!
//! `libcuda.so.1` is loaded at runtime (as FFmpeg does), so builds and
//! machines without the NVIDIA driver are unaffected.

use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::os::fd::{AsFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd};
use std::sync::OnceLock;

use crate::CodecError;

pub type CuResult = c_int;
pub type CuContext = *mut c_void;
pub type CuDevice = c_int;
pub type CuDevicePtr = u64;
type CuExternalMemory = *mut c_void;

const CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD: c_int = 1;
const CU_MEMORYTYPE_DEVICE: c_uint = 2;
// Virtual memory management, values from cuda.h.
const CU_DEVICE_ATTRIBUTE_VIRTUAL_ADDRESS_MANAGEMENT_SUPPORTED: c_int = 102;
const CU_DEVICE_ATTRIBUTE_HANDLE_TYPE_POSIX_FILE_DESCRIPTOR_SUPPORTED: c_int = 103;
const CU_MEM_ALLOCATION_TYPE_PINNED: c_int = 1;
const CU_MEM_HANDLE_TYPE_POSIX_FILE_DESCRIPTOR: c_int = 1;
const CU_MEM_LOCATION_TYPE_DEVICE: c_int = 1;
const CU_MEM_ACCESS_FLAGS_PROT_READWRITE: c_int = 3;
const CU_MEM_ALLOC_GRANULARITY_RECOMMENDED: c_int = 1;

/// `CUmemLocation`.
#[repr(C)]
#[derive(Clone, Copy)]
struct MemLocation {
    kind: c_int,
    id: c_int,
}

/// `CUmemAllocationProp`.
#[repr(C)]
struct MemAllocationProp {
    kind: c_int,
    requested_handle_types: c_int,
    location: MemLocation,
    win32_handle_metadata: *mut c_void,
    /// compressionType, gpuDirectRDMACapable, usage (u16), reserved[4].
    alloc_flags: [u8; 8],
}

/// `CUmemAccessDesc`.
#[repr(C)]
struct MemAccessDesc {
    location: MemLocation,
    flags: c_int,
}

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
    cuDeviceGetAttribute: unsafe extern "C" fn(*mut c_int, c_int, CuDevice) -> CuResult,
    cuMemGetAllocationGranularity:
        unsafe extern "C" fn(*mut usize, *const MemAllocationProp, c_int) -> CuResult,
    cuMemCreate: unsafe extern "C" fn(*mut u64, usize, *const MemAllocationProp, u64) -> CuResult,
    cuMemExportToShareableHandle: unsafe extern "C" fn(*mut c_void, u64, c_int, u64) -> CuResult,
    cuMemAddressReserve:
        unsafe extern "C" fn(*mut CuDevicePtr, usize, usize, CuDevicePtr, u64) -> CuResult,
    cuMemMap: unsafe extern "C" fn(CuDevicePtr, usize, usize, u64, u64) -> CuResult,
    cuMemSetAccess:
        unsafe extern "C" fn(CuDevicePtr, usize, *const MemAccessDesc, usize) -> CuResult,
    cuMemUnmap: unsafe extern "C" fn(CuDevicePtr, usize) -> CuResult,
    cuMemRelease: unsafe extern "C" fn(u64) -> CuResult,
    cuMemAddressFree: unsafe extern "C" fn(CuDevicePtr, usize) -> CuResult,
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
                cuDeviceGetAttribute: sym!("cuDeviceGetAttribute"),
                cuMemGetAllocationGranularity: sym!("cuMemGetAllocationGranularity"),
                cuMemCreate: sym!("cuMemCreate"),
                cuMemExportToShareableHandle: sym!("cuMemExportToShareableHandle"),
                cuMemAddressReserve: sym!("cuMemAddressReserve"),
                cuMemMap: sym!("cuMemMap"),
                cuMemSetAccess: sym!("cuMemSetAccess"),
                cuMemUnmap: sym!("cuMemUnmap"),
                cuMemRelease: sym!("cuMemRelease"),
                cuMemAddressFree: sym!("cuMemAddressFree"),
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
            drop(unsafe { OwnedFd::from_raw_fd(raw) });
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

/// GPU memory CUDA allocated and exported as a file descriptor, for Vulkan
/// to import (`VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT`): NVDEC's
/// pictures reach the renderer without leaving the GPU.
pub struct ExportedBuffer {
    api: &'static Api,
    ctx: CuContext,
    pub ptr: CuDevicePtr,
    /// The allocation's size (a multiple of CUDA's granularity).
    pub size: usize,
    fd: OwnedFd,
}

impl ExportedBuffer {
    /// At least `len` bytes on the GPU of `ctx`.
    pub fn new(ctx: CuContext, len: usize) -> Result<Self, CodecError> {
        let api = api()?;
        let _current = Current::push(ctx)?;
        let mut dev = 0;
        // SAFETY: valid out-pointer; a context is current.
        check(api, "get device", unsafe { (api.cuCtxGetDevice)(&mut dev) })?;
        for (attribute, what) in [
            (
                CU_DEVICE_ATTRIBUTE_VIRTUAL_ADDRESS_MANAGEMENT_SUPPORTED,
                "virtual memory management",
            ),
            (
                CU_DEVICE_ATTRIBUTE_HANDLE_TYPE_POSIX_FILE_DESCRIPTOR_SUPPORTED,
                "memory exported as file descriptors",
            ),
        ] {
            let mut on = 0;
            // SAFETY: valid out-pointer and device.
            check(api, "query device", unsafe {
                (api.cuDeviceGetAttribute)(&mut on, attribute, dev)
            })?;
            if on == 0 {
                return Err(CodecError::Backend(format!("this GPU has no {what}")));
            }
        }
        let location = MemLocation {
            kind: CU_MEM_LOCATION_TYPE_DEVICE,
            id: dev,
        };
        let prop = MemAllocationProp {
            kind: CU_MEM_ALLOCATION_TYPE_PINNED,
            requested_handle_types: CU_MEM_HANDLE_TYPE_POSIX_FILE_DESCRIPTOR,
            location,
            win32_handle_metadata: std::ptr::null_mut(),
            alloc_flags: [0; 8],
        };
        let mut granularity = 0;
        // SAFETY: valid pointers.
        check(api, "query allocation granularity", unsafe {
            (api.cuMemGetAllocationGranularity)(
                &mut granularity,
                &prop,
                CU_MEM_ALLOC_GRANULARITY_RECOMMENDED,
            )
        })?;
        let size = len.next_multiple_of(granularity.max(1));
        let mut handle = 0u64;
        // SAFETY: valid pointers; the handle is released below either way.
        check(api, "allocate", unsafe {
            (api.cuMemCreate)(&mut handle, size, &prop, 0)
        })?;
        let mut raw: c_int = -1;
        // SAFETY: a POSIX handle is written as an int.
        let exported = check(api, "export memory", unsafe {
            (api.cuMemExportToShareableHandle)(
                (&raw mut raw).cast(),
                handle,
                prop.requested_handle_types,
                0,
            )
        });
        let mapped = exported.and_then(|()| {
            let mut ptr = 0;
            // SAFETY: reserve, map and allow access to the allocation; each
            // step undone below when a later one fails.
            unsafe {
                check(
                    api,
                    "reserve addresses",
                    (api.cuMemAddressReserve)(&mut ptr, size, granularity, 0, 0),
                )?;
                let desc = MemAccessDesc {
                    location,
                    flags: CU_MEM_ACCESS_FLAGS_PROT_READWRITE,
                };
                let r = check(api, "map memory", (api.cuMemMap)(ptr, size, 0, handle, 0)).and_then(
                    |()| {
                        check(
                            api,
                            "allow access",
                            (api.cuMemSetAccess)(ptr, size, &desc, 1),
                        )
                        .inspect_err(|_| {
                            (api.cuMemUnmap)(ptr, size);
                        })
                    },
                );
                if r.is_err() {
                    (api.cuMemAddressFree)(ptr, size);
                }
                r.map(|()| ptr)
            }
        });
        // The mapping and the exported descriptor keep the memory; the
        // handle is not needed any more.
        // SAFETY: created above.
        unsafe { (api.cuMemRelease)(handle) };
        // SAFETY: on success the export wrote a descriptor we now own.
        let fd = (raw >= 0).then(|| unsafe { OwnedFd::from_raw_fd(raw) });
        let ptr = mapped?;
        Ok(Self {
            api,
            ctx,
            ptr,
            size,
            fd: fd.ok_or_else(|| CodecError::Backend("CUDA exported no descriptor".into()))?,
        })
    }

    /// The memory's descriptor; an importer takes a duplicate.
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl Drop for ExportedBuffer {
    fn drop(&mut self) {
        let Ok(_current) = Current::push(self.ctx) else {
            return;
        };
        // SAFETY: mapped and reserved in new() under this context.
        unsafe {
            (self.api.cuMemUnmap)(self.ptr, self.size);
            (self.api.cuMemAddressFree)(self.ptr, self.size);
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
        assert_eq!(std::mem::size_of::<MemAllocationProp>(), 32);
        assert_eq!(
            std::mem::offset_of!(MemAllocationProp, win32_handle_metadata),
            16
        );
        assert_eq!(std::mem::size_of::<MemAccessDesc>(), 12);
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
pub fn nvdec_supports(
    _current: &Current<'_>,
    cuvid_codec: c_uint,
    bits: c_uint,
) -> Result<bool, CodecError> {
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
            bit_depth_minus8: bits - 8,
            ..Default::default()
        };
        check(api()?, "query NVDEC", caps_fn(&mut caps))?;
        Ok(caps.supported != 0 && caps.max_width >= 1920 && caps.max_height >= 1080)
    }
}
