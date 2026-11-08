use crate::{
    app::with,
    lang::{S, t},
    theme,
    util::{send, wide},
};
use fc_core::{NONE, State};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

unsafe extern "system" fn subclass(h: HWND, msg: u32, w: WPARAM, l: LPARAM, _: usize, _: usize) -> LRESULT {
    unsafe {
        if msg == WM_NOTIFY && l != 0 {
            let header = &*(l as *const NMHDR);
            if header.code == NM_CUSTOMDRAW {
                let draw = &mut *(l as *mut NMCUSTOMDRAW);
                if draw.dwDrawStage == CDDS_PREPAINT {
                    return CDRF_NOTIFYITEMDRAW as isize;
                }
                if draw.dwDrawStage == CDDS_ITEMPREPAINT {
                    SetTextColor(draw.hdc, with(|a| a.pal.text).unwrap_or(0));
                    return CDRF_DODEFAULT as isize;
                }
            }
        }
        let result = DefSubclassProc(h, msg, w, l);
        if matches!(msg, WM_VSCROLL | WM_MOUSEWHEEL | WM_KEYDOWN) {
            sync_scroll(h);
        }
        result
    }
}

pub fn setup() {
    let Some((lists, dpi)) = with(|a| (a.w.lists, a.dpi)) else { return };
    let mut info: SHFILEINFOW = unsafe { std::mem::zeroed() };
    let images = unsafe {
        SHGetFileInfoW(
            wide("folder").as_ptr(),
            0x10,
            &mut info,
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_SMALLICON | SHGFI_SYSICONINDEX | SHGFI_USEFILEATTRIBUTES,
        )
    };
    for list in lists {
        unsafe {
            send(list, LVM_SETEXTENDEDLISTVIEWSTYLE, 0, (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_INFOTIP) as isize);
            send(list, LVM_SETIMAGELIST, LVSIL_SMALL as usize, images as isize);
            SetWindowSubclass(list, Some(subclass), 1, 0);
        }
        columns(list, dpi);
    }
}

pub fn columns(list: HWND, dpi: u32) {
    unsafe {
        while send(list, LVM_DELETECOLUMN, 0, 0) != 0 {}
        for (i, (label, width)) in [(S::Name, 200), (S::State, 158), (S::Size, 76), (S::Modified, 136)].into_iter().enumerate() {
            let title = wide(t(label));
            let mut col: LVCOLUMNW = std::mem::zeroed();
            col.mask = LVCF_TEXT | LVCF_WIDTH | LVCF_FMT;
            col.pszText = title.as_ptr() as *mut _;
            col.cx = width * dpi as i32 / 96;
            col.fmt = if i == 2 { LVCFMT_RIGHT } else { LVCFMT_LEFT };
            send(list, LVM_INSERTCOLUMNW, i, &col as *const _ as isize);
        }
    }
    fit_columns(list, dpi);
}

/// Let the name use the remaining pane width; preserve user widths of other columns.
pub fn fit_columns(list: HWND, dpi: u32) {
    unsafe {
        let client = crate::util::client(list);
        let other = (1..4).map(|col| send(list, LVM_GETCOLUMNWIDTH, col, 0) as i32).sum::<i32>();
        let width = (client.right - other - 2).max(120 * dpi as i32 / 96);
        if send(list, LVM_GETCOLUMNWIDTH, 0, 0) as i32 != width {
            send(list, LVM_SETCOLUMNWIDTH, 0, width as isize);
        }
    }
}

pub fn apply_theme() {
    let Some((lists, pal)) = with(|a| (a.w.lists, a.pal)) else { return };
    for list in lists {
        theme::style_control(list, pal.dark, theme::Kind::Explorer);
        unsafe {
            send(list, LVM_SETBKCOLOR, 0, pal.list as isize);
            send(list, LVM_SETTEXTBKCOLOR, 0, pal.list as isize);
            send(list, LVM_SETTEXTCOLOR, 0, pal.text as isize);
            let header = send(list, LVM_GETHEADER, 0, 0) as HWND;
            theme::style_control(header, pal.dark, theme::Kind::ItemsView);
            InvalidateRect(list, std::ptr::null(), 1);
        }
    }
}

