//! 去掉悬浮球原生窗口的系统边框与阴影。
//!
//! egui immediate viewport 在部分 Windows/eframe 组合下仍会创建带 `WS_CAPTION`、
//! `WS_THICKFRAME` 的顶层窗口。客户区虽然透明，但 DWM 会在外面画一圈圆角边框/阴影，
//! 看起来像一块半透明方框。这里按窗口标题找到悬浮球 HWND，直接改成 `WS_POPUP`。

/// 把指定标题的窗口改成无边框弹出窗口。
#[cfg(windows)]
pub fn make_borderless(title: &str) -> Option<(i32, i32)> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Dwm::{
        DWMNCRP_DISABLED, DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE, DWMWA_NCRENDERING_POLICY,
        DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DwmSetWindowAttribute,
    };
    use windows_sys::Win32::Graphics::Gdi::{CreateRectRgn, SetWindowRgn};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, GWL_STYLE, GetClientRect, GetWindowLongW, SWP_FRAMECHANGED, SWP_NOACTIVATE,
        SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetWindowLongW, SetWindowPos, WS_BORDER, WS_CAPTION,
        WS_DLGFRAME, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_POPUP, WS_SYSMENU, WS_THICKFRAME,
    };

    let title: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let hwnd = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
    if hwnd.is_null() {
        return None;
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
    let mut client = RECT::default();
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

        // Windows 11 仍可能给顶层窗口画 1px DWM 边框；显式关闭边框颜色和圆角。
        let none: u32 = DWMWA_COLOR_NONE;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR as u32,
            (&none as *const u32).cast(),
            std::mem::size_of::<u32>() as u32,
        );
        let policy = DWMNCRP_DISABLED;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_NCRENDERING_POLICY as u32,
            (&policy as *const i32).cast(),
            std::mem::size_of::<i32>() as u32,
        );
        let corner = DWMWCP_DONOTROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            (&corner as *const i32).cast(),
            std::mem::size_of::<i32>() as u32,
        );

        // 某些 Windows 版本即使关掉 DWM 边框，客户区顶部仍会保留 1px 亮线。
        // 把客户区四周各裁掉 1px，圆形本身留了边距，不会影响绘制。
        if GetClientRect(hwnd, &mut client) != 0 {
            let region = CreateRectRgn(1, 1, client.right - 1, client.bottom - 1);
            if !region.is_null() {
                let _ = SetWindowRgn(hwnd, region, 1);
            }
        }
    }
    Some((client.right, client.bottom))
}

/// 非 Windows 平台暂不处理。
#[cfg(not(windows))]
pub fn make_borderless(_title: &str) -> Option<(i32, i32)> {
    None
}
