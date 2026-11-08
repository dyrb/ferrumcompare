use crate::{
    app::{Event, WM_WORK, with},
    lang::{self, S, t},
    util::*,
};
use fc_core::{Backend, Case, NONE, Options, Progress};
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::sync::{Arc, atomic::Ordering::Relaxed};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::{ClientToScreen, InvalidateRect};
use windows_sys::Win32::UI::Controls::Dialogs::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const EXPORT: u32 = 300;
const EXIT: u32 = 301;
const EXPAND: u32 = 302;
const COLLAPSE: u32 = 303;
const ABOUT: u32 = 304;
const OPEN_FILES: u32 = 305;
const FOLDER_FILTERS: u32 = 306;
const THEME: u32 = 310;
const LANGUAGE: u32 = 320;
const CASE: u32 = 330;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    File,
    Folder,
}
fn source_kind(path: &std::path::Path) -> std::io::Result<SourceKind> {
    let meta = std::fs::metadata(path)?;
    if meta.is_dir() {
        return Ok(SourceKind::Folder);
    }
    if meta.is_file() {
        if crate::drop_target::archive(path) {
            return Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "archive contents are not supported"));
        }
        return Ok(SourceKind::File);
    }
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "only files and folders can be compared"))
}
pub fn validate_drop(pending: &crate::drop_target::Pending) -> std::io::Result<SourceKind> {
    if pending.paths.is_empty() || pending.paths.len() > 2 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "drop count exceeded"));
    }
    let first = source_kind(&pending.paths[0])?;
    if let Some(second) = pending.paths.get(1)
        && source_kind(second)? != first
    {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "source kinds differ"));
    }
    Ok(first)
}
pub fn drop_feedback(hint: Option<crate::drop_target::Feedback>) {
    if let Some(h) = with(|a| {
        if a.drop_hint == hint {
            None
        } else {
            a.drop_hint = hint;
            Some(a.w.main)
        }
    })
    .flatten()
    {
        unsafe {
            InvalidateRect(h, std::ptr::null(), 0);
        }
    }
}
pub fn accept_drop(pending: crate::drop_target::Pending) {
    if let Some((h, true)) = with(|a| (a.w.main, a.busy() || a.closing)) {
        message(h, t(S::DropBusy), false);
        return;
    }
    match validate_drop(&pending) {
        Ok(kind) => accept_checked(pending, kind),
        Err(e) => {
            if let Some(h) = with(|a| a.w.main) {
                message(h, &lang::error(&e), false);
            }
        }
    }
}
pub fn accept_checked(pending: crate::drop_target::Pending, kind: SourceKind) {
    let Some((w, busy)) = with(|a| (a.w, a.busy() || a.closing)) else { return };
    if busy {
        message(w.main, t(S::DropBusy), false);
        return;
    }
    let original = [edit_path(w.left), edit_path(w.right)].map(|p| if p.as_os_str().is_empty() { None } else { Some(p) });
    let pair = pending.paths.len() == 2;
    let side = usize::from(pending.zone == crate::drop_target::Zone::Right);
    let ready = pair || original[1 - side].as_deref().is_some_and(|p| source_kind(p).ok() == Some(kind));
    let paths = crate::drop_target::assign(original.clone(), pending);
    invalidate_results();
    for (side, path) in paths.iter().enumerate() {
        if *path != original[side] {
            set_path(if side == 0 { w.left } else { w.right }, path.as_deref().unwrap_or(std::path::Path::new("")));
        }
    }
    if ready {
        start_with(Some(kind))
    } else {
        with(|a| a.status = t(S::DropOther).into());
        unsafe {
            InvalidateRect(w.main, std::ptr::null(), 0);
        }
    }
}

pub fn invalidate_results() {
    let changed = with(|a| {
        if a.busy() || a.pair.is_none() {
            return false;
        }
        a.pair = None;
        a.summary = Default::default();
        a.status.clear();
        a.expanded.clear();
        true
    })
    .unwrap_or(false);
    if changed {
        crate::list::refresh();
    }
}

fn spawn(f: impl FnOnce() -> Event + Send + 'static, exporting: bool) {
    let Some((hwnd, tx)) = with(|a| (a.w.main as usize, a.tx.clone())) else { return };
    let backup = tx.clone();
    let result = std::thread::Builder::new().name("comparison".into()).spawn(move || {
        let event = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|_| {
            let e = std::io::Error::other("comparison worker failed");
            if exporting { Event::Exported(Err(e)) } else { Event::Compared(Err(e)) }
        });
        if tx.send(event).is_ok() {
            unsafe {
                PostMessageW(hwnd as HWND, WM_WORK, 0, 0);
            }
        }
    });
    if let Err(e) = result {
        let _ = backup.send(if exporting { Event::Exported(Err(e)) } else { Event::Compared(Err(e)) });
        unsafe {
            PostMessageW(hwnd as HWND, WM_WORK, 0, 0);
        }
    }
}