pub fn refresh() {
    let Some((lists, count, hwnd, dpi)) = with(|a| {
        a.rebuild_rows();
        (a.w.lists, a.rows.len(), a.w.main, a.dpi)
    }) else {
        return;
    };
    for list in lists {
        unsafe {
            send(list, LVM_SETITEMCOUNT, count, LVSICF_NOSCROLL as isize);
            fit_columns(list, dpi);
            InvalidateRect(list, std::ptr::null(), 1);
        }
    }
    unsafe {
        InvalidateRect(hwnd, std::ptr::null(), 0);
    }
}

fn icon(row: usize, side: usize) -> i32 {
    with(|a| {
        let Some(pair) = &a.pair else { return 0 };
        let Some(row) = a.rows.get(row) else { return 0 };
        let item = pair.items[row.id as usize];
        let (tree, id) = if side == 0 { (&pair.left, item.left) } else { (&pair.right, item.right) };
        if id == NONE {
            return -1;
        }
        let n = tree.nodes[id as usize];
        let name = String::from_utf16_lossy(tree.name(id));
        let ext =
            if n.dir() { String::new() } else { name.rsplit_once('.').map(|(_, e)| format!(".{e}").to_lowercase()).unwrap_or_default() };
        let key = (ext, n.dir());
        if let Some(&icon) = a.icons.get(&key) {
            return icon;
        }
        let probe = wide(if key.1 {
            "folder"
        } else if key.0.is_empty() {
            "file"
        } else {
            &key.0
        });
        let mut info: SHFILEINFOW = unsafe { std::mem::zeroed() };
        unsafe {
            SHGetFileInfoW(
                probe.as_ptr(),
                if n.dir() { 0x10 } else { 0x80 },
                &mut info,
                size_of::<SHFILEINFOW>() as u32,
                SHGFI_SYSICONINDEX | SHGFI_SMALLICON | SHGFI_USEFILEATTRIBUTES,
            );
        }
        a.icons.insert(key, info.iIcon);
        info.iIcon
    })
    .unwrap_or(0)
}

pub fn selected(list: HWND) -> Option<usize> {
    let row = unsafe { send(list, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize) };
    (row >= 0).then_some(row as usize)
}

pub fn toggle(index: usize) {
    let changed = with(|a| {
        let Some(row) = a.rows.get(index) else { return false };
        let Some(pair) = &a.pair else { return false };
        if pair.items[row.id as usize].first == NONE {
            return false;
        }
        if !a.expanded.remove(&row.id) {
            a.expanded.insert(row.id);
        }
        true
    })
    .unwrap_or(false);
    if changed {
        refresh();
    }
}

fn sync_scroll(source: HWND) {
    let Some(target) = with(|a| {
        if a.syncing {
            return None;
        }
        a.syncing = true;
        Some(if source == a.w.lists[0] { a.w.lists[1] } else { a.w.lists[0] })
    })
    .flatten() else {
        return;
    };
    unsafe {
        let from = send(source, LVM_GETTOPINDEX, 0, 0);
        let to = send(target, LVM_GETTOPINDEX, 0, 0);
        if from != to {
            let mut r = RECT { left: LVIR_BOUNDS as i32, ..Default::default() };
            if send(target, LVM_GETITEMRECT, to as usize, &mut r as *mut _ as isize) != 0 {
                send(target, LVM_SCROLL, 0, (from - to) * (r.bottom - r.top) as isize);
            }
        }
    }
    with(|a| a.syncing = false);
}

