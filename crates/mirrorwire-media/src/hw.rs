use anyhow::{bail, Result};
use ffmpeg_next::ffi;
use std::ffi::CString;
use std::ptr;

/// Owned reference to an FFmpeg hardware device or frames context.
pub(crate) struct HwRef(*mut ffi::AVBufferRef);

unsafe impl Send for HwRef {}

impl HwRef {
    pub(crate) fn as_ptr(&self) -> *mut ffi::AVBufferRef {
        self.0
    }

    pub(crate) fn new_ref(&self) -> *mut ffi::AVBufferRef {
        unsafe { ffi::av_buffer_ref(self.0) }
    }
}

impl Drop for HwRef {
    fn drop(&mut self) {
        unsafe { ffi::av_buffer_unref(&mut self.0) };
    }
}

pub(crate) fn create_device(kind: ffi::AVHWDeviceType, device: Option<&str>) -> Result<HwRef> {
    let device = device.map(CString::new).transpose()?;
    let mut buf = ptr::null_mut();
    let ret = unsafe {
        ffi::av_hwdevice_ctx_create(
            &mut buf,
            kind,
            device.as_ref().map_or(ptr::null(), |d| d.as_ptr()),
            ptr::null_mut(),
            0,
        )
    };
    if ret < 0 || buf.is_null() {
        bail!("creating {kind:?} device failed: {}", ffmpeg_next::Error::from(ret));
    }
    Ok(HwRef(buf))
}

pub(crate) fn create_frames(
    device: &HwRef,
    hw_format: ffi::AVPixelFormat,
    sw_format: ffi::AVPixelFormat,
    width: u32,
    height: u32,
) -> Result<HwRef> {
    create_frames_sized(device.as_ptr(), hw_format, sw_format, width, height, 4)
}

/// Frames context with an explicit initial pool size (0 = surfaces are made
/// on demand, which is what DMA-BUF mapping needs).
pub(crate) fn create_frames_sized(
    device: *mut ffi::AVBufferRef,
    hw_format: ffi::AVPixelFormat,
    sw_format: ffi::AVPixelFormat,
    width: u32,
    height: u32,
    pool: i32,
) -> Result<HwRef> {
    unsafe {
        let buf = ffi::av_hwframe_ctx_alloc(device);
        if buf.is_null() {
            bail!("av_hwframe_ctx_alloc failed");
        }
        let frames = HwRef(buf);
        let ctx = (*buf).data as *mut ffi::AVHWFramesContext;
        (*ctx).format = hw_format;
        (*ctx).sw_format = sw_format;
        (*ctx).width = width as i32;
        (*ctx).height = height as i32;
        (*ctx).initial_pool_size = pool;
        let ret = ffi::av_hwframe_ctx_init(buf);
        if ret < 0 {
            bail!("av_hwframe_ctx_init failed: {}", ffmpeg_next::Error::from(ret));
        }
        Ok(frames)
    }
}
