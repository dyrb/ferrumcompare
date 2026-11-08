//! Native OLE file drops. Only path lists cross the callback boundary; opening
//! sources is deferred until OLE has completed the operation. No MOVE effects.
use crate::lang::{S, t};
use std::{
    cell::RefCell,
    ffi::{OsString, c_void},
    io,
    os::windows::ffi::OsStringExt,
    path::PathBuf,
    ptr::null_mut,
    sync::atomic::{
        AtomicU32,
        Ordering::{Acquire, Relaxed, Release},
    },
};
use windows_sys::{
    Win32::{
        Foundation::*,
        Globalization::{CP_ACP, MultiByteToWideChar},
        Graphics::Gdi::ScreenToClient,
        System::{
            Com::*,
            Memory::{GlobalLock, GlobalSize, GlobalUnlock},
            Ole::*,
        },
        UI::WindowsAndMessaging::*,
    },
    core::{GUID, HRESULT, IID_IUnknown, IUnknown_Vtbl},
};

pub const WM_FILES: u32 = WM_APP + 73;
const IID_TARGET: GUID = GUID::from_u128(0x00000122_0000_0000_c000_000000000046);
const CLSID_HELPER: GUID = GUID::from_u128(0x4657278a_411b_11d2_839a_00c04fd918d0);
const IID_HELPER: GUID = GUID::from_u128(0x4657278b_411b_11d2_839a_00c04fd918d0);

#[derive(Clone, Copy)]
pub enum Surface {
    Main,
    Text,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Zone {
    Left,
    Right,
    Both,
}
impl Zone {
    pub fn contains(self, side: usize) -> bool {
        self == Self::Both || side == usize::from(self == Self::Right)
    }
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reason {
    Ready,
    Data,
    Count,
    Busy,
    Move,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Feedback {
    pub zone: Zone,
    pub reason: Reason,
}
impl Feedback {
    pub fn accepted(self) -> bool {
        self.reason == Reason::Ready
    }
    pub fn text(self) -> &'static str {
        t(match self.reason {
            Reason::Ready => match self.zone {
                Zone::Left => S::DropLeft,
                Zone::Right => S::DropRight,
                Zone::Both => S::DropPair,
            },
            Reason::Data => S::DropData,
            Reason::Count => S::DropCount,
            Reason::Busy => S::DropBusy,
            Reason::Move => S::DropMove,
        })
    }
}
pub struct Pending {
    pub paths: Vec<PathBuf>,
    pub zone: Zone,
}

#[repr(C)]
struct DataVtbl {
    unknown: IUnknown_Vtbl,
    get_data: unsafe extern "system" fn(*mut c_void, *const FORMATETC, *mut STGMEDIUM) -> HRESULT,
}
#[repr(C)]
struct TargetVtbl {
    unknown: IUnknown_Vtbl,
    enter: unsafe extern "system" fn(*mut c_void, *mut c_void, u32, POINTL, *mut u32) -> HRESULT,
    over: unsafe extern "system" fn(*mut c_void, u32, POINTL, *mut u32) -> HRESULT,
    leave: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    drop: unsafe extern "system" fn(*mut c_void, *mut c_void, u32, POINTL, *mut u32) -> HRESULT,
}
#[repr(C)]
struct HelperVtbl {
    unknown: IUnknown_Vtbl,
    enter: unsafe extern "system" fn(*mut c_void, HWND, *mut c_void, *const POINT, u32) -> HRESULT,
    leave: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    over: unsafe extern "system" fn(*mut c_void, *const POINT, u32) -> HRESULT,
    drop: unsafe extern "system" fn(*mut c_void, *mut c_void, *const POINT, u32) -> HRESULT,
    _show: usize,
}
struct Helper(*mut c_void);
impl Helper {
    fn new() -> Option<Self> {
        let mut p = null_mut();
        let hr = unsafe { CoCreateInstance(&CLSID_HELPER, null_mut(), CLSCTX_INPROC_SERVER, &IID_HELPER, &mut p) };
        (hr >= 0 && !p.is_null()).then_some(Self(p))
    }
    fn vtable(&self) -> &HelperVtbl {
        unsafe { &**(self.0 as *const *const HelperVtbl) }
    }
}
impl Drop for Helper {
    fn drop(&mut self) {
        unsafe {
            (self.vtable().unknown.Release)(self.0);
        }
    }
}

#[repr(C)]
struct Target {
    vtable: *const TargetVtbl,
    refs: AtomicU32,
    hwnd: HWND,
    surface: Surface,
    hover: RefCell<Result<Vec<PathBuf>, Reason>>,
    pending: RefCell<Option<Pending>>,
    helper: Option<Helper>,
}
pub struct Registration {
    target: *mut Target,
}
impl Registration {
    pub fn new(hwnd: HWND, surface: Surface) -> io::Result<Self> {
        let target = Box::into_raw(Box::new(Target {
            vtable: &VTABLE,
            refs: AtomicU32::new(1),
            hwnd,
            surface,
            hover: RefCell::new(Err(Reason::Data)),
            pending: RefCell::new(None),
            helper: Helper::new(),
        }));
        let hr = unsafe { RegisterDragDrop(hwnd, target.cast()) };
        if hr < 0 {
            unsafe {
                release(target.cast());
            }
            return Err(io::Error::other(format!("OLE drop registration failed: 0x{:08X}", hr as u32)));
        }
        unsafe {
            windows_sys::Win32::UI::Shell::DragAcceptFiles(hwnd, 1);
        }
        Ok(Self { target })
    }
    pub fn take(&self) -> Option<Pending> {
        unsafe { (&*self.target).pending.borrow_mut().take() }
    }
    #[cfg(test)]
    pub fn test_ref(&self) -> TestTarget {
        unsafe {
            add_ref(self.target.cast());
        }
        TestTarget(self.target.cast())
    }
}
impl Drop for Registration {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::UI::Shell::DragAcceptFiles((*self.target).hwnd, 0);
            RevokeDragDrop((*self.target).hwnd);
            release(self.target.cast());
        }
    }
}

