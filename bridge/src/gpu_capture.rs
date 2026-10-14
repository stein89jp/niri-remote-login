//! Small explicit-modifier GBM capture pool. No CPU mapping, CUDA SDK or host display.
use std::{
    ffi::CStr,
    fs::File,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    sync::{Arc, Mutex},
};

type DeviceCreate = unsafe extern "C" fn(i32) -> *mut libc::c_void;
type DeviceDestroy = unsafe extern "C" fn(*mut libc::c_void);
type BoCreate = unsafe extern "C" fn(*mut libc::c_void, u32, u32, u32, u32) -> *mut libc::c_void;
type BoCreateModifiers = unsafe extern "C" fn(
    *mut libc::c_void,
    u32,
    u32,
    u32,
    *const u64,
    u32,
    u32,
) -> *mut libc::c_void;
type BoPlaneCount = unsafe extern "C" fn(*mut libc::c_void) -> i32;
type BoOffset = unsafe extern "C" fn(*mut libc::c_void, i32) -> u32;
type BoDestroy = unsafe extern "C" fn(*mut libc::c_void);
type BoFd = unsafe extern "C" fn(*mut libc::c_void) -> i32;
type BoStride = unsafe extern "C" fn(*mut libc::c_void) -> u32;
type BoModifier = unsafe extern "C" fn(*mut libc::c_void) -> u64;

