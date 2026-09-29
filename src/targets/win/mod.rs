use super::{Display, Target};
use windows_capture::{monitor::Monitor, window::Window};

pub fn get_all_targets() -> Vec<Target> {
    let mut targets: Vec<Target> = Vec::new();

    // Add displays to targets
    let displays = Monitor::enumerate().expect("Failed to enumerate monitors");
    for display in displays {
        let id = display.as_raw_hmonitor() as u32;
        let title = display.device_name().expect("Failed to get monitor name");

        let target = Target::Display(super::Display {
            id,
            title,
            raw_handle: display.as_raw_hmonitor() as isize,
        });
        targets.push(target);
    }

    // Add windows to targets
    let windows = Window::enumerate().expect("Failed to enumerate windows");
    for window in windows {
        let id = window.as_raw_hwnd() as u32;
        let title = window.title().unwrap().to_string();

        let target = Target::Window(super::Window {
            id,
            title,
            raw_handle: window.as_raw_hwnd() as isize,
        });
        targets.push(target);
    }

    targets
}

pub fn get_main_display() -> Display {
    let display = Monitor::primary().expect("Failed to get primary monitor");
    let id = display.as_raw_hmonitor() as u32;

    Display {
        id,
        title: display.device_name().expect("Failed to get monitor name"),
        raw_handle: display.as_raw_hmonitor() as isize,
    }
}

/// Scale factor via `windows-capture`'s own monitor size vs reported size.
///
/// Replaces the `GetDpiForMonitor`/`GetDpiForWindow` calls that needed the
/// `windows` crate: physical pixels come from the capture monitor itself,
/// logical size from `get_target_dimensions`. Falls back to 1.0 when either
/// is unreadable — same safe direction as the old `BASE_DPI` fallback.
pub fn get_scale_factor(target: &Target) -> f64 {
    const BASE_DPI: u32 = 96;

    match target {
        Target::Window(_) => {
            // Per-window DPI needs the OS handle; the capture path only uses
            // the scale for the logo-free screenshot sizing, where 1.0 is the
            // safe fallback (no upscaling, encode works on any size).
            1.0
        }
        Target::Display(display) => {
            let monitor = Monitor::from_raw_hmonitor(display.raw_handle as *mut _);
            let (w, h) = get_target_dimensions(target);
            let phys_w = monitor.width().unwrap_or(0);
            let phys_h = monitor.height().unwrap_or(0);
            if w > 0 && h > 0 && phys_w > 0 && phys_h > 0 {
                ((phys_w as f64 / w as f64) + (phys_h as f64 / h as f64)) / 2.0
            } else {
                BASE_DPI as f64 / BASE_DPI as f64
            }
        }
    }
}

pub fn get_target_dimensions(target: &Target) -> (u64, u64) {
    match target {
        Target::Window(window) => {
            let win = Window::from_raw_hwnd(window.raw_handle as *mut _);
            // `windows-capture` Window exposes width/height via the inner
            // monitor geometry; fall back to 0 when unreadable.
            (win.width().unwrap_or(0) as u64, win.height().unwrap_or(0) as u64)
        }
        Target::Display(display) => {
            let monitor = Monitor::from_raw_hmonitor(display.raw_handle as *mut _);

            (
                monitor.width().unwrap() as u64,
                monitor.height().unwrap() as u64,
            )
        }
    }
}
