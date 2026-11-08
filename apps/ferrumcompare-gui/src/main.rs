#![windows_subsystem = "windows"]
mod app;
mod cmds;
mod dialog;
mod drop_target;
mod lang;
mod list;
mod prompt;
mod settings;
mod shell;
mod theme;
mod ui;
mod util;
mod viewer;

use crate::{
    app::{App, Windows, with},
    util::*,
};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

pub const ID_COMPARE: i32 = 101;
const ID_STOP: i32 = 102;
const ID_CONTENTS: i32 = 103;
const ID_SWAP: i32 = 104;
const ID_LEFT: i32 = 105;
const ID_RIGHT: i32 = 106;
const ID_BROWSE_LEFT: i32 = 107;
const ID_BROWSE_RIGHT: i32 = 108;
const ID_BACKEND: i32 = 109;
const ID_SEARCH: i32 = 110;
const ID_FILTER: i32 = 111;

unsafe fn child(parent: HWND, class: windows_sys::core::PCWSTR, style: u32, id: i32, ex: u32) -> HWND {
    unsafe {
        CreateWindowExW(
            ex,
            class,
            wide("").as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS | WS_TABSTOP | style,
            0,
            0,
            10,
            10,
            parent,
            id as isize as HMENU,
            GetModuleHandleW(null()),
            null(),
        )
    }
}

unsafe fn create(hwnd: HWND) -> bool {
    unsafe {
        let settings = settings::Settings::load();
        lang::set(settings.language);
        let dpi = GetDpiForWindow(hwnd).max(96);
        set_window_icons(hwnd, dpi);
        let font = ui::font(dpi);
        let left = child(hwnd, WC_EDITW, ES_AUTOHSCROLL as u32, ID_LEFT, 0);
        let right = child(hwnd, WC_EDITW, ES_AUTOHSCROLL as u32, ID_RIGHT, 0);
        let browse = [
            child(hwnd, WC_BUTTONW, BS_OWNERDRAW as u32, ID_BROWSE_LEFT, 0),
            child(hwnd, WC_BUTTONW, BS_OWNERDRAW as u32, ID_BROWSE_RIGHT, 0),
        ];
        let compare = child(hwnd, WC_BUTTONW, BS_OWNERDRAW as u32, ID_COMPARE, 0);
        let contents = child(hwnd, WC_BUTTONW, BS_OWNERDRAW as u32, ID_CONTENTS, 0);
        let stop = child(hwnd, WC_BUTTONW, BS_OWNERDRAW as u32, ID_STOP, 0);
        let swap = child(hwnd, WC_BUTTONW, BS_OWNERDRAW as u32, ID_SWAP, 0);
        let backend = child(hwnd, WC_COMBOBOXW, CBS_DROPDOWNLIST as u32 | WS_VSCROLL, ID_BACKEND, 0);
        let search = child(hwnd, WC_EDITW, ES_AUTOHSCROLL as u32, ID_SEARCH, 0);
        let filter = child(hwnd, WC_COMBOBOXW, CBS_DROPDOWNLIST as u32 | WS_VSCROLL, ID_FILTER, 0);
        let style = LVS_REPORT | LVS_OWNERDATA | LVS_SINGLESEL | LVS_SHOWSELALWAYS | LVS_SHAREIMAGELISTS;
        let lists = [child(hwnd, WC_LISTVIEWW, style, 201, 0), child(hwnd, WC_LISTVIEWW, style, 202, 0)];
        let windows = Windows { main: hwnd, left, right, browse, compare, contents, stop, swap, backend, search, filter, lists };
        let controls = [left, right, browse[0], browse[1], compare, contents, stop, swap, backend, search, filter, lists[0], lists[1]];
        if controls.iter().any(|h| h.is_null()) {
            return false;
        }
        for h in controls {
            send(h, WM_SETFONT, font as usize, 0);
        }
        for h in browse {
            SetWindowTextW(h, wide("...").as_ptr());
        }
        let (tx, rx) = std::sync::mpsc::channel();
        app::install(App {
            w: windows,
            pal: if ui::dark(settings.theme) { theme::DARK } else { theme::LIGHT },
            settings,
            font,
            dpi,
            layout: Default::default(),
            metrics: ui::Metrics::for_dpi(dpi),
            pair: None,
            rows: Vec::new(),
            expanded: Default::default(),
            filter: 0,
            query: String::new(),
            display_scope: None,
            progress: None,
            exporting: false,
            status: String::new(),
            closing: false,
            dragging: false,
            drop_target: None,
            drop_hint: None,
            icons: Default::default(),
            syncing: false,
            summary: Default::default(),
            sort: (0, 0, false),
            tx,
            rx,
        });
        list::setup();
        ui::labels();
        ui::apply_theme();
        ui::enabled();
        match drop_target::Registration::new(hwnd, drop_target::Surface::Main) {
            Ok(target) => {
                with(|a| a.drop_target = Some(target));
            }
            Err(e) => {
                message(hwnd, &lang::error(&e), true);
            }
        }
        let args: Vec<_> = std::env::args_os().skip(1).collect();
        if let Some(path) = args.first() {
            set_path(left, std::path::Path::new(path));
        }
        if let Some(path) = args.get(1) {
            set_path(right, std::path::Path::new(path));
            PostMessageW(hwnd, WM_COMMAND, ID_COMPARE as usize, 0);
        }
        true
    }
}