fn same_guid(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}
unsafe extern "system" fn query(this: *mut c_void, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    unsafe {
        if out.is_null() || iid.is_null() {
            return E_POINTER;
        }
        *out = null_mut();
        if same_guid(&*iid, &IID_IUnknown) || same_guid(&*iid, &IID_TARGET) {
            *out = this;
            add_ref(this);
            S_OK
        } else {
            E_NOINTERFACE
        }
    }
}
unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
    unsafe { (&*(this as *mut Target)).refs.fetch_add(1, Relaxed) + 1 }
}
unsafe extern "system" fn release(this: *mut c_void) -> u32 {
    unsafe {
        let refs = (&*(this as *mut Target)).refs.fetch_sub(1, Release) - 1;
        if refs == 0 {
            std::sync::atomic::fence(Acquire);
            drop(Box::from_raw(this as *mut Target));
        }
        refs
    }
}
// A provider's GetData can pump messages and destroy the receiving window.
// Retain the COM object for the entire callback even when registration is revoked.
struct Call(*mut c_void);
impl Call {
    unsafe fn retain(p: *mut c_void) -> Self {
        unsafe {
            add_ref(p);
        }
        Self(p)
    }
}
impl Drop for Call {
    fn drop(&mut self) {
        unsafe {
            release(self.0);
        }
    }
}

struct Medium(STGMEDIUM);
impl Drop for Medium {
    fn drop(&mut self) {
        unsafe {
            ReleaseStgMedium(&mut self.0);
        }
    }
}
struct Locked(HGLOBAL);
impl Drop for Locked {
    fn drop(&mut self) {
        unsafe {
            GlobalUnlock(self.0);
        }
    }
}

unsafe fn paths(object: *mut c_void) -> Result<Vec<PathBuf>, Reason> {
    unsafe {
        if object.is_null() {
            return Err(Reason::Data);
        }
        let format = FORMATETC { cfFormat: CF_HDROP, ptd: null_mut(), dwAspect: DVASPECT_CONTENT, lindex: -1, tymed: TYMED_HGLOBAL as u32 };
        let mut storage = STGMEDIUM::default();
        let vt = &**(object as *const *const DataVtbl);
        if (vt.get_data)(object, &format, &mut storage) < 0 {
            return Err(Reason::Data);
        }
        let medium = Medium(storage);
        if medium.0.tymed != TYMED_HGLOBAL as u32 {
            return Err(Reason::Data);
        }
        read_global(medium.0.u.hGlobal)
    }
}
unsafe fn read_global(handle: HGLOBAL) -> Result<Vec<PathBuf>, Reason> {
    unsafe {
        let len = GlobalSize(handle);
        if len < 20 || len > isize::MAX as usize {
            return Err(Reason::Data);
        }
        let pointer = GlobalLock(handle);
        if pointer.is_null() {
            return Err(Reason::Data);
        }
        let _locked = Locked(handle);
        parse(std::slice::from_raw_parts(pointer.cast::<u8>(), len))
    }
}