pub fn start() {
    start_with(None);
}
fn start_with(known: Option<SourceKind>) {
    let Some((w, busy, settings)) = with(|a| (a.w, a.busy(), a.settings.clone())) else { return };
    if busy {
        return;
    }
    let left = edit_path(w.left);
    let right = edit_path(w.right);
    if left.as_os_str().is_empty() || right.as_os_str().is_empty() {
        message(w.main, t(S::Missing), false);
        return;
    }
    let kind = match known {
        Some(kind) => kind,
        None => match source_kind(&left).and_then(|a| source_kind(&right).map(|b| (a, b))) {
            Ok((a, b)) if a == b => a,
            Ok(_) => {
                message(w.main, t(S::DropKinds), false);
                return;
            }
            Err(e) => {
                message(w.main, &lang::error(&e), true);
                return;
            }
        },
    };
    if kind == SourceKind::File {
        crate::viewer::open(w.main, [Some(left), Some(right)]);
        return;
    }
    let scope = match fc_core::filter::Scope::new(&settings.include_masks, &settings.exclude_masks) {
        Ok(scope) => scope,
        Err(e) => {
            message(w.main, &lang::error(&e), true);
            return;
        }
    };
    let options = Options {
        scope,
        contents: settings.contents,
        backend: match settings.backend {
            1 => Backend::Walk,
            2 => Backend::Mft,
            _ => Backend::Auto,
        },
        case: match settings.case {
            1 => Case::Sensitive,
            2 => Case::Insensitive,
            _ => Case::Auto,
        },
        ..Default::default()
    };
    let progress = Arc::new(Progress::default());
    with(|a| {
        a.progress = Some(progress.clone());
        a.status.clear();
    });
    spawn(move || Event::Compared(fc_core::compare(&left, &right, &options, &progress).map(Box::new)), false);
    unsafe {
        SetTimer(w.main, 1, 125, None);
        InvalidateRect(w.main, std::ptr::null(), 0);
    }
    crate::ui::enabled();
}

pub fn stop() {
    with(|a| {
        if let Some(p) = &a.progress {
            p.cancel.store(true, Relaxed);
        }
    });
    crate::ui::enabled();
}

pub fn receive() {
    let events = with(|a| a.rx.try_iter().collect::<Vec<_>>()).unwrap_or_default();
    for event in events {
        let Some((w, closing)) = with(|a| {
            a.progress = None;
            a.exporting = false;
            (a.w, a.closing)
        }) else {
            return;
        };
        unsafe {
            KillTimer(w.main, 1);
        }
        if closing {
            unsafe {
                DestroyWindow(w.main);
            }
            return;
        }
        let error = match event {
            Event::Compared(Ok(pair)) => {
                with(|a| {
                    a.summary = pair.summary();
                    let backend = |b| t(if b == Backend::Mft { S::Mft } else { S::Walk });
                    a.status = format!(
                        "{} {:.2} {} ({}, {})",
                        t(S::Completed),
                        pair.elapsed.as_secs_f64(),
                        if lang::ru() { "сек" } else { "sec" },
                        backend(pair.left.backend),
                        backend(pair.right.backend)
                    );
                    a.pair = Some(Arc::from(pair));
                    a.expanded.clear();
                });
                crate::list::refresh();
                None
            }
            Event::Compared(Err(e)) if e.kind() == std::io::ErrorKind::Interrupted => {
                with(|a| a.status = t(S::Cancelled).into());
                None
            }
            Event::Compared(Err(e)) | Event::Exported(Err(e)) => Some(e),
            Event::Exported(Ok(())) => {
                with(|a| a.status = t(S::Saved).into());
                None
            }
        };
        crate::ui::enabled();
        unsafe {
            InvalidateRect(w.main, std::ptr::null(), 0);
        }
        if let Some(e) = error {
            with(|a| a.status = lang::error(&e));
            message(w.main, &lang::error(&e), true);
        }
    }
}

pub fn browse(side: usize) {
    let Some((w, busy)) = with(|a| (a.w, a.busy())) else { return };
    if busy {
        return;
    }
    let field = if side == 0 { w.left } else { w.right };
    match crate::shell::choose_folder(w.main, &edit_path(field), t(S::Choose)) {
        Ok(Some(path)) => set_path(field, &path),
        Ok(None) => {}
        Err(e) => message(w.main, &lang::error(&e), true),
    }
}