unsafe extern "system" fn procedure(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        if let Some(result) = shell::menu_message(msg, w, l) {
            return result;
        }
        match msg {
            WM_CREATE => return if create(hwnd) { 0 } else { -1 },
            WM_ERASEBKGND => return 1,
            WM_PAINT => {
                ui::paint(hwnd);
                return 0;
            }
            WM_SIZE => {
                ui::layout();
                return 0;
            }
            WM_GETMINMAXINFO => {
                let dpi = GetDpiForWindow(hwnd).max(96);
                let mm = &mut *(l as *mut MINMAXINFO);
                let minimum = with(|a| a.metrics.minimum_width()).unwrap_or(840 * dpi as i32 / 96);
                let mut frame = RECT::default();
                AdjustWindowRectExForDpi(&mut frame, WS_OVERLAPPEDWINDOW, 0, WS_EX_CONTROLPARENT, dpi);
                mm.ptMinTrackSize = POINT { x: minimum + frame.right - frame.left, y: 400 * dpi as i32 / 96 + frame.bottom - frame.top };
                return 0;
            }
            WM_DPICHANGED => {
                let dpi = (w & 0xffff) as u32;
                set_window_icons(hwnd, dpi);
                let new_font = ui::font(dpi);
                if let Some((old, controls)) = with(|a| {
                    let old = a.font;
                    a.font = new_font;
                    a.dpi = dpi;
                    (
                        old,
                        [
                            a.w.left,
                            a.w.right,
                            a.w.browse[0],
                            a.w.browse[1],
                            a.w.compare,
                            a.w.contents,
                            a.w.stop,
                            a.w.swap,
                            a.w.backend,
                            a.w.search,
                            a.w.filter,
                            a.w.lists[0],
                            a.w.lists[1],
                        ],
                    )
                }) {
                    for h in controls {
                        send(h, WM_SETFONT, new_font as usize, 1);
                    }
                    DeleteObject(old);
                }
                let r = &*(l as *const RECT);
                SetWindowPos(hwnd, null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                if let Some(lists) = with(|a| a.w.lists) {
                    for h in lists {
                        list::columns(h, dpi);
                    }
                }
                ui::measure();
                ui::layout();
                return 0;
            }
            WM_SETTINGCHANGE | WM_THEMECHANGED => {
                if with(|a| a.settings.theme == 0 && a.pal.dark != theme::system_dark()).unwrap_or(false) {
                    ui::apply_theme();
                }
            }
            WM_COMMAND => {
                let id = (w & 0xffff) as i32;
                let code = (w >> 16) as u16;
                if matches!(id, ID_LEFT | ID_RIGHT | ID_SEARCH) && matches!(code as u32, EN_SETFOCUS | EN_KILLFOCUS) {
                    if let Some(r) = with(|a| match id {
                        ID_LEFT => a.layout.paths[0],
                        ID_RIGHT => a.layout.paths[1],
                        _ => a.layout.search,
                    }) {
                        InvalidateRect(hwnd, &r, 0);
                    }
                    return 0;
                }
                match id {
                    ID_COMPARE => cmds::start(),
                    ID_STOP => cmds::stop(),
                    ID_CONTENTS => {
                        with(|a| {
                            if !a.busy() {
                                a.settings.contents = !a.settings.contents;
                            }
                        });
                        cmds::invalidate_results();
                        InvalidateRect(l as HWND, null(), 1);
                    }
                    ID_SWAP => {
                        if let Some((controls, false)) = with(|a| (a.w, a.busy())) {
                            let left = edit_path(controls.left);
                            let right = edit_path(controls.right);
                            set_path(controls.left, &right);
                            set_path(controls.right, &left);
                        }
                    }
                    ID_BROWSE_LEFT => cmds::browse(0),
                    ID_BROWSE_RIGHT => cmds::browse(1),
                    ID_BACKEND if code == CBN_SELCHANGE as u16 => {
                        let choice = send(l as HWND, CB_GETCURSEL, 0, 0);
                        with(|a| a.settings.backend = choice.clamp(0, 2) as u8);
                    }
                    ID_FILTER if code == CBN_SELCHANGE as u16 => {
                        let choice = send(l as HWND, CB_GETCURSEL, 0, 0);
                        with(|a| a.filter = choice.clamp(0, 3) as u8);
                        list::refresh();
                    }
                    ID_SEARCH if code == EN_CHANGE as u16 => {
                        SetTimer(hwnd, 2, 160, None);
                    }
                    ID_LEFT | ID_RIGHT if code == EN_CHANGE as u16 => cmds::invalidate_results(),
                    _ => cmds::command(id as u32),
                }
                return 0;
            }
            WM_TIMER => {
                if w == 1 {
                    if let Some(r) = with(|a| a.layout.status) {
                        InvalidateRect(hwnd, &r, 0);
                    }
                    if let Some(r) = with(|a| a.layout.progress) {
                        InvalidateRect(hwnd, &r, 0);
                    }
                }
                if w == 2 {
                    KillTimer(hwnd, 2);
                    if let Some(h) = with(|a| a.w.search) {
                        let query = edit_path(h).to_string_lossy().to_lowercase();
                        let masks = if query.contains(['*', '?', ';']) || query.starts_with('!') {
                            fc_core::filter::Scope::from_display(&query).map(Some)
                        } else {
                            Ok(None)
                        };
                        match masks {
                            Ok(masks) => {
                                with(|a| {
                                    a.query = query;
                                    a.display_scope = masks;
                                });
                            }
                            Err(e) => {
                                with(|a| a.status = lang::error(&e));
                            }
                        }
                        list::refresh();
                    }
                }
                return 0;
            }
            app::WM_WORK => {
                cmds::receive();
                return 0;
            }
            drop_target::WM_FILES => {
                if let Some(pending) = with(|a| a.drop_target.as_ref().and_then(|t| t.take())).flatten() {
                    cmds::accept_drop(pending);
                }
                return 0;
            }
            WM_DROPFILES => {
                match drop_target::legacy(hwnd, drop_target::Surface::Main, w as _) {
                    Ok(pending) => cmds::accept_drop(pending),
                    Err(reason) => message(hwnd, drop_target::Feedback { zone: drop_target::Zone::Both, reason }.text(), false),
                }
                return 0;
            }
            WM_NOTIFY => return list::notify(l),
            WM_DRAWITEM => {
                if ui::draw_button(l) {
                    return 1;
                }
            }
            WM_CTLCOLOREDIT | WM_CTLCOLORSTATIC | WM_CTLCOLORLISTBOX => {
                if let Some(pal) = with(|a| a.pal) {
                    SetTextColor(w as HDC, pal.text);
                    SetBkColor(w as HDC, pal.input);
                    SetDCBrushColor(w as HDC, pal.input);
                    return GetStockObject(DC_BRUSH) as LRESULT;
                }
            }
            WM_LBUTTONDOWN => {
                let x = (l & 0xffff) as i16 as i32;
                let y = ((l >> 16) & 0xffff) as i16 as i32;
                let hit = with(|a| {
                    if let Some(i) = a.layout.menus.iter().position(|r| x >= r.left && x < r.right && y >= r.top && y < r.bottom) {
                        return Some(i);
                    }
                    let r = a.layout.split;
                    if x >= r.left && x < r.right && y >= r.top && y < r.bottom {
                        a.dragging = true;
                        return Some(3);
                    }
                    None
                })
                .flatten();
                if let Some(i) = hit {
                    if i < 3 {
                        cmds::menu(i);
                    } else {
                        SetCapture(hwnd);
                    }
                }
                return 0;
            }
            WM_MOUSEMOVE => {
                if with(|a| a.dragging).unwrap_or(false) {
                    let x = (l & 0xffff) as i16 as i32;
                    let c = client(hwnd);
                    with(|a| {
                        a.settings.split =
                            ((x - a.metrics.margin) as f32 / (c.right - 2 * a.metrics.margin - a.scale(6)).max(1) as f32).clamp(0.25, 0.75)
                    });
                    ui::layout();
                    return 0;
                }
            }
            WM_LBUTTONUP => {
                if with(|a| std::mem::replace(&mut a.dragging, false)).unwrap_or(false) {
                    ReleaseCapture();
                }
            }
            WM_CAPTURECHANGED => {
                with(|a| a.dragging = false);
            }
            WM_SETCURSOR => {
                let mut p = POINT::default();
                GetCursorPos(&mut p);
                ScreenToClient(hwnd, &mut p);
                let split = with(|a| a.layout.split).unwrap_or_default();
                if p.x >= split.left && p.x < split.right && p.y >= split.top && p.y < split.bottom {
                    SetCursor(LoadCursorW(null_mut(), IDC_SIZEWE));
                    return 1;
                }
            }
            WM_CONTEXTMENU => {
                let source = w as HWND;
                if with(|a| a.w.lists.contains(&source)).unwrap_or(false) {
                    cmds::context(source, POINT { x: (l & 0xffff) as i16 as i32, y: ((l >> 16) & 0xffff) as i16 as i32 });
                    return 0;
                }
            }
            WM_CLOSE => {
                if with(|a| {
                    if a.busy() {
                        a.closing = true;
                        true
                    } else {
                        false
                    }
                })
                .unwrap_or(false)
                {
                    cmds::stop();
                } else {
                    DestroyWindow(hwnd);
                }
                return 0;
            }
            WM_DESTROY => {
                drop(with(|a| a.drop_target.take()));
                viewer::close_all();
                KillTimer(hwnd, 1);
                KillTimer(hwnd, 2);
                if let Some(a) = app::take() {
                    a.settings.save();
                    DeleteObject(a.font);
                }
                PostQuitMessage(0);
                return 0;
            }
            _ => {}
        }
        DefWindowProcW(hwnd, msg, w, l)
    }
}