/// WM_DROPFILES compatibility for senders that do not implement OLE.
pub fn legacy(hwnd: HWND, surface: Surface, drop: windows_sys::Win32::UI::Shell::HDROP) -> Result<Pending, Reason> {
    use windows_sys::Win32::UI::Shell::{DragFinish, DragQueryPoint};
    struct Finish(windows_sys::Win32::UI::Shell::HDROP);
    impl Drop for Finish {
        fn drop(&mut self) {
            unsafe {
                DragFinish(self.0);
            }
        }
    }
    let _finish = Finish(drop);
    let paths = unsafe { read_global(drop) }?;
    let mut point = POINT::default();
    unsafe {
        if DragQueryPoint(drop, &mut point) == 0 {
            ScreenToClient(hwnd, &mut point);
        }
    }
    let (busy, divider) = match surface {
        Surface::Main => {
            crate::app::with(|a| (a.busy() || a.closing, (a.layout.split.left + a.layout.split.right) / 2)).unwrap_or((true, 0))
        }
        Surface::Text => crate::viewer::drop_state(hwnd),
    };
    if busy {
        return Err(Reason::Busy);
    }
    let zone = if paths.len() == 2 {
        Zone::Both
    } else if point.x < divider {
        Zone::Left
    } else {
        Zone::Right
    };
    Ok(Pending { paths, zone })
}

/// Validate the DROPFILES envelope before reading any strings. No path-list
/// allocation is proportional to an external count or an unbounded string.
fn parse(bytes: &[u8]) -> Result<Vec<PathBuf>, Reason> {
    if bytes.len() < 20 {
        return Err(Reason::Data);
    }
    let offset = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
    let wide = u32::from_le_bytes(bytes[16..20].try_into().unwrap()) != 0;
    if offset < 20 || offset >= bytes.len() || wide && !offset.is_multiple_of(2) {
        return Err(Reason::Data);
    }
    let width = if wide { 2 } else { 1 };
    let mut p = offset;
    let mut paths = Vec::with_capacity(2);
    loop {
        let start = p;
        while p + width <= bytes.len() && (bytes[p] != 0 || wide && bytes[p + 1] != 0) {
            p += width;
            if (p - start) / width > 32767 {
                return Err(Reason::Data);
            }
        }
        if p + width > bytes.len() {
            return Err(Reason::Data);
        }
        if p == start {
            return if paths.is_empty() { Err(Reason::Data) } else { Ok(paths) };
        }
        if paths.len() == 2 {
            return Err(Reason::Count);
        }
        let units = if wide {
            bytes[start..p].as_chunks::<2>().0.iter().map(|b| u16::from_le_bytes(*b)).collect::<Vec<_>>()
        } else {
            let input = &bytes[start..p];
            let n = unsafe { MultiByteToWideChar(CP_ACP, 0, input.as_ptr(), input.len() as i32, null_mut(), 0) };
            if n == 0 {
                return Err(Reason::Data);
            }
            let mut out = vec![0; n as usize];
            if unsafe { MultiByteToWideChar(CP_ACP, 0, input.as_ptr(), input.len() as i32, out.as_mut_ptr(), n) } != n {
                return Err(Reason::Data);
            }
            out
        };
        let path = PathBuf::from(OsString::from_wide(&units));
        if !path.is_absolute() {
            return Err(Reason::Data);
        }
        paths.push(path);
        p += width;
    }
}

