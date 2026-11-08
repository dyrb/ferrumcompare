use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::{FILETIME, HWND, RECT, SYSTEMTIME};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}
pub fn path_wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain([0]).collect()
}
pub unsafe fn send(h: HWND, m: u32, w: usize, l: isize) -> isize {
    unsafe { SendMessageW(h, m, w, l) }
}
pub fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
    RECT { left, top, right, bottom }
}
pub fn client(h: HWND) -> RECT {
    let mut r = RECT::default();
    unsafe {
        GetClientRect(h, &mut r);
    }
    r
}
pub fn fill(dc: HDC, r: &RECT, color: u32) {
    unsafe {
        SetDCBrushColor(dc, color);
        FillRect(dc, r, GetStockObject(DC_BRUSH) as HBRUSH);
    }
}

pub fn frame(dc: HDC, r: &RECT, color: u32, thickness: i32) {
    fill(dc, &rect(r.left, r.top, r.right, r.top + thickness), color);
    fill(dc, &rect(r.left, r.bottom - thickness, r.right, r.bottom), color);
    fill(dc, &rect(r.left, r.top, r.left + thickness, r.bottom), color);
    fill(dc, &rect(r.right - thickness, r.top, r.right, r.bottom), color);
}
pub fn text(dc: HDC, s: &str, r: &RECT, color: u32, flags: u32) {
    if s.is_empty() || r.right <= r.left || r.bottom <= r.top {
        return;
    }
    let w = wide(s);
    let mut r = *r;
    unsafe {
        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, color);
        DrawTextW(dc, w.as_ptr(), (w.len() - 1) as i32, &mut r, DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS | flags);
    }
}
pub fn edit_path(h: HWND) -> PathBuf {
    let n = unsafe { GetWindowTextLengthW(h) }.max(0) as usize;
    let mut w = vec![0u16; n + 1];
    let got = unsafe { GetWindowTextW(h, w.as_mut_ptr(), w.len() as i32) }.max(0) as usize;
    PathBuf::from(std::ffi::OsString::from_wide(&w[..got]))
}
pub fn set_path(h: HWND, path: &Path) {
    let w = path_wide(path);
    unsafe {
        SetWindowTextW(h, w.as_ptr());
    }
}
pub fn set_window_icons(h: HWND, dpi: u32) {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::HiDpi::GetSystemMetricsForDpi;
    unsafe {
        for (kind, x, y) in [(ICON_SMALL, SM_CXSMICON, SM_CYSMICON), (ICON_BIG, SM_CXICON, SM_CYICON)] {
            // LR_SHARED icons belong to Windows and can be reused across windows.
            let icon = LoadImageW(
                GetModuleHandleW(std::ptr::null()),
                std::ptr::without_provenance(1),
                IMAGE_ICON,
                GetSystemMetricsForDpi(x, dpi),
                GetSystemMetricsForDpi(y, dpi),
                LR_SHARED,
            );
            if !icon.is_null() {
                send(h, WM_SETICON, kind as usize, icon as isize);
            }
        }
    }
}
pub fn message(hwnd: HWND, text: &str, error: bool) {
    unsafe {
        MessageBoxW(
            hwnd,
            wide(text).as_ptr(),
            wide("FerrumCompare").as_ptr(),
            MB_OK | if error { MB_ICONERROR } else { MB_ICONINFORMATION },
        );
    }
}
pub fn filetime(n: u64) -> String {
    if n == 0 {
        return String::new();
    }
    let file = FILETIME { dwLowDateTime: n as u32, dwHighDateTime: (n >> 32) as u32 };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    unsafe {
        use windows_sys::Win32::System::Time::*;
        if FileTimeToSystemTime(&file, &mut utc) == 0 || SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) == 0 {
            return String::new();
        }
    }
    if crate::lang::ru() {
        format!("{:02}.{:02}.{:04} {:02}:{:02}:{:02}", local.wDay, local.wMonth, local.wYear, local.wHour, local.wMinute, local.wSecond)
    } else {
        format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute, local.wSecond)
    }
}