fn main() {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let ole = windows_sys::Win32::System::Ole::OleInitialize(null());
        let controls =
            INITCOMMONCONTROLSEX { dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_LISTVIEW_CLASSES | ICC_STANDARD_CLASSES };
        InitCommonControlsEx(&controls);
        let instance = GetModuleHandleW(null());
        let class = wide("FerrumCompareMain");
        let icon = LoadIconW(instance, std::ptr::without_provenance(1));
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(procedure),
            hInstance: instance,
            hIcon: icon,
            hIconSm: icon,
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassExW(&wc);
        let dpi = GetDpiForSystem().max(96) as i32;
        let mut work = RECT::default();
        SystemParametersInfoW(SPI_GETWORKAREA, 0, (&mut work as *mut RECT).cast(), 0);
        let width = (1180 * dpi / 96).min(work.right - work.left);
        let height = (720 * dpi / 96).min(work.bottom - work.top);
        let hwnd = CreateWindowExW(
            WS_EX_CONTROLPARENT,
            class.as_ptr(),
            wide("FerrumCompare").as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            width,
            height,
            null_mut(),
            null_mut(),
            instance,
            null(),
        );
        if hwnd.is_null() {
            message(null_mut(), &std::io::Error::last_os_error().to_string(), true);
            return;
        }
        ShowWindow(hwnd, SW_SHOW);
        let mut message: MSG = std::mem::zeroed();
        while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
            if viewer::route(&message) {
                continue;
            }
            if message.message == WM_KEYDOWN {
                if message.wParam == 0x74 {
                    cmds::start();
                    continue;
                }
                if message.wParam == 0x1b {
                    cmds::stop();
                    continue;
                }
            }
            if IsDialogMessageW(hwnd, &message) == 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        if ole >= 0 {
            windows_sys::Win32::System::Ole::OleUninitialize();
        }
    }
}
