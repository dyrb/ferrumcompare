//! Small native modal forms, expressed in dialog units and using the system dialog manager.
use crate::{
    lang::{S, t},
    theme,
    util::*,
};
use std::{cell::RefCell, io, ptr::null};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Controls::{DRAWITEMSTRUCT, EM_SETLIMITTEXT, ODS_FOCUS, ODS_SELECTED},
        WindowsAndMessaging::*,
    },
};

struct Form {
    values: Vec<String>,
    pal: theme::Palette,
}

fn dword(words: &mut Vec<u16>, n: u32) {
    words.extend([n as u16, (n >> 16) as u16]);
}
fn item(words: &mut Vec<u16>, style: u32, bounds: [u16; 4], id: u16, class: u16, label: &str) {
    if !words.len().is_multiple_of(2) {
        words.push(0);
    }
    dword(words, WS_CHILD | WS_VISIBLE | style);
    dword(words, 0);
    words.extend(bounds);
    words.extend([id, 0xffff, class]);
    words.extend(wide(label));
    words.push(0);
}

unsafe extern "system" fn procedure(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> isize {
    unsafe {
        if msg == WM_INITDIALOG {
            SetWindowLongPtrW(h, GWLP_USERDATA, l);
            let cell = &*(l as *const RefCell<Form>);
            let (values, pal) = {
                let form = cell.borrow();
                (form.values.clone(), form.pal)
            };
            theme::set_title_dark(h, pal.dark);
            for (i, value) in values.iter().enumerate() {
                let edit = GetDlgItem(h, 1000 + i as i32);
                SetWindowTextW(edit, wide(value).as_ptr());
                send(edit, EM_SETLIMITTEXT, 4096, 0);
                theme::style_control(edit, pal.dark, theme::Kind::Cfd);
            }
            return 1;
        }
        let pointer = GetWindowLongPtrW(h, GWLP_USERDATA) as *const RefCell<Form>;
        if pointer.is_null() {
            return 0;
        }
        let cell = &*pointer;
        let pal = cell.borrow().pal;
        match msg {
            WM_COMMAND if w & 0xffff == IDOK as usize => {
                let count = cell.borrow().values.len();
                let mut values = Vec::with_capacity(count);
                for i in 0..count {
                    let edit = GetDlgItem(h, 1000 + i as i32);
                    let n = GetWindowTextLengthW(edit).clamp(0, 4096) as usize;
                    let mut text = vec![0u16; n + 1];
                    let read = GetWindowTextW(edit, text.as_mut_ptr(), text.len() as i32).max(0) as usize;
                    values.push(String::from_utf16_lossy(&text[..read]));
                }
                cell.borrow_mut().values = values;
                EndDialog(h, 1);
                return 1;
            }
            WM_CLOSE => {
                EndDialog(h, 0);
                return 1;
            }
            WM_COMMAND if w & 0xffff == IDCANCEL as usize => {
                EndDialog(h, 0);
                return 1;
            }
            WM_CTLCOLORDLG | WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT => {
                let dc = w as HDC;
                let bg = if msg == WM_CTLCOLOREDIT { pal.input } else { pal.bg };
                SetTextColor(dc, pal.text);
                SetBkColor(dc, bg);
                SetDCBrushColor(dc, bg);
                return GetStockObject(DC_BRUSH) as isize;
            }
            WM_DRAWITEM => {
                let d = &*(l as *const DRAWITEMSTRUCT);
                let saved = SaveDC(d.hDC);
                let font = send(d.hwndItem, WM_GETFONT, 0, 0) as HFONT;
                if !font.is_null() {
                    SelectObject(d.hDC, font);
                }
                fill(d.hDC, &d.rcItem, if d.itemState & ODS_SELECTED != 0 { pal.sel } else { pal.panel });
                frame(d.hDC, &d.rcItem, if d.itemState & ODS_FOCUS != 0 { pal.accent } else { pal.border }, 1);
                SetBkMode(d.hDC, TRANSPARENT as i32);
                text(d.hDC, t(if d.CtlID == IDOK as u32 { S::Apply } else { S::Cancel }), &d.rcItem, pal.text, DT_CENTER);
                RestoreDC(d.hDC, saved);
                return 1;
            }
            _ => {}
        }
        0
    }
}

fn template(title: &str, labels: &[&str], help: &str) -> Vec<u32> {
    let mut words = Vec::new();
    dword(&mut words, WS_POPUP | WS_CAPTION | WS_SYSMENU | DS_MODALFRAME as u32 | DS_SETFONT as u32 | DS_CENTER as u32);
    dword(&mut words, 0);
    let bottom = 10 + labels.len() as u16 * 32;
    words.extend([labels.len() as u16 * 2 + 3, 0, 0, 320, bottom + 58, 0, 0]);
    words.extend(wide(title));
    words.push(9);
    words.extend(wide("Segoe UI"));
    for (i, label) in labels.iter().enumerate() {
        let y = 8 + i as u16 * 32;
        item(&mut words, 0, [10, y, 300, 10], 2000 + i as u16, 0x82, label);
        item(&mut words, WS_TABSTOP | WS_BORDER | ES_AUTOHSCROLL as u32, [10, y + 12, 300, 14], 1000 + i as u16, 0x81, "");
    }
    item(&mut words, 0, [10, bottom, 300, 24], 2003, 0x82, help);
    item(&mut words, WS_TABSTOP | BS_OWNERDRAW as u32, [180, bottom + 32, 62, 14], IDOK as u16, 0x80, t(S::Apply));
    item(&mut words, WS_TABSTOP | BS_OWNERDRAW as u32, [248, bottom + 32, 62, 14], IDCANCEL as u16, 0x80, t(S::Cancel));
    // A Vec<u32> guarantees the DWORD alignment required by DLGTEMPLATE.
    words.chunks(2).map(|w| w[0] as u32 | (w.get(1).copied().unwrap_or(0) as u32) << 16).collect()
}

pub fn fields(owner: HWND, title: &str, labels: &[&str], initial: &[String], help: &str) -> io::Result<Option<Vec<String>>> {
    if labels.len() != initial.len() || labels.is_empty() || labels.len() > 3 {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    let template = template(title, labels, help);
    let form = RefCell::new(Form { values: initial.into(), pal: crate::app::with(|a| a.pal).unwrap_or(theme::LIGHT) });
    let result = unsafe {
        DialogBoxIndirectParamW(
            GetModuleHandleW(null()),
            template.as_ptr().cast(),
            owner,
            Some(procedure),
            (&form as *const RefCell<Form>) as isize,
        )
    };
    match result {
        -1 => Err(io::Error::last_os_error()),
        1 => Ok(Some(form.into_inner().values)),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_template_creates_hidden_controls_and_collects_values() {
        let template = template("Filters", &["Include", "Exclude"], "Help");
        let form = RefCell::new(Form { values: vec!["*.cpp;*.hpp".into(), ".git;build".into()], pal: theme::DARK });
        unsafe {
            let h = CreateDialogIndirectParamW(
                GetModuleHandleW(null()),
                template.as_ptr().cast(),
                std::ptr::null_mut(),
                Some(procedure),
                (&form as *const RefCell<Form>) as isize,
            );
            assert!(!h.is_null());
            assert_eq!(IsWindowVisible(h), 0);
            assert!(!GetDlgItem(h, IDOK).is_null());
            SetWindowTextW(GetDlgItem(h, 1000), wide("*.rs").as_ptr());
            send(h, WM_COMMAND, IDOK as usize, 0);
            assert_eq!(form.borrow().values, vec!["*.rs", ".git;build"]);
            DestroyWindow(h);
        }
    }
}
