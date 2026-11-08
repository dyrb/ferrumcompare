use crate::{
    app::{Layout, with},
    lang::{self, S, t},
    theme,
    util::*,
};
use std::ptr::null;
use std::sync::atomic::Ordering::Relaxed;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::HiDpi::{GetSystemMetricsForDpi, SystemParametersInfoForDpi};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, GetFocus};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

#[derive(Clone, Copy)]
pub struct Metrics {
    pub margin: i32,
    gap: i32,
    height: i32,
    text_height: i32,
    menu_height: i32,
    menus: [i32; 3],
    toolbar: [i32; 5],
    filter: i32,
    path_label: i32,
    minimum: i32,
}

impl Metrics {
    pub fn for_dpi(dpi: u32) -> Self {
        let s = |n| n * dpi as i32 / 96;
        Self {
            margin: s(8),
            gap: s(6),
            height: s(24),
            text_height: s(13),
            menu_height: s(22),
            menus: [s(56), s(48), s(72)],
            toolbar: [s(82), s(190), s(82), s(140), s(156)],
            filter: s(208),
            path_label: s(48),
            minimum: s(840),
        }
    }

    pub fn minimum_width(self) -> i32 {
        self.minimum.max(2 * self.margin + self.toolbar.iter().sum::<i32>() + 4 * self.gap)
    }
}

/// Measure translated labels only when the language or DPI changes.
pub fn measure() {
    let Some((w, dpi, font)) = with(|a| (a.w, a.dpi, a.font)) else { return };
    let s = |n| n * dpi as i32 / 96;
    let mut m = Metrics::for_dpi(dpi);
    unsafe {
        let dc = GetDC(w.main);
        if !dc.is_null() {
            let old = SelectObject(dc, font);
            let mut tm: TEXTMETRICW = std::mem::zeroed();
            if GetTextMetricsW(dc, &mut tm) != 0 {
                m.text_height = tm.tmHeight;
            }
            let width = |label| {
                let text = wide(t(label));
                let mut size = SIZE::default();
                GetTextExtentPoint32W(dc, text.as_ptr(), (text.len() - 1) as i32, &mut size);
                size.cx
            };
            m.height = s(24).max(m.text_height + s(8));
            m.menu_height = s(22).max(m.text_height + s(6));
            m.menus = [S::File, S::View, S::Help].map(|label| width(label) + s(16));
            m.path_label = width(S::Left).max(width(S::Right)) + s(6);
            m.toolbar = [
                (width(S::Compare) + s(18)).max(s(76)),
                width(S::Contents) + s(28),
                (width(S::Stop) + s(18)).max(s(72)),
                (width(S::Swap) + s(18)).max(s(108)),
                s(132).max([S::Auto, S::Walk, S::Mft].map(width).into_iter().max().unwrap_or(0) + s(28)),
            ];
            m.filter = [S::All, S::Changes, S::Equal, S::Uncertain].map(width).into_iter().max().unwrap_or(0) + s(28);
            SelectObject(dc, old);
            ReleaseDC(w.main, dc);
        }
    }
    with(|a| a.metrics = m);
    align_controls();
    crate::viewer::refresh_preferences();
}

fn align_controls() {
    let Some((w, dpi, m)) = with(|a| (a.w, a.dpi, a.metrics)) else { return };
    unsafe {
        for edit in [w.left, w.right, w.search] {
            send(edit, EM_SETMARGINS, (EC_LEFTMARGIN | EC_RIGHTMARGIN) as usize, 0);
        }
        for combo in [w.backend, w.filter] {
            let edge = GetSystemMetricsForDpi(SM_CYEDGE, dpi);
            send(combo, CB_SETITEMHEIGHT, 0, (m.text_height + 4 * dpi as i32 / 96) as isize);
            send(combo, CB_SETITEMHEIGHT, usize::MAX, (m.height - 2 * edge).max(m.text_height) as isize);
            let mut r = RECT::default();
            if GetWindowRect(combo, &mut r) != 0 {
                let current = send(combo, CB_GETITEMHEIGHT, usize::MAX, 0) as i32;
                let desired = (current + m.height - (r.bottom - r.top)).max(m.text_height);
                if current != desired {
                    send(combo, CB_SETITEMHEIGHT, usize::MAX, desired as isize);
                }
            }
        }
    }
}

