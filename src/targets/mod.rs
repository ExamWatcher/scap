#[cfg(target_os = "macos")]
mod mac;

#[cfg(target_os = "windows")]
mod win;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "windows")]
unsafe impl Send for Window {}

#[derive(Debug, Clone)]
pub struct Window {
    pub id: u32,
    pub title: String,

    /// Raw HWND as `isize` — no `windows` crate needed. Converted at the
    /// single use site (`engine::win`) via `from_raw_hwnd`.
    #[cfg(target_os = "windows")]
    pub raw_handle: isize,

    #[cfg(target_os = "macos")]
    pub raw_handle: cidre::cg::WindowId,
}

#[cfg(target_os = "windows")]
unsafe impl Send for Display {}

#[derive(Debug, Clone)]
pub struct Display {
    pub id: u32,
    pub title: String,

    /// Raw HMONITOR as `isize` — no `windows` crate needed. Converted at the
    /// single use site (`engine::win`) via `from_raw_hmonitor`.
    #[cfg(target_os = "windows")]
    pub raw_handle: isize,

    #[cfg(target_os = "macos")]
    pub raw_handle: cidre::cg::DirectDisplayId,
}

#[derive(Debug, Clone)]
pub enum Target {
    Window(Window),
    Display(Display),
}

/// Returns a list of targets that can be captured
pub fn get_all_targets() -> Vec<Target> {
    #[cfg(target_os = "macos")]
    return mac::get_all_targets();

    #[cfg(target_os = "windows")]
    return win::get_all_targets();

    #[cfg(target_os = "linux")]
    return linux::get_all_targets();
}

#[allow(unused_variables)]
pub fn get_scale_factor(target: &Target) -> f64 {
    #[cfg(target_os = "macos")]
    return mac::get_scale_factor(target);

    #[cfg(target_os = "windows")]
    return win::get_scale_factor(target);

    #[cfg(target_os = "linux")]
    return 1.0;
}

pub fn get_main_display() -> Display {
    #[cfg(target_os = "macos")]
    return mac::get_main_display();

    #[cfg(target_os = "windows")]
    return win::get_main_display();

    #[cfg(target_os = "linux")]
    unreachable!();
}

#[allow(unused_variables)]
pub fn get_target_dimensions(target: &Target) -> (u64, u64) {
    #[cfg(target_os = "macos")]
    return mac::get_target_dimensions(target);

    #[cfg(target_os = "windows")]
    return win::get_target_dimensions(target);

    #[cfg(target_os = "linux")]
    unreachable!();
}
