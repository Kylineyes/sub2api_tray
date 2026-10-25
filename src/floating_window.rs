//! 去掉悬浮球原生窗口的系统边框与阴影。
//!
//! egui immediate viewport 在部分 Windows/eframe 组合下仍会创建带 `WS_CAPTION`、
//! `WS_THICKFRAME` 的顶层窗口。客户区虽然透明，但 DWM 会在外面画一圈圆角边框/阴影，
//! 看起来像一块半透明方框。这里按窗口标题找到悬浮球 HWND，直接改成 `WS_POPUP`。

/// 把指定标题的窗口改成无边框弹出窗口。
#[cfg(windows)]
pub fn make_borderless(title: &str) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, GWL_STYLE, GetWindowLongW, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE,
        SWP_NOSIZE, SWP_NOZORDER, SetWindowLongW, SetWindowPos, WS_BORDER, WS_CAPTION, WS_DLGFRAME,
        WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_POPUP, WS_SYSMENU, WS_THICKFRAME,
    };

    let title: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let hwnd = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
    if hwnd.is_null() {
        return false;
    }

    let style = unsafe { GetWindowLongW(hwnd, GWL_STYLE) } as u32;
    let remove = WS_CAPTION
        | WS_THICKFRAME
        | WS_SYSMENU
        | WS_MINIMIZEBOX
        | WS_MAXIMIZEBOX
        | WS_BORDER
        | WS_DLGFRAME;
    let new_style = (style & !remove) | WS_POPUP;
    unsafe {
        SetWindowLongW(hwnd, GWL_STYLE, new_style as i32);
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
    true
}

/// 非 Windows 平台暂不处理。
#[cfg(not(windows))]
pub fn make_borderless(_title: &str) -> bool {
    false
}