pub fn font(dpi: u32) -> HFONT {
    let mut metrics: NONCLIENTMETRICSW = unsafe { std::mem::zeroed() };
    metrics.cbSize = size_of::<NONCLIENTMETRICSW>() as u32;
    unsafe {
        SystemParametersInfoForDpi(SPI_GETNONCLIENTMETRICS, metrics.cbSize, (&mut metrics as *mut NONCLIENTMETRICSW).cast(), 0, dpi);
        CreateFontIndirectW(&metrics.lfMessageFont)
    }
}

pub fn dark(choice: u8) -> bool {
    match choice {
        1 => false,
        2 => true,
        _ => theme::system_dark(),
    }
}

pub fn apply_theme() {
    let Some((w, pal)) = with(|a| {
        a.pal = if dark(a.settings.theme) { theme::DARK } else { theme::LIGHT };
        (a.w, a.pal)
    }) else {
        return;
    };
    theme::set_app_dark(pal.dark);
    theme::set_title_dark(w.main, pal.dark);
    for h in [w.left, w.right, w.search, w.backend, w.filter] {
        theme::style_control(h, pal.dark, theme::Kind::Cfd);
    }
    crate::list::apply_theme();
    align_controls();
    crate::viewer::refresh_preferences();
    unsafe {
        RedrawWindow(w.main, null(), std::ptr::null_mut(), RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN);
    }
}

pub fn labels() {
    let Some(w) = with(|a| a.w) else { return };
    for (h, s) in [(w.compare, S::Compare), (w.contents, S::Contents), (w.stop, S::Stop), (w.swap, S::Swap)] {
        unsafe {
            SetWindowTextW(h, wide(t(s)).as_ptr());
        }
    }
    unsafe {
        send(w.search, EM_SETCUEBANNER, 1, wide(t(S::Search)).as_ptr() as isize);
        send(w.backend, CB_RESETCONTENT, 0, 0);
        for s in [S::Auto, S::Walk, S::Mft] {
            send(w.backend, CB_ADDSTRING, 0, wide(t(s)).as_ptr() as isize);
        }
        send(w.filter, CB_RESETCONTENT, 0, 0);
        for s in [S::All, S::Changes, S::Equal, S::Uncertain] {
            send(w.filter, CB_ADDSTRING, 0, wide(t(s)).as_ptr() as isize);
        }
    }
    if let Some((backend, filter, dpi)) = with(|a| (a.settings.backend, a.filter, a.dpi)) {
        unsafe {
            send(w.backend, CB_SETCURSEL, backend as usize, 0);
            send(w.filter, CB_SETCURSEL, filter as usize, 0);
        }
        for h in w.lists {
            crate::list::columns(h, dpi);
        }
    }
    measure();
    layout();
}

fn compute(width: i32, height: i32, dpi: u32, split: f32, m: Metrics) -> Layout {
    let s = |n| n * dpi as i32 / 96;
    let margin = m.margin;
    let gap = m.gap;
    let pane_space = (width - 2 * margin - s(6)).max(0);
    let boundary = margin + (pane_space as f32 * split.clamp(0.25, 0.75)) as i32;
    let path_top = m.menu_height + s(4);
    let action_top = path_top + m.height + gap;
    let search_top = action_top + m.height + gap;
    let body_top = search_top + m.height + gap;
    let line = (m.text_height + s(6)).max(s(20));
    let footer = rect(margin, height - line, width - margin, height);
    let status = rect(margin, footer.top - line, width - margin, footer.top);
    let progress = rect(margin, status.top - s(4), width - margin, status.top - s(2));
    let bottom = (progress.top - s(4)).max(body_top);
    let lists = [rect(margin, body_top, boundary, bottom), rect(boundary + s(6), body_top, width - margin, bottom)];
    let mut out = Layout {
        split: rect(boundary, body_top, boundary + s(6), bottom),
        lists,
        status,
        progress,
        footer,
        search: rect(margin, search_top, width - margin - m.filter - gap, search_top + m.height),
        filter: rect(width - margin - m.filter, search_top, width - margin, search_top + m.height),
        ..Default::default()
    };
    let mut x = margin;
    for (i, width) in m.menus.into_iter().enumerate() {
        out.menus[i] = rect(x, 0, x + width, m.menu_height);
        x += width;
    }
    for (i, pane) in lists.into_iter().enumerate() {
        out.path_labels[i] = rect(pane.left, path_top, pane.left + m.path_label, path_top + m.height);
        out.paths[i] = rect(out.path_labels[i].right, path_top, pane.right - gap - m.height, path_top + m.height);
        out.browse[i] = rect(pane.right - m.height, path_top, pane.right, path_top + m.height);
    }
    x = margin;
    for (i, width) in m.toolbar.into_iter().enumerate() {
        out.toolbar[i] = rect(x, action_top, x + width, action_top + m.height);
        x += width + gap;
    }
    out
}