pub fn activate(index: usize) {
    let selected = with(|a| {
        let pair = a.pair.as_ref()?;
        let row = a.rows.get(index)?;
        let item = pair.items[row.id as usize];
        if pair.is_dir(row.id) {
            return None;
        }
        Some((
            a.w.main,
            [
                if item.left != NONE { Some(pair.left.path(item.left)) } else { None },
                if item.right != NONE { Some(pair.right.path(item.right)) } else { None },
            ],
        ))
    })
    .flatten();
    if let Some((h, paths)) = selected { crate::viewer::open(h, paths) } else { crate::list::toggle(index) }
}

fn open_files() {
    let Some(owner) = with(|a| a.w.main) else { return };
    let mut paths = [None, None];
    for (side, path) in paths.iter_mut().enumerate() {
        let mut buffer = vec![0u16; 32768];
        let filter = wide(&format!("{}\0*.*\0", t(S::AllFiles)));
        let title = wide(&format!("{} {}", t(S::ChooseFile), t(if side == 0 { S::Left } else { S::Right })));
        let mut dialog: OPENFILENAMEW = unsafe { std::mem::zeroed() };
        dialog.lStructSize = size_of::<OPENFILENAMEW>() as u32;
        dialog.hwndOwner = owner;
        dialog.lpstrFile = buffer.as_mut_ptr();
        dialog.nMaxFile = buffer.len() as u32;
        dialog.lpstrFilter = filter.as_ptr();
        dialog.lpstrTitle = title.as_ptr();
        dialog.Flags = OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR;
        if unsafe { GetOpenFileNameW(&mut dialog) } == 0 {
            return;
        }
        let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        *path = Some(PathBuf::from(std::ffi::OsString::from_wide(&buffer[..len])));
    }
    crate::viewer::open(owner, paths);
}

fn export() {
    let Some((w, pair, busy)) = with(|a| (a.w, a.pair.clone(), a.busy())) else { return };
    let Some(pair) = pair.filter(|_| !busy) else { return };
    let mut buffer = vec![0u16; 32768];
    let name = wide("comparison.csv");
    buffer[..name.len()].copy_from_slice(&name);
    let filter = wide("CSV (*.csv)\0*.csv\0JSON (*.json)\0*.json\0");
    let title = wide(t(S::Save));
    let extension = wide("csv");
    let mut dialog: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    dialog.lStructSize = size_of::<OPENFILENAMEW>() as u32;
    dialog.hwndOwner = w.main;
    dialog.lpstrFilter = filter.as_ptr();
    dialog.nFilterIndex = 1;
    dialog.lpstrFile = buffer.as_mut_ptr();
    dialog.nMaxFile = buffer.len() as u32;
    dialog.lpstrTitle = title.as_ptr();
    dialog.lpstrDefExt = extension.as_ptr();
    dialog.Flags = OFN_EXPLORER | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR;
    if unsafe { GetSaveFileNameW(&mut dialog) } == 0 {
        return;
    }
    let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    let path = PathBuf::from(std::ffi::OsString::from_wide(&buffer[..len]));
    let json = dialog.nFilterIndex == 2 || path.extension().is_some_and(|e| e.eq_ignore_ascii_case("json"));
    with(|a| a.exporting = true);
    crate::ui::enabled();
    unsafe {
        InvalidateRect(w.main, std::ptr::null(), 0);
    }
    spawn(move || Event::Exported(fc_core::export::save(&pair, &path, json)), true);
}

fn item(menu: HMENU, id: u32, label: &str, checked: bool, disabled: bool) {
    unsafe {
        AppendMenuW(
            menu,
            MF_STRING | if checked { MF_CHECKED } else { 0 } | if disabled { MF_GRAYED } else { 0 },
            id as usize,
            wide(label).as_ptr(),
        );
    }
}
fn submenu(menu: HMENU, label: S, choices: &[(u32, &str, bool)]) {
    unsafe {
        let child = CreatePopupMenu();
        for &(id, label, checked) in choices {
            item(child, id, label, checked, false);
        }
        AppendMenuW(menu, MF_POPUP, child as usize, wide(t(label)).as_ptr());
    }
}