fn feedback(target: &Target, value: Option<Feedback>) {
    match target.surface {
        Surface::Main => crate::cmds::drop_feedback(value),
        Surface::Text => crate::viewer::drop_feedback(target.hwnd, value),
    }
}
fn evaluate(target: &Target, point: POINTL, effects: u32) -> Feedback {
    let mut local = POINT { x: point.x, y: point.y };
    unsafe {
        ScreenToClient(target.hwnd, &mut local);
    }
    let (busy, divider) = match target.surface {
        Surface::Main => {
            crate::app::with(|a| (a.busy() || a.closing, (a.layout.split.left + a.layout.split.right) / 2)).unwrap_or((true, 0))
        }
        Surface::Text => crate::viewer::drop_state(target.hwnd),
    };
    let default = if local.x < divider { Zone::Left } else { Zone::Right };
    let hover = target.hover.borrow();
    let zone = if hover.as_ref().is_ok_and(|p| p.len() == 2) { Zone::Both } else { default };
    let reason = if busy || target.pending.borrow().is_some() {
        Reason::Busy
    } else if effects & DROPEFFECT_COPY == 0 {
        Reason::Move
    } else {
        hover.as_ref().err().copied().unwrap_or(Reason::Ready)
    };
    Feedback { zone, reason }
}
unsafe extern "system" fn enter(this: *mut c_void, data: *mut c_void, _keys: u32, point: POINTL, effect: *mut u32) -> HRESULT {
    unsafe {
        if effect.is_null() {
            return E_POINTER;
        }
        let _call = Call::retain(this);
        let target = &*(this as *mut Target);
        let values = paths(data);
        *target.hover.borrow_mut() = values;
        let hint = evaluate(target, point, *effect);
        *effect = if hint.accepted() { DROPEFFECT_COPY } else { DROPEFFECT_NONE };
        if let Some(h) = &target.helper {
            (h.vtable().enter)(h.0, target.hwnd, data, &POINT { x: point.x, y: point.y }, *effect);
        }
        feedback(target, Some(hint));
        S_OK
    }
}
unsafe extern "system" fn over(this: *mut c_void, _keys: u32, point: POINTL, effect: *mut u32) -> HRESULT {
    unsafe {
        if effect.is_null() {
            return E_POINTER;
        }
        let _call = Call::retain(this);
        let target = &*(this as *mut Target);
        let hint = evaluate(target, point, *effect);
        *effect = if hint.accepted() { DROPEFFECT_COPY } else { DROPEFFECT_NONE };
        if let Some(h) = &target.helper {
            (h.vtable().over)(h.0, &POINT { x: point.x, y: point.y }, *effect);
        }
        feedback(target, Some(hint));
        S_OK
    }
}
unsafe extern "system" fn leave(this: *mut c_void) -> HRESULT {
    unsafe {
        let _call = Call::retain(this);
        let target = &*(this as *mut Target);
        *target.hover.borrow_mut() = Err(Reason::Data);
        if let Some(h) = &target.helper {
            (h.vtable().leave)(h.0);
        }
        feedback(target, None);
        S_OK
    }
}
unsafe extern "system" fn drop_files(this: *mut c_void, data: *mut c_void, _keys: u32, point: POINTL, effect: *mut u32) -> HRESULT {
    unsafe {
        if effect.is_null() {
            return E_POINTER;
        }
        let _call = Call::retain(this);
        let target = &*(this as *mut Target);
        let values = paths(data);
        *target.hover.borrow_mut() = values;
        let hint = evaluate(target, point, *effect);
        *effect = DROPEFFECT_NONE;
        if hint.accepted()
            && IsWindow(target.hwnd) != 0
            && let Ok(paths) = std::mem::replace(&mut *target.hover.borrow_mut(), Err(Reason::Data))
        {
            *target.pending.borrow_mut() = Some(Pending { paths, zone: hint.zone });
            *effect = DROPEFFECT_COPY;
        }
        if let Some(h) = &target.helper {
            (h.vtable().drop)(h.0, data, &POINT { x: point.x, y: point.y }, *effect);
        }
        feedback(target, None);
        *target.hover.borrow_mut() = Err(Reason::Data);
        // Post after all provider/helper calls: those calls may pump messages.
        if *effect == DROPEFFECT_COPY && PostMessageW(target.hwnd, WM_FILES, 0, 0) == 0 {
            target.pending.borrow_mut().take();
            *effect = DROPEFFECT_NONE;
        }
        S_OK
    }
}
static VTABLE: TargetVtbl = TargetVtbl {
    unknown: IUnknown_Vtbl { QueryInterface: query, AddRef: add_ref, Release: release },
    enter,
    over,
    leave,
    drop: drop_files,
};

pub fn archive(path: &std::path::Path) -> bool {
    path.extension().and_then(|s| s.to_str()).is_some_and(|e| {
        ["zip", "7z", "rar", "tar", "gz", "bz2", "xz", "zst", "tgz", "tbz", "tbz2", "txz", "cab", "iso", "jar", "war"]
            .iter()
            .any(|a| e.eq_ignore_ascii_case(a))
    })
}
pub fn assign(mut current: [Option<PathBuf>; 2], pending: Pending) -> [Option<PathBuf>; 2] {
    let mut paths = pending.paths.into_iter();
    if pending.zone == Zone::Both {
        [paths.next(), paths.next()]
    } else {
        current[usize::from(pending.zone == Zone::Right)] = paths.next();
        current
    }
}