struct Api {
    library: *mut libc::c_void,
    device_create: DeviceCreate,
    device_destroy: DeviceDestroy,
    bo_create: BoCreate,
    bo_create_modifiers: BoCreateModifiers,
    bo_plane_count: BoPlaneCount,
    bo_offset: BoOffset,
    bo_destroy: BoDestroy,
    bo_fd: BoFd,
    bo_stride: BoStride,
    bo_modifier: BoModifier,
}
// GBM functions and exported handles are process-global, not EGL-context-bound.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}
impl Api {
    fn load() -> anyhow::Result<Arc<Self>> {
        // SAFETY: a literal NUL-terminated soname and standard dlopen flags.
        let library =
            unsafe { libc::dlopen(c"libgbm.so.1".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        anyhow::ensure!(!library.is_null(), "GBM library unavailable");
        let symbol = |name: &CStr| -> anyhow::Result<*mut libc::c_void> {
            // SAFETY: the library remains loaded and symbol name is terminated.
            let value = unsafe { libc::dlsym(library, name.as_ptr()) };
            anyhow::ensure!(
                !value.is_null(),
                "missing GBM symbol {}",
                name.to_string_lossy()
            );
            Ok(value)
        };
        let result = (|| -> anyhow::Result<Self> {
            // SAFETY: each name is the stable GBM ABI function with this signature.
            Ok(unsafe {
                Self {
                    library,
                    device_create: std::mem::transmute::<*mut libc::c_void, DeviceCreate>(symbol(
                        c"gbm_create_device",
                    )?),
                    device_destroy: std::mem::transmute::<*mut libc::c_void, DeviceDestroy>(
                        symbol(c"gbm_device_destroy")?,
                    ),
                    bo_create: std::mem::transmute::<*mut libc::c_void, BoCreate>(symbol(
                        c"gbm_bo_create",
                    )?),
                    bo_create_modifiers: std::mem::transmute::<*mut libc::c_void, BoCreateModifiers>(
                        symbol(c"gbm_bo_create_with_modifiers2")?,
                    ),
                    bo_plane_count: std::mem::transmute::<*mut libc::c_void, BoPlaneCount>(symbol(
                        c"gbm_bo_get_plane_count",
                    )?),
                    bo_offset: std::mem::transmute::<*mut libc::c_void, BoOffset>(symbol(
                        c"gbm_bo_get_offset",
                    )?),
                    bo_destroy: std::mem::transmute::<*mut libc::c_void, BoDestroy>(symbol(
                        c"gbm_bo_destroy",
                    )?),
                    bo_fd: std::mem::transmute::<*mut libc::c_void, BoFd>(symbol(
                        c"gbm_bo_get_fd",
                    )?),
                    bo_stride: std::mem::transmute::<*mut libc::c_void, BoStride>(symbol(
                        c"gbm_bo_get_stride",
                    )?),
                    bo_modifier: std::mem::transmute::<*mut libc::c_void, BoModifier>(symbol(
                        c"gbm_bo_get_modifier",
                    )?),
                }
            })
        })();
        if result.is_err() {
            unsafe { libc::dlclose(library) };
        }
        result.map(Arc::new)
    }
}
impl Drop for Api {
    fn drop(&mut self) {
        unsafe { libc::dlclose(self.library) };
    }
}
struct Device {
    api: Arc<Api>,
    raw: *mut libc::c_void,
    _file: File,
    pub identity: u64,
    lock: Mutex<()>,
}
unsafe impl Send for Device {}
unsafe impl Sync for Device {}
impl Drop for Device {
    fn drop(&mut self) {
        unsafe { (self.api.device_destroy)(self.raw) };
    }
}

pub(crate) struct Buffer {
    device: Arc<Device>,
    raw: *mut libc::c_void,
    pub stride: u32,
    pub modifier: u64,
    pub offset: u32,
}
// No caller accesses GBM contents; lifetime prevents compositor reuse while the
// exported DMA-BUF is being encoded. gbm_bo_destroy releases only this BO.
unsafe impl Send for Buffer {}
unsafe impl Sync for Buffer {}
impl Drop for Buffer {
    fn drop(&mut self) {
        let _guard = self.device.lock.lock().unwrap();
        unsafe { (self.device.api.bo_destroy)(self.raw) };
    }
}
impl Buffer {
    pub fn fd(&self) -> anyhow::Result<OwnedFd> {
        let _guard = self.device.lock.lock().unwrap();
        let fd = unsafe { (self.device.api.bo_fd)(self.raw) };
        anyhow::ensure!(fd >= 0, "exporting capture DMA-BUF failed");
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
    pub fn device(&self) -> u64 {
        self.device.identity
    }
}

pub(crate) struct Allocator {
    device: Arc<Device>,
}
impl Allocator {
    pub fn new() -> anyhow::Result<Self> {
        let api = Api::load()?;
        let paths = if let Ok(path) = std::env::var("NIRI_RDP_GPU_DEVICE") {
            anyhow::ensure!(
                path.starts_with("/dev/dri/renderD"),
                "NIRI_RDP_GPU_DEVICE must be a DRM render node"
            );
            vec![std::path::PathBuf::from(path)]
        } else {
            let mut paths: Vec<_> = std::fs::read_dir("/dev/dri")?
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("renderD"))
                .map(|entry| entry.path())
                .collect();
            paths.sort();
            paths
        };
        for path in paths {
            let Ok(file) = File::options().read(true).write(true).open(&path) else {
                continue;
            };
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            if unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
                continue;
            }
            let stat = unsafe { stat.assume_init() };
            if stat.st_mode & libc::S_IFMT != libc::S_IFCHR {
                continue;
            }
            let vendor = std::fs::read_to_string(format!(
                "/sys/dev/char/{}:{}/device/vendor",
                libc::major(stat.st_rdev),
                libc::minor(stat.st_rdev)
            ))
            .unwrap_or_default();
            if vendor.trim() != "0x10de" {
                continue;
            }
            let raw = unsafe { (api.device_create)(file.as_raw_fd()) };
            if raw.is_null() {
                continue;
            }
            return Ok(Self {
                device: Arc::new(Device {
                    api,
                    raw,
                    _file: file,
                    identity: stat.st_rdev,
                    lock: Mutex::new(()),
                }),
            });
        }
        anyhow::bail!("accessible NVIDIA GBM render node unavailable")
    }
    pub fn allocate(
        &self,
        width: u32,
        height: u32,
        modifiers: &[u64],
    ) -> anyhow::Result<Arc<Buffer>> {
        crate::buffer_length(width, height, width * 4)?;
        anyhow::ensure!(
            !modifiers.is_empty() && !modifiers.contains(&u64::MAX),
            "explicit capture modifiers unavailable"
        );
        let guard = self.device.lock.lock().unwrap();
        // Prefer linear when available. NVIDIA commonly requires block-linear
        // rendering buffers, selected from the compositor's exact advertisement.
        let mut raw = std::ptr::null_mut();
        if modifiers.contains(&0) {
            raw = unsafe {
                (self.device.api.bo_create)(
                    self.device.raw,
                    width,
                    height,
                    0x34325258,
                    (1 << 2) | (1 << 4),
                )
            };
        }
        if raw.is_null() {
            raw = unsafe {
                (self.device.api.bo_create_modifiers)(
                    self.device.raw,
                    width,
                    height,
                    0x34325258,
                    modifiers.as_ptr(),
                    modifiers.len() as u32,
                    1 << 2,
                )
            };
        }
        if raw.is_null() {
            raw = unsafe {
                (self.device.api.bo_create)(self.device.raw, width, height, 0x34325258, 1 << 2)
            };
        }
        anyhow::ensure!(!raw.is_null(), "GPU capture allocation failed");
        let planes = unsafe { (self.device.api.bo_plane_count)(raw) };
        let buffer = Buffer {
            device: self.device.clone(),
            raw,
            stride: unsafe { (self.device.api.bo_stride)(raw) },
            modifier: unsafe { (self.device.api.bo_modifier)(raw) },
            offset: unsafe { (self.device.api.bo_offset)(raw, 0) },
        };
        drop(guard);
        anyhow::ensure!(planes == 1, "multi-plane XRGB capture unsupported");
        anyhow::ensure!(
            modifiers.contains(&buffer.modifier),
            "capture modifier not advertised by compositor"
        );
        anyhow::ensure!(buffer.stride >= width * 4, "GPU capture stride invalid");
        Ok(Arc::new(buffer))
    }
}