pub fn menu(index: usize) {
    let Some((w, layout, settings, busy, has_data)) = with(|a| (a.w, a.layout.clone(), a.settings.clone(), a.busy(), a.pair.is_some()))
    else {
        return;
    };
    unsafe {
        let menu = CreatePopupMenu();
        match index {
            0 => {
                item(menu, crate::ID_COMPARE as u32, t(S::Compare), false, busy);
                item(menu, OPEN_FILES, t(S::OpenFiles), false, false);
                item(menu, EXPORT, t(S::Export), false, busy || !has_data);
                item(menu, EXIT, t(S::Exit), false, false);
            }
            1 => {
                submenu(
                    menu,
                    S::Theme,
                    &[
                        (THEME, t(S::System), settings.theme == 0),
                        (THEME + 1, t(S::Light), settings.theme == 1),
                        (THEME + 2, t(S::Dark), settings.theme == 2),
                    ],
                );
                submenu(
                    menu,
                    S::Language,
                    &[
                        (LANGUAGE, t(S::System), settings.language == 0),
                        (LANGUAGE + 1, "English", settings.language == 1),
                        (LANGUAGE + 2, "Русский", settings.language == 2),
                    ],
                );
                submenu(
                    menu,
                    S::Case,
                    &[
                        (CASE, t(S::Auto), settings.case == 0),
                        (CASE + 1, t(S::Sensitive), settings.case == 1),
                        (CASE + 2, t(S::Insensitive), settings.case == 2),
                    ],
                );
                item(menu, EXPAND, t(S::Expand), false, !has_data);
                item(menu, COLLAPSE, t(S::Collapse), false, !has_data);
                item(menu, FOLDER_FILTERS, t(S::FolderFilters), false, busy);
            }
            _ => item(menu, ABOUT, t(S::About), false, false),
        }
        let mut point = POINT { x: layout.menus[index].left, y: layout.menus[index].bottom };
        ClientToScreen(w.main, &mut point);
        let id = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_LEFTALIGN, point.x, point.y, 0, w.main, std::ptr::null());
        DestroyMenu(menu);
        if id > 0 {
            command(id as u32);
        }
    }
}

pub fn command(id: u32) {
    match id {
        n if n == crate::ID_COMPARE as u32 => start(),
        EXPORT => export(),
        OPEN_FILES => open_files(),
        FOLDER_FILTERS => {
            let Some((h, settings, false)) = with(|a| (a.w.main, a.settings.clone(), a.busy())) else { return };
            match crate::prompt::fields(
                h,
                t(S::FolderFilters),
                &[t(S::IncludeMasks), t(S::ExcludeMasks)],
                &[settings.include_masks, settings.exclude_masks],
                t(S::MasksHelp),
            ) {
                Ok(Some(values)) => match fc_core::filter::Scope::new(&values[0], &values[1]) {
                    Ok(_) => {
                        with(|a| {
                            a.settings.include_masks = values[0].clone();
                            a.settings.exclude_masks = values[1].clone();
                        });
                        invalidate_results();
                    }
                    Err(e) => message(h, &lang::error(&e), true),
                },
                Ok(None) => {}
                Err(e) => message(h, &lang::error(&e), true),
            }
        }
        EXIT => {
            if let Some(h) = with(|a| a.w.main) {
                unsafe {
                    PostMessageW(h, WM_CLOSE, 0, 0);
                }
            }
        }
        ABOUT => {
            if let Some(h) = with(|a| a.w.main) {
                message(h, t(S::AboutText), false);
            }
        }
        EXPAND => {
            with(|a| {
                if let Some(pair) = &a.pair {
                    a.expanded = pair.items.iter().enumerate().filter_map(|(i, row)| (row.first != NONE).then_some(i as u32)).collect();
                }
            });
            crate::list::refresh();
        }
        COLLAPSE => {
            with(|a| a.expanded.clear());
            crate::list::refresh();
        }
        THEME..=312 => {
            with(|a| a.settings.theme = (id - THEME) as u8);
            crate::ui::apply_theme();
        }
        LANGUAGE..=322 => {
            with(|a| {
                a.settings.language = (id - LANGUAGE) as u8;
                lang::set(a.settings.language);
            });
            crate::ui::labels();
            crate::list::refresh();
        }
        CASE..=332 if with(|a| !a.busy()).unwrap_or(false) => {
            with(|a| a.settings.case = (id - CASE) as u8);
            invalidate_results();
        }
        _ => {}
    }
}

pub fn context(source: HWND, mut point: POINT) {
    let Some((w, pair)) = with(|a| (a.w, a.pair.clone())) else { return };
    let Some(pair) = pair else { return };
    let side = if source == w.lists[1] { 1 } else { 0 };
    let Some(selected) = crate::list::selected(w.lists[side]) else { return };
    let Some(id) = with(|a| a.rows.get(selected).map(|r| r.id)).flatten() else { return };
    let row = pair.items[id as usize];
    let (tree, id) = if side == 0 { (&pair.left, row.left) } else { (&pair.right, row.right) };
    if id == NONE {
        return;
    }
    if point.x == -1 && point.y == -1 {
        point = POINT { x: 30, y: 60 };
        unsafe {
            ClientToScreen(source, &mut point);
        }
    }
    let path = tree.path(id);
    if let Some(path) = path.to_str() {
        crate::shell::context_menu(w.main, path, point, |_| false);
    }
}
