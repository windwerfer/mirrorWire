//! Which window system the app really runs on. Linux is Wayland-first
//! (decisions.md); this is only reported, never used to pick X11 code paths.

/// From the environment, before any window exists (CLI and log header).
pub fn session_from_env() -> String {
    if cfg!(target_os = "macos") {
        return "macOS".into();
    }
    if cfg!(target_os = "android") {
        return "Android".into();
    }
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let x11 = std::env::var_os("DISPLAY").is_some();
    let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
    match (wayland, x11) {
        (true, _) => format!("Wayland session (XDG_SESSION_TYPE={session})"),
        (false, true) => format!("X11 only (no WAYLAND_DISPLAY; XDG_SESSION_TYPE={session})"),
        (false, false) => "no display (headless)".into(),
    }
}

/// From the real window handle: tells native Wayland apart from X11/XWayland.
#[cfg(feature = "gui")]
pub fn window_system(frame: &eframe::Frame) -> String {
    use eframe::wgpu::rwh::{HasWindowHandle, RawWindowHandle};
    let raw = match frame.window_handle() {
        Ok(h) => h.as_raw(),
        Err(_) => return session_from_env(),
    };
    let wayland_session = std::env::var_os("WAYLAND_DISPLAY").is_some();
    match raw {
        RawWindowHandle::Wayland(_) => "Wayland (native)".into(),
        RawWindowHandle::Xlib(_) | RawWindowHandle::Xcb(_) if wayland_session => {
            "X11 via XWayland (native Wayland not used)".into()
        }
        RawWindowHandle::Xlib(_) | RawWindowHandle::Xcb(_) => "X11".into(),
        RawWindowHandle::AppKit(_) => "macOS (AppKit)".into(),
        RawWindowHandle::AndroidNdk(_) => "Android".into(),
        RawWindowHandle::Win32(_) => "Windows".into(),
        other => format!("{other:?}"),
    }
}
