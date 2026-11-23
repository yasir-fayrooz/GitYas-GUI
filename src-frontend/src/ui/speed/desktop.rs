//! What speed mode needs from the desktop, per platform (safe wrappers only: Win32 through `winsafe`, AppKit through
//! `objc2-app-kit`):
//!
//! * The app that had the focus before the summon shortcut brought speed mode forward, so tucking the panel away
//!   hands the focus back to it (e.g. back to the editor you were typing in). Windows: the foreground window.
//!   macOS: the frontmost application. Elsewhere nothing is remembered; the window manager decides.
//! * The screen's work area (without the taskbar / dock / menu bar), so the panel is placed where it can be seen.

/// Another app's window / application that had the focus. Never GitYas itself.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
pub struct PreviousApp {
    #[cfg(windows)]
    window: winsafe::HWND,
    #[cfg(target_os = "macos")]
    app: objc2::rc::Retained<objc2_app_kit::NSRunningApplication>,
}

/// The app in front right now, unless it is GitYas.
#[cfg(windows)]
pub fn capture() -> Option<PreviousApp> {
    let window = winsafe::HWND::GetForegroundWindow()?;
    let (_, pid) = window.GetWindowThreadProcessId();
    (pid != std::process::id()).then_some(PreviousApp { window })
}

/// Gives the focus back to `previous` (GitYas has it, so the system lets it pass the focus on).
#[cfg(windows)]
pub fn restore(previous: PreviousApp) {
    if !previous.window.SetForegroundWindow() {
        tracing::debug!("could not give the focus back to the previous window");
    }
}

#[cfg(target_os = "macos")]
pub fn capture() -> Option<PreviousApp> {
    let app = objc2_app_kit::NSWorkspace::sharedWorkspace().frontmostApplication()?;
    let own = i64::from(std::process::id());
    (i64::from(app.processIdentifier()) != own).then_some(PreviousApp { app })
}

#[cfg(target_os = "macos")]
pub fn restore(previous: PreviousApp) {
    if !previous.app.activateWithOptions(objc2_app_kit::NSApplicationActivationOptions(0)) {
        tracing::debug!("could not give the focus back to the previous app");
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn capture() -> Option<PreviousApp> {
    None
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn restore(_previous: PreviousApp) {}

/// The usable part of the screen the window is on, in egui points with the primary screen's top-left at (0, 0):
/// without the taskbar (Windows) or the dock and menu bar (macOS). `None` when unknown (then a fixed margin is used).
#[cfg(windows)]
pub fn work_area(near: egui::Pos2, pixels_per_point: f32) -> Option<egui::Rect> {
    use winsafe::{HMONITOR, POINT, co};
    // A point inside the window, in whole pixels.
    let point = POINT {
        x: (near.x * pixels_per_point) as i32,
        y: (near.y * pixels_per_point) as i32,
    };
    let info = HMONITOR::MonitorFromPoint(point, co::MONITOR::DEFAULTTONEAREST)
        .GetMonitorInfo()
        .ok()?;
    let work = info.rcWork;
    let to_points = |v: i32| v as f32 / pixels_per_point;
    Some(egui::Rect::from_min_max(
        egui::pos2(to_points(work.left), to_points(work.top)),
        egui::pos2(to_points(work.right), to_points(work.bottom)),
    ))
}

#[cfg(target_os = "macos")]
pub fn work_area(_near: egui::Pos2, _pixels_per_point: f32) -> Option<egui::Rect> {
    use objc2_app_kit::NSScreen;
    let mtm = objc2::MainThreadMarker::new()?;
    // AppKit's coordinates start at the bottom-left of the primary screen; egui's (winit's) at its top-left.
    let primary_height = NSScreen::screens(mtm).firstObject()?.frame().size.height;
    let visible = NSScreen::mainScreen(mtm)?.visibleFrame();
    let top = primary_height - (visible.origin.y + visible.size.height);
    Some(egui::Rect::from_min_size(
        egui::pos2(visible.origin.x as f32, top as f32),
        egui::vec2(visible.size.width as f32, visible.size.height as f32),
    ))
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn work_area(_near: egui::Pos2, _pixels_per_point: f32) -> Option<egui::Rect> {
    None
}