pub unsafe fn notify(l: LPARAM) -> LRESULT {
    unsafe {
        let header = &*(l as *const NMHDR);
        let Some(side) = with(|a| a.w.lists.iter().position(|&h| h == header.hwndFrom)).flatten() else { return 0 };
        match header.code {
            LVN_GETDISPINFOW => {
                let d = &mut *(l as *mut NMLVDISPINFOW);
                if d.item.iItem < 0 {
                    return 0;
                }
                let row = d.item.iItem as usize;
                if d.item.mask & LVIF_TEXT != 0 && !d.item.pszText.is_null() && d.item.cchTextMax > 0 {
                    let text = with(|a| a.cell(row, side, d.item.iSubItem)).unwrap_or_default();
                    let text: Vec<u16> = text.encode_utf16().collect();
                    let n = text.len().min((d.item.cchTextMax - 1) as usize);
                    std::ptr::copy_nonoverlapping(text.as_ptr(), d.item.pszText, n);
                    *d.item.pszText.add(n) = 0;
                }
                if d.item.mask & LVIF_IMAGE != 0 {
                    d.item.iImage = icon(row, side);
                }
                if d.item.mask & LVIF_INDENT != 0 {
                    d.item.iIndent = with(|a| a.rows.get(row).map_or(0, |r| r.depth as i32 + 1)).unwrap_or(0);
                }
            }
            NM_CUSTOMDRAW => {
                let d = &mut *(l as *mut NMLVCUSTOMDRAW);
                match d.nmcd.dwDrawStage {
                    CDDS_PREPAINT => return CDRF_NOTIFYITEMDRAW as isize,
                    CDDS_ITEMPREPAINT => {
                        let row = d.nmcd.dwItemSpec;
                        with(|a| {
                            if let (Some(pair), Some(row)) = (&a.pair, a.rows.get(row)) {
                                let item = pair.items[row.id as usize];
                                let missing = if side == 0 { item.left == NONE } else { item.right == NONE };
                                let different = item.state.content_differs() || item.state == State::Type;
                                d.clrText = if missing {
                                    a.pal.dim
                                } else if item.state.uncertain() {
                                    if a.pal.dark { 0x70BFFF } else { 0x006090 }
                                } else if different {
                                    if a.pal.dark { 0x7070EF } else { 0x3030B0 }
                                } else {
                                    a.pal.text
                                };
                                d.clrTextBk = if !missing && different && d.nmcd.uItemState & CDIS_SELECTED == 0 {
                                    if a.pal.dark { 0x222235 } else { 0xEEEEFF }
                                } else {
                                    a.pal.list
                                };
                            }
                        });
                        return (CDRF_DODEFAULT | CDRF_NOTIFYPOSTPAINT) as isize;
                    }
                    CDDS_ITEMPOSTPAINT => {
                        let row = d.nmcd.dwItemSpec;
                        with(|a| {
                            if let (Some(pair), Some(row)) = (&a.pair, a.rows.get(row))
                                && pair.items[row.id as usize].first != NONE
                            {
                                let mut bounds = RECT { left: LVIR_BOUNDS as i32, ..Default::default() };
                                send(header.hwndFrom, LVM_GETITEMRECT, d.nmcd.dwItemSpec, &mut bounds as *mut _ as isize);
                                let x = bounds.left + a.scale(row.depth as i32 * 16 + 4);
                                let y = (bounds.top + bounds.bottom) / 2;
                                let saved = SaveDC(d.nmcd.hdc);
                                let width = send(header.hwndFrom, LVM_GETCOLUMNWIDTH, 0, 0) as i32;
                                IntersectClipRect(d.nmcd.hdc, bounds.left.max(0), bounds.top, bounds.left + width, bounds.bottom);
                                let color = a.pal.text;
                                crate::util::fill(d.nmcd.hdc, &crate::util::rect(x, y, x + a.scale(9), y + 1), color);
                                if !a.expanded.contains(&row.id) && a.filter == 0 && a.query.is_empty() {
                                    crate::util::fill(
                                        d.nmcd.hdc,
                                        &crate::util::rect(x + a.scale(4), y - a.scale(4), x + a.scale(4) + 1, y + a.scale(5)),
                                        color,
                                    );
                                }
                                RestoreDC(d.nmcd.hdc, saved);
                            }
                        });
                    }
                    _ => {}
                }
            }
            NM_CLICK => {
                let d = &*(l as *const NMITEMACTIVATE);
                if d.iItem >= 0 {
                    let glyph = with(|a| a.rows.get(d.iItem as usize).is_some_and(|r| d.ptAction.x < a.scale(r.depth as i32 * 16 + 20)))
                        .unwrap_or(false);
                    if glyph {
                        toggle(d.iItem as usize);
                    }
                }
            }
            NM_DBLCLK => {
                let d = &*(l as *const NMITEMACTIVATE);
                if d.iItem >= 0 {
                    crate::cmds::activate(d.iItem as usize);
                }
            }
            LVN_ITEMCHANGED => {
                let d = &*(l as *const NMLISTVIEW);
                if d.iItem >= 0 && d.uChanged & LVIF_STATE != 0 && d.uNewState & LVIS_SELECTED != 0 {
                    let target = with(|a| {
                        if a.syncing {
                            None
                        } else {
                            a.syncing = true;
                            Some(a.w.lists[1 - side])
                        }
                    })
                    .flatten();
                    if let Some(target) = target {
                        let clear = LVITEMW { stateMask: LVIS_SELECTED, ..std::mem::zeroed() };
                        send(target, LVM_SETITEMSTATE, usize::MAX, &clear as *const _ as isize);
                        let item = LVITEMW { stateMask: LVIS_SELECTED, state: LVIS_SELECTED, ..std::mem::zeroed() };
                        send(target, LVM_SETITEMSTATE, d.iItem as usize, &item as *const _ as isize);
                        with(|a| a.syncing = false);
                    }
                }
            }
            LVN_KEYDOWN => {
                let d = &*(l as *const NMLVKEYDOWN);
                if matches!(d.wVKey, 13 | 32)
                    && let Some(row) = selected(header.hwndFrom)
                {
                    if d.wVKey == 13 {
                        crate::cmds::activate(row);
                    } else {
                        toggle(row);
                    }
                }
            }
            LVN_COLUMNCLICK => {
                let d = &*(l as *const NMLISTVIEW);
                with(|a| {
                    let descending = a.sort.0 == side && a.sort.1 == d.iSubItem && !a.sort.2;
                    a.sort = (side, d.iSubItem, descending);
                });
                refresh();
            }
            LVN_GETINFOTIPW => {
                let d = &mut *(l as *mut NMLVGETINFOTIPW);
                if d.iItem >= 0 && !d.pszText.is_null() && d.cchTextMax > 0 {
                    let tip = with(|a| {
                        let pair = a.pair.as_ref()?;
                        let row = a.rows.get(d.iItem as usize)?;
                        let item = pair.items[row.id as usize];
                        let (tree, id) = if side == 0 { (&pair.left, item.left) } else { (&pair.right, item.right) };
                        let mut text = crate::lang::state_detail(item.state).to_owned();
                        if id != NONE {
                            text.push_str(&format!("\n{}", tree.path(id).display()));
                        }
                        if item.error != 0 {
                            text.push_str(&format!("\n{}", std::io::Error::from_raw_os_error(item.error as i32)));
                        }
                        Some(text)
                    })
                    .flatten()
                    .unwrap_or_default();
                    let text: Vec<u16> = tip.encode_utf16().collect();
                    let n = text.len().min((d.cchTextMax - 1) as usize);
                    std::ptr::copy_nonoverlapping(text.as_ptr(), d.pszText, n);
                    *d.pszText.add(n) = 0;
                }
            }
            _ => {}
        }
        0
    }
}
