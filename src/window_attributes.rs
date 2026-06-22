use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows_sys::Win32::Foundation::{HWND, LPARAM};
use windows_sys::Win32::Graphics::Dwm::{
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmSetWindowAttribute,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GWL_EXSTYLE, GetWindowLongPtrW, GetWindowThreadProcessId, LWA_ALPHA,
    SetLayeredWindowAttributes, SetWindowLongPtrW, WS_EX_LAYERED,
};

pub(crate) fn apply_window_attributes(
    frame: &eframe::Frame,
    opacity: f32,
    force_opacity: bool,
    applied_opacity: &mut Option<f32>,
    corner_applied_hwnd: &mut Option<isize>,
) {
    let opacity = opacity.clamp(0.35, 1.0);
    let Ok(window_handle) = frame.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(window_handle) = window_handle.as_raw() else {
        return;
    };
    let hwnd = window_handle.hwnd.get() as HWND;
    let hwnd_key = hwnd as isize;
    // SAFETY: hwnd comes from the active eframe Win32 window handle. The DWM
    // attribute pointer references a local constant for the duration of the call.
    unsafe {
        if *corner_applied_hwnd != Some(hwnd_key) {
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE as u32,
                std::ptr::from_ref(&DWMWCP_ROUND).cast(),
                std::mem::size_of_val(&DWMWCP_ROUND) as u32,
            );
            *corner_applied_hwnd = Some(hwnd_key);
        }
        if force_opacity
            || !applied_opacity.is_some_and(|current| (current - opacity).abs() < f32::EPSILON)
        {
            let extended_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, extended_style | WS_EX_LAYERED as isize);
            if SetLayeredWindowAttributes(hwnd, 0, (opacity * 255.0).round() as u8, LWA_ALPHA) != 0
            {
                *applied_opacity = Some(opacity);
            }
        }
    }
}

pub(crate) fn apply_rounding_to_process_windows() {
    // SAFETY: EnumWindows invokes this callback with the documented ABI and valid HWND values.
    unsafe extern "system" fn apply_rounding(hwnd: HWND, process_id: LPARAM) -> i32 {
        let mut window_process_id = 0;
        // SAFETY: EnumWindows provides a valid top-level hwnd for this callback.
        unsafe {
            GetWindowThreadProcessId(hwnd, &mut window_process_id);
        }
        if window_process_id != process_id as u32 {
            return 1;
        }
        // SAFETY: hwnd belongs to this process and the attribute pointer is valid
        // for the duration of the synchronous DwmSetWindowAttribute call.
        unsafe {
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE as u32,
                std::ptr::from_ref(&DWMWCP_ROUND).cast(),
                std::mem::size_of_val(&DWMWCP_ROUND) as u32,
            );
        }
        1
    }

    // SAFETY: The callback does not capture Rust references; lparam is only the
    // current process id cast through LPARAM for the duration of EnumWindows.
    unsafe {
        EnumWindows(Some(apply_rounding), std::process::id() as LPARAM);
    }
}
