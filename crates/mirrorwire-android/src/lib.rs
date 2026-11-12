//! MirrorWire viewer for Android.
//!
//! The APK is a thin Java shell (`packaging/android`, a `NativeActivity`
//! subclass that only holds a Wi-Fi multicast lock and forwards
//! `mirrorwire://` links) around this library. Everything else is Rust:
//! egui UI, QUIC session (`mirrorwire-net`), and H.264 decode through the
//! platform `MediaCodec` (hardware first, Google's software decoder as the
//! fallback).

pub mod h264;
pub mod yuv;

#[cfg(target_os = "android")]
mod app;
#[cfg(target_os = "android")]
mod codec;
#[cfg(target_os = "android")]
mod session;

/// Entry point called by android-activity's NativeActivity glue.
#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: winit::platform::android::activity::AndroidApp) {
    app::run(app);
}