#[cfg(test)]
pub struct TestTarget(*mut c_void);
#[cfg(test)]
impl Drop for TestTarget {
    fn drop(&mut self) {
        unsafe {
            release(self.0);
        }
    }
}
#[cfg(test)]
impl TestTarget {
    /// Real Shell IDataObject, invoked without DoDragDrop or mouse input.
    pub fn drag(&self, paths: &[PathBuf], point: POINTL, commit: bool) -> u32 {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc};
        use windows_sys::Win32::UI::Shell::SHCreateDataObject;
        #[repr(C)]
        struct SetDataVtbl {
            base: DataVtbl,
            _other: [usize; 3],
            set_data: unsafe extern "system" fn(*mut c_void, *const FORMATETC, *const STGMEDIUM, i32) -> HRESULT,
        }
        struct Data(*mut c_void);
        impl Drop for Data {
            fn drop(&mut self) {
                unsafe {
                    let vt = &**(self.0 as *const *const DataVtbl);
                    (vt.unknown.Release)(self.0);
                }
            }
        }
        unsafe {
            let mut object = null_mut();
            let iid = GUID::from_u128(0x0000010e_0000_0000_c000_000000000046);
            assert!(SHCreateDataObject(std::ptr::null(), 0, std::ptr::null(), null_mut(), &iid, &mut object) >= 0);
            let object = Data(object);
            let mut bytes = vec![0u8; 20];
            bytes[0] = 20;
            bytes[16] = 1;
            for path in paths {
                bytes.extend(path.as_os_str().encode_wide().chain([0]).flat_map(u16::to_le_bytes));
            }
            bytes.extend([0, 0]);
            let handle = GlobalAlloc(GMEM_MOVEABLE, bytes.len());
            assert!(!handle.is_null());
            let pointer = GlobalLock(handle);
            assert!(!pointer.is_null());
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.cast(), bytes.len());
            GlobalUnlock(handle);
            let medium = Medium(STGMEDIUM { tymed: TYMED_HGLOBAL as u32, u: STGMEDIUM_0 { hGlobal: handle }, pUnkForRelease: null_mut() });
            let format =
                FORMATETC { cfFormat: CF_HDROP, ptd: null_mut(), dwAspect: DVASPECT_CONTENT, lindex: -1, tymed: TYMED_HGLOBAL as u32 };
            let vt = &**(object.0 as *const *const SetDataVtbl);
            assert!((vt.set_data)(object.0, &format, &medium.0, 1) >= 0);
            std::mem::forget(medium);
            let mut effect = DROPEFFECT_COPY;
            assert_eq!(enter(self.0, object.0, 0, point, &mut effect), S_OK);
            effect = DROPEFFECT_COPY;
            assert_eq!(over(self.0, 0, point, &mut effect), S_OK);
            if commit && effect != DROPEFFECT_NONE {
                assert_eq!(drop_files(self.0, object.0, 0, point, &mut effect), S_OK);
            } else {
                assert_eq!(leave(self.0), S_OK);
                effect = DROPEFFECT_NONE;
            }
            effect
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn encoded(paths: &[&str]) -> Vec<u8> {
        let mut data = vec![0; 20];
        data[0] = 20;
        data[16] = 1;
        for path in paths {
            data.extend(path.encode_utf16().chain([0]).flat_map(u16::to_le_bytes));
        }
        data.extend([0, 0]);
        data
    }
    #[test]
    fn bounded_native_paths_and_atomic_side_assignment() {
        let data = encoded(&[r"C:\Имя\left.rs", r"\\server\share\right.rs"]);
        let paths = parse(&data).unwrap();
        assert_eq!(paths.len(), 2);
        let current = [Some(PathBuf::from(r"C:\old-left")), Some(PathBuf::from(r"C:\old-right"))];
        assert_eq!(
            assign(current.clone(), Pending { paths: vec![paths[0].clone()], zone: Zone::Right }),
            [current[0].clone(), Some(paths[0].clone())]
        );
        assert_eq!(assign(current, Pending { paths: paths.clone(), zone: Zone::Both }), [Some(paths[0].clone()), Some(paths[1].clone())]);
        assert_eq!(parse(&encoded(&[r"C:\a", r"C:\b", r"C:\c"])).unwrap_err(), Reason::Count);
        assert_eq!(parse(&encoded(&["relative.txt"])).unwrap_err(), Reason::Data);
        for n in 0..data.len() {
            assert!(parse(&data[..n]).is_err());
        }
        let mut bad = data.clone();
        bad[0] = 19;
        assert_eq!(parse(&bad).unwrap_err(), Reason::Data);
        let long = format!("C:\\{}", "x".repeat(32768));
        assert_eq!(parse(&encoded(&[&long])).unwrap_err(), Reason::Data);
        let mut ansi = vec![0; 20];
        ansi[0] = 20;
        ansi.extend_from_slice(b"C:\\one.txt\0C:\\two.txt\0\0");
        assert_eq!(parse(&ansi).unwrap(), vec![PathBuf::from(r"C:\one.txt"), PathBuf::from(r"C:\two.txt")]);
    }
}