fn edit_rect(outer: RECT, dpi: u32, metrics: Metrics) -> RECT {
    let padding = 4 * dpi as i32 / 96;
    let height = (metrics.text_height + 2 * dpi as i32 / 96).min(outer.bottom - outer.top);
    let top = outer.top + (outer.bottom - outer.top - height) / 2;
    rect(outer.left + padding, top, outer.right - padding, top + height)
}

pub fn layout() {
    let Some((w, dpi, split, m)) = with(|a| (a.w, a.dpi, a.settings.split, a.metrics)) else { return };
    let c = client(w.main);
    if c.right == 0 || c.bottom == 0 {
        return;
    }
    let layout = compute(c.right, c.bottom, dpi, split, m);
    with(|a| a.layout = layout.clone());
    let dropdown = |mut r: RECT| {
        r.bottom = r.top + m.height + (m.text_height + 4 * dpi as i32 / 96) * 6;
        r
    };
    let positions = [
        (w.left, edit_rect(layout.paths[0], dpi, m)),
        (w.right, edit_rect(layout.paths[1], dpi, m)),
        (w.browse[0], layout.browse[0]),
        (w.browse[1], layout.browse[1]),
        (w.compare, layout.toolbar[0]),
        (w.contents, layout.toolbar[1]),
        (w.stop, layout.toolbar[2]),
        (w.swap, layout.toolbar[3]),
        (w.backend, dropdown(layout.toolbar[4])),
        (w.search, edit_rect(layout.search, dpi, m)),
        (w.filter, dropdown(layout.filter)),
        (w.lists[0], layout.lists[0]),
        (w.lists[1], layout.lists[1]),
    ];
    unsafe {
        let mut batch = BeginDeferWindowPos(positions.len() as i32);
        for &(h, r) in &positions {
            if batch.is_null() {
                break;
            }
            batch = DeferWindowPos(
                batch,
                h,
                std::ptr::null_mut(),
                r.left,
                r.top,
                (r.right - r.left).max(0),
                (r.bottom - r.top).max(0),
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        let moved = !batch.is_null() && EndDeferWindowPos(batch) != 0;
        if !moved {
            for &(h, r) in &positions {
                SetWindowPos(
                    h,
                    std::ptr::null_mut(),
                    r.left,
                    r.top,
                    (r.right - r.left).max(0),
                    (r.bottom - r.top).max(0),
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
        }
        for list in w.lists {
            crate::list::fit_columns(list, dpi);
        }
        InvalidateRect(w.main, null(), 0);
    }
}

pub fn enabled() {
    let Some((w, busy, scanning, cancelled)) =
        with(|a| (a.w, a.busy(), a.progress.is_some(), a.progress.as_ref().is_some_and(|p| p.cancel.load(Relaxed))))
    else {
        return;
    };
    unsafe {
        for h in [w.left, w.right, w.browse[0], w.browse[1], w.compare, w.contents, w.swap, w.backend] {
            EnableWindow(h, (!busy) as i32);
        }
        EnableWindow(w.stop, (scanning && !cancelled) as i32);
    }
}

pub fn paint(hwnd: HWND) {
    unsafe {
        let mut ps: PAINTSTRUCT = std::mem::zeroed();
        let dc = BeginPaint(hwnd, &mut ps);
        let c = client(hwnd);
        let memory = CreateCompatibleDC(dc);
        let bitmap = CreateCompatibleBitmap(dc, c.right.max(1), c.bottom.max(1));
        let old_bitmap = SelectObject(memory, bitmap);
        with(|a| {
            let old_font = SelectObject(memory, a.font);
            fill(memory, &c, a.pal.bg);
            for (r, s) in a.layout.menus.iter().zip([S::File, S::View, S::Help]) {
                text(memory, t(s), r, a.pal.text, DT_CENTER);
            }
            for (r, label) in a.layout.path_labels.iter().zip([S::Left, S::Right]) {
                text(memory, t(label), r, a.pal.dim, 0);
            }
            let focus = GetFocus();
            for (i, (r, control)) in
                [(a.layout.paths[0], a.w.left), (a.layout.paths[1], a.w.right), (a.layout.search, a.w.search)].into_iter().enumerate()
            {
                fill(memory, &r, a.pal.input);
                let highlighted = i < 2 && a.drop_hint.is_some_and(|h| h.accepted() && h.zone.contains(i));
                frame(memory, &r, if focus == control || highlighted { a.pal.accent } else { a.pal.border }, a.scale(1).max(1));
            }
            if let Some(hint) = a.drop_hint.filter(|h| h.accepted()) {
                for (i, r) in a.layout.lists.iter().enumerate() {
                    if hint.zone.contains(i) {
                        let d = a.scale(2).max(1);
                        frame(memory, &rect(r.left - d, r.top - d, r.right + d, r.bottom + d), a.pal.accent, d);
                    }
                }
            }
            let mut fraction = None;
            let status = if let Some(hint) = a.drop_hint {
                hint.text().into()
            } else if let Some(p) = &a.progress {
                let phase = p.phase.load(Relaxed);
                let label = if p.cancel.load(Relaxed) {
                    S::Stopping
                } else {
                    match phase {
                        1 => S::Reading,
                        2 => S::Matching,
                        3 => S::Checking,
                        _ => S::Compare,
                    }
                };
                if phase == 3 {
                    let total = p.total.load(Relaxed);
                    let done = p.done.load(Relaxed);
                    if total != 0 {
                        fraction = Some(done as f64 / total as f64);
                    }
                    format!("{}: {} / {}    {}", t(label), lang::number(done), lang::number(total), lang::size(p.bytes.load(Relaxed)))
                } else {
                    format!(
                        "{} ({}: {}, {}: {})",
                        t(label),
                        t(S::Dirs),
                        lang::number(p.dirs.load(Relaxed)),
                        t(S::Files),
                        lang::number(p.files.load(Relaxed))
                    )
                }
            } else if a.exporting {
                t(S::ExportBusy).into()
            } else if a.status.is_empty() {
                t(S::Ready).into()
            } else {
                a.status.clone()
            };
            text(memory, &status, &a.layout.status, a.pal.text, 0);
            fill(memory, &a.layout.progress, a.pal.border);
            if let Some(f) = fraction {
                let mut r = a.layout.progress;
                r.right = r.left + ((r.right - r.left) as f64 * f.clamp(0., 1.)) as i32;
                fill(memory, &r, a.pal.accent);
            }
            let splitter = a.layout.split;
            let y = (splitter.top + splitter.bottom) / 2;
            fill(memory, &rect(splitter.left + a.scale(2), y - a.scale(12), splitter.left + a.scale(2) + 1, y + a.scale(12)), a.pal.border);
            text(memory, &a.footer(), &a.layout.footer, a.pal.dim, 0);
            SelectObject(memory, old_font);
        });
        BitBlt(dc, 0, 0, c.right, c.bottom, memory, 0, 0, SRCCOPY);
        SelectObject(memory, old_bitmap);
        DeleteObject(bitmap);
        DeleteDC(memory);
        EndPaint(hwnd, &ps);
    }
}

pub unsafe fn draw_button(l: LPARAM) -> bool {
    unsafe {
        let d = &*(l as *const DRAWITEMSTRUCT);
        if d.CtlType != ODT_BUTTON {
            return false;
        }
        with(|a| {
            let saved = SaveDC(d.hDC);
            SelectObject(d.hDC, a.font);
            let disabled = d.itemState & ODS_DISABLED != 0;
            let pressed = d.itemState & ODS_SELECTED != 0;
            let primary = d.hwndItem == a.w.compare && !disabled;
            let checkbox = d.hwndItem == a.w.contents;
            fill(
                d.hDC,
                &d.rcItem,
                if checkbox {
                    a.pal.bg
                } else if primary {
                    a.pal.accent
                } else if pressed {
                    a.pal.sel
                } else {
                    a.pal.panel
                },
            );
            if !checkbox {
                frame(d.hDC, &d.rcItem, if primary { a.pal.accent } else { a.pal.border }, a.scale(1).max(1));
            }
            let mut r = d.rcItem;
            if checkbox {
                let top = r.top + (r.bottom - r.top - a.scale(14)) / 2;
                let mut check = rect(r.left + a.scale(4), top, r.left + a.scale(18), top + a.scale(14));
                DrawFrameControl(
                    d.hDC,
                    &mut check,
                    DFC_BUTTON,
                    DFCS_BUTTONCHECK | if a.settings.contents { DFCS_CHECKED } else { 0 } | if disabled { DFCS_INACTIVE } else { 0 },
                );
                r.left += a.scale(24);
            }
            let n = GetWindowTextLengthW(d.hwndItem).max(0);
            let mut title = vec![0u16; n as usize + 1];
            GetWindowTextW(d.hwndItem, title.as_mut_ptr(), n + 1);
            text(
                d.hDC,
                &String::from_utf16_lossy(&title[..n as usize]),
                &r,
                if disabled {
                    a.pal.dim
                } else if primary {
                    0xffffff
                } else if pressed {
                    a.pal.sel_text
                } else {
                    a.pal.text
                },
                if d.hwndItem == a.w.contents { 0 } else { DT_CENTER },
            );
            if d.itemState & ODS_FOCUS != 0 {
                let mut r = d.rcItem;
                InflateRect(&mut r, -3, -3);
                DrawFocusRect(d.hDC, &r);
            }
            RestoreDC(d.hDC, saved);
        });
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_grid_keeps_controls_aligned_when_resized_or_scaled() {
        for dpi in [96, 120, 144, 192, 240] {
            let scale = |n| n * dpi as i32 / 96;
            let metrics = Metrics::for_dpi(dpi);
            for width in [840, 1180, 1600] {
                for split in [0.25, 0.5, 0.75] {
                    let width = scale(width);
                    let height = scale(500);
                    let l = compute(width, height, dpi, split, metrics);
                    for r in
                        l.paths.into_iter().chain(l.browse).chain(l.toolbar).chain(l.lists).chain([l.search, l.filter, l.status, l.footer])
                    {
                        assert!(r.left >= 0 && r.right <= width && r.left < r.right);
                        assert!(r.top >= 0 && r.bottom <= height && r.top < r.bottom);
                    }
                    for pair in l.toolbar.windows(2) {
                        assert!(pair[0].right < pair[1].left);
                        assert_eq!(pair[0].top, pair[1].top);
                        assert_eq!(pair[0].bottom, pair[1].bottom);
                    }
                    for i in 0..2 {
                        assert_eq!(l.paths[i].left, l.path_labels[i].right);
                        assert!(l.paths[i].right < l.browse[i].left);
                        assert_eq!(l.paths[i].bottom, l.browse[i].bottom);
                        assert_eq!(l.browse[i].right, l.lists[i].right);
                        let edit = edit_rect(l.paths[i], dpi, metrics);
                        assert!(edit.left > l.paths[i].left && edit.right < l.paths[i].right);
                        assert!(edit.top > l.paths[i].top && edit.bottom < l.paths[i].bottom);
                        assert!((edit.top + edit.bottom - l.paths[i].top - l.paths[i].bottom).abs() <= 1);
                    }
                    assert_eq!(l.search.top, l.filter.top);
                    assert_eq!(l.search.bottom, l.filter.bottom);
                    assert!(l.search.right < l.filter.left);
                    assert!(l.filter.bottom < l.lists[0].top);
                    assert!(l.lists[0].right < l.lists[1].left);
                    assert!(l.lists[0].bottom < l.progress.top);
                    assert_eq!(l.status.bottom, l.footer.top);
                }
            }
        }
        let l = compute(1180, 720, 96, 0.5, Metrics::for_dpi(96));
        assert_eq!(l.lists[0].top, 116);
    }
}
