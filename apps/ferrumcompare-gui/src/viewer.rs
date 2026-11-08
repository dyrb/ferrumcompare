//! Modeless, virtual text comparison. One worker owns all disk access; a single
//! replaceable request slot prevents queued renders from accumulating on resize.
use crate::{
    lang::{self, S, t},
    theme,
    util::*,
};
use fc_core::{
    Progress,
    text::{
        EncodingChoice, InlineDiff, RowMap, TextDiff, TextOptions, TextRules,
        syntax::{self, Kind},
    },
};
use std::{
    cell::RefCell,
    collections::VecDeque,
    io,
    os::windows::io::AsRawHandle,
    path::PathBuf,
    ptr::{null, null_mut},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
    },
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Controls::*,
        HiDpi::*,
        Input::KeyboardAndMouse::{EnableWindow, GetFocus, GetKeyState, ReleaseCapture, SetCapture, SetFocus},
        WindowsAndMessaging::*,
    },
};

const CLASS: &str = "FerrumCompareText";
const WORK: u32 = WM_APP + 71;
const PREVIOUS: usize = 501;
const NEXT: usize = 502;
const SYNTAX: usize = 503;
const STOP: usize = 504;
const SEARCH: usize = 505;
const FIND: usize = 506;
const COMPARE_OPTIONS: usize = 507;
const IGNORE_CRLF: usize = 508;
const IGNORE_SPACES: usize = 509;
const IGNORE_TABS: usize = 510;
const IGNORE_BLANK_LINES: usize = 511;
const IGNORE_CASE: usize = 512;
const IGNORE_ENDINGS: usize = 513;
const EXACT_RULES: usize = 514;
const FOLD: usize = 515;
const GO_LINE: usize = 516;
const COPY: usize = 517;
const ENCODING_LEFT: usize = 530;
const ENCODING_RIGHT: usize = 540;
const SCROLL_MAX: usize = 1_000_000_000;
thread_local! { static VIEWS: RefCell<Vec<HWND>> = const { RefCell::new(Vec::new()) }; }

struct Input {
    paths: [Option<PathBuf>; 2],
    pal: theme::Palette,
    ignore_crlf: bool,
    rules: TextRules,
    encodings: [EncodingChoice; 2],
}
enum Request {
    View { serial: u64, top: usize, column: u64, count: usize, width: usize, mapping: Arc<RowMap> },
    Find { serial: u64, query: String, from: [(usize, u64); 2] },
    Copy { serial: u64, selection: Selection },
}
#[derive(Clone, Copy)]
struct Selection {
    side: usize,
    anchor: (usize, u64),
    end: (usize, u64),
}
struct Mailbox {
    request: Mutex<Option<Request>>,
    wake: Condvar,
    closed: AtomicBool,
}
impl Mailbox {
    fn send(&self, request: Request) {
        *self.request.lock().unwrap_or_else(|e| e.into_inner()) = Some(request);
        self.wake.notify_one();
    }
    fn receive(&self) -> Option<Request> {
        let mut lock = self.request.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if self.closed.load(Relaxed) {
                return None;
            }
            if let Some(r) = lock.take() {
                return Some(r);
            }
            lock = self.wake.wait(lock).unwrap_or_else(|e| e.into_inner());
        }
    }
}
struct Rendered {
    aligned: usize,
    folded: Option<std::ops::Range<usize>>,
    source: [Option<usize>; 2],
    text: [String; 2],
    equal: bool,
    ignored: bool,
    inline: Option<Arc<InlineDiff>>,
    lex: [syntax::LexState; 2],
    ending: [&'static str; 2],
}
struct Frame {
    serial: u64,
    top: usize,
    column: u64,
    lines: Vec<Rendered>,
    coarse: bool,
    mapping: Arc<RowMap>,
}
enum Reply {
    Ready(Arc<TextDiff>),
    Frame(Frame),
    Found(u64, Option<(usize, u64)>),
    Error(io::Error),
    Copied(u64, io::Result<Vec<u16>>),
}
type Replies = Arc<Mutex<Option<Reply>>>;

struct State {
    paths: [Option<PathBuf>; 2],
    pal: theme::Palette,
    dpi: u32,
    font: HFONT,
    code: HFONT,
    cell: i32,
    line: i32,
    text_height: i32,
    search_rect: RECT,
    controls: [HWND; 7],
    progress: Arc<Progress>,
    mailbox: Arc<Mailbox>,
    replies: Replies,
    epoch: Arc<AtomicU64>,
    pair: Option<Arc<TextDiff>>,
    frame: Option<Frame>,
    top: usize,
    column: u64,
    selection: Option<usize>,
    syntax: bool,
    ignore_crlf: bool,
    rules: TextRules,
    encodings: [EncodingChoice; 2],
    loading: bool,
    status: String,
    summary: String,
    detail: String,
    mapping: Arc<RowMap>,
    map_bins: Vec<bool>,
    fold: bool,
    expanded_folds: Vec<usize>,
    text_selection: Option<Selection>,
    selecting: bool,
    active_side: usize,
    copying: bool,
    query: String,
    searching: bool,
    hit: Option<(usize, u64)>,
    wheel: i32,
    worker: Option<std::thread::JoinHandle<()>>,
    drop_target: Option<crate::drop_target::Registration>,
    drop_hint: Option<crate::drop_target::Feedback>,
}
fn cancel_worker(s: &State) {
    s.progress.cancel.store(true, Relaxed);
    s.epoch.fetch_add(1, Relaxed);
    {
        let _lock = s.mailbox.request.lock().unwrap_or_else(|e| e.into_inner());
        s.mailbox.closed.store(true, Relaxed);
        s.mailbox.wake.notify_one();
    }
    if let Some(worker) = &s.worker {
        unsafe {
            windows_sys::Win32::System::IO::CancelSynchronousIo(worker.as_raw_handle().cast());
        }
    }
}
impl Drop for State {
    fn drop(&mut self) {
        cancel_worker(self);
        unsafe {
            DeleteObject(self.font);
            DeleteObject(self.code);
        }
    }
}

pub fn drop_state(h: HWND) -> (bool, i32) {
    with(h, |s| (s.loading || s.copying, client(h).right / 2)).unwrap_or((true, 0))
}
pub fn drop_feedback(h: HWND, hint: Option<crate::drop_target::Feedback>) {
    if with(h, |s| {
        if s.drop_hint == hint {
            false
        } else {
            s.drop_hint = hint;
            true
        }
    })
    .unwrap_or(false)
    {
        unsafe {
            InvalidateRect(h, null(), 0);
        }
    }
}
fn launch(h: HWND, previous: Option<std::thread::JoinHandle<()>>) {
    let Some((input, progress, mailbox, replies, epoch)) = with(h, |s| {
        (
            Input { paths: s.paths.clone(), pal: s.pal, ignore_crlf: s.ignore_crlf, rules: s.rules, encodings: s.encodings },
            s.progress.clone(),
            s.mailbox.clone(),
            s.replies.clone(),
            s.epoch.clone(),
        )
    }) else {
        return;
    };
    let handle = h as usize;
    let result = std::thread::Builder::new().name("text-comparison".into()).spawn(move || {
        // The replacement waits off the UI thread. Old indexes are released
        // before the next pair is indexed, bounding memory across repeated drops.
        if let Some(previous) = previous {
            let _ = previous.join();
        }
        worker(handle, input, progress, mailbox, replies, epoch);
    });
    match result {
        Ok(worker) => {
            with(h, |s| s.worker = Some(worker));
            unsafe {
                SetTimer(h, 1, 150, None);
            }
        }
        Err(e) => {
            with(h, |s| {
                s.loading = false;
                s.status = lang::error(&e);
            });
            unsafe {
                KillTimer(h, 1);
            }
        }
    }
    labels(h);
    unsafe {
        InvalidateRect(h, null(), 0);
    }
}
fn replace_sources(h: HWND, paths: [Option<PathBuf>; 2]) {
    let previous = with(h, |s| {
        cancel_worker(s);
        let previous = s.worker.take();
        s.progress = Arc::new(Progress::default());
        s.mailbox = Arc::new(Mailbox { request: Mutex::new(None), wake: Condvar::new(), closed: AtomicBool::new(false) });
        s.replies = Arc::new(Mutex::new(None));
        s.epoch = Arc::new(AtomicU64::new(0));
        s.paths = paths;
        s.pair = None;
        s.frame = None;
        s.loading = true;
        s.top = 0;
        s.column = 0;
        s.selection = None;
        s.hit = None;
        s.searching = false;
        s.status = t(S::IndexingText).into();
        s.summary.clear();
        s.detail.clear();
        s.mapping = Arc::new(RowMap::direct(0));
        s.map_bins.clear();
        s.expanded_folds.clear();
        s.text_selection = None;
        s.selecting = false;
        s.copying = false;
        previous
    })
    .flatten();
    launch(h, previous);
}
fn accept_drop(h: HWND) {
    let Some((pending, current, loading)) = with(h, |s| (s.drop_target.as_ref().and_then(|t| t.take()), s.paths.clone(), s.loading)) else {
        return;
    };
    let Some(pending) = pending else { return };
    accept_pending(h, pending, current, loading);
}
fn accept_pending(h: HWND, pending: crate::drop_target::Pending, current: [Option<PathBuf>; 2], loading: bool) {
    if loading {
        message(h, t(S::DropBusy), false);
        return;
    }
    match crate::cmds::validate_drop(&pending) {
        Err(e) => message(h, &lang::error(&e), false),
        Ok(crate::cmds::SourceKind::Folder) => {
            crate::cmds::accept_checked(pending, crate::cmds::SourceKind::Folder);
            if let Some(main) = crate::app::with(|a| a.w.main) {
                unsafe {
                    ShowWindow(main, SW_RESTORE);
                    SetForegroundWindow(main);
                }
            }
        }
        Ok(crate::cmds::SourceKind::File) => replace_sources(h, crate::drop_target::assign(current, pending)),
    }
}

fn with<T>(h: HWND, f: impl FnOnce(&mut State) -> T) -> Option<T> {
    let p = unsafe { GetWindowLongPtrW(h, GWLP_USERDATA) } as *const RefCell<State>;
    if p.is_null() {
        return None;
    }
    unsafe { (&*p).try_borrow_mut().ok().map(|mut s| f(&mut s)) }
}
fn scale(dpi: u32, n: i32) -> i32 {
    n * dpi as i32 / 96
}
fn body(h: HWND, s: &State) -> RECT {
    let c = client(h);
    let top = toolbar_height(s) + scale(s.dpi, 40);
    rect(0, top, (c.right - scale(s.dpi, 14)).max(0), (c.bottom - 2 * status_height(s)).max(top))
}
fn toolbar_height(s: &State) -> i32 {
    scale(s.dpi, 24).max(s.text_height + scale(s.dpi, 8))
}
fn status_height(s: &State) -> i32 {
    scale(s.dpi, 22).max(s.text_height + scale(s.dpi, 6))
}
fn gutter(s: &State) -> i32 {
    let digits = s.pair.as_ref().map_or(4, |p| p.left.line_count().max(p.right.line_count()).max(1).ilog10() as i32 + 1);
    (digits + 2) * s.cell
}
fn visible(h: HWND, s: &State) -> (usize, usize) {
    let r = body(h, s);
    (
        ((r.bottom - r.top) / s.line.max(1) + 1).clamp(1, 256) as usize,
        ((r.right / 2 - gutter(s) - scale(s.dpi, 12)) / s.cell.max(1) - 6).clamp(1, 8192) as usize,
    )
}
fn post(h: usize, replies: &Replies, reply: Reply) {
    *replies.lock().unwrap_or_else(|e| e.into_inner()) = Some(reply);
    unsafe {
        PostMessageW(h as HWND, WORK, 0, 0);
    }
}

fn worker(h: usize, input: Input, progress: Arc<Progress>, mailbox: Arc<Mailbox>, replies: Replies, epoch: Arc<AtomicU64>) {
    let run = || -> io::Result<()> {
        let options = TextOptions { ignore_crlf: input.ignore_crlf, rules: input.rules, encodings: input.encodings, ..Default::default() };
        let pair = Arc::new(TextDiff::open(input.paths[0].as_deref(), input.paths[1].as_deref(), options, &progress)?);
        post(h, &replies, Reply::Ready(pair.clone()));
        let mut a = pair.left.reader();
        let mut b = pair.right.reader();
        // Bounded reuse across horizontal scrolling. Large results are not retained.
        let mut inline_cache: VecDeque<(usize, Arc<InlineDiff>)> = VecDeque::new();
        while let Some(request) = mailbox.receive() {
            progress.check()?;
            match request {
                Request::View { serial, top, column, count, width, mapping } => {
                    let mut lines = Vec::with_capacity(count);
                    let mut coarse = false;
                    for displayed in top..top.saturating_add(count).min(mapping.rows) {
                        if epoch.load(Relaxed) != serial {
                            break;
                        }
                        let (row, folded) = mapping.at(displayed).unwrap();
                        if let Some(folded) = folded {
                            lines.push(Rendered {
                                aligned: row,
                                folded: Some(folded),
                                source: [None, None],
                                text: [String::new(), String::new()],
                                equal: true,
                                ignored: true,
                                inline: None,
                                lex: [Default::default(); 2],
                                ending: ["", ""],
                            });
                            continue;
                        }
                        let (left, right, equal) = pair.row(row).unwrap();
                        let inline = if equal {
                            None
                        } else if let Some((_, cached)) = inline_cache.iter().find(|(key, _)| *key == row) {
                            Some(cached.clone())
                        } else {
                            let diff = Arc::new(InlineDiff::between(&mut a, left, &mut b, right, pair.rules, &progress)?);
                            if diff.spans.iter().map(|s| s.capacity() * size_of::<std::ops::Range<u64>>()).sum::<usize>() <= 16 * 1024 {
                                if inline_cache.len() == 256 {
                                    inline_cache.pop_front();
                                }
                                inline_cache.push_back((row, diff.clone()));
                            }
                            Some(diff)
                        };
                        coarse |= inline.as_ref().is_some_and(|d| d.coarse);
                        let (left_text, left_lex) =
                            left.map(|l| a.window_styled(l, column, width, &progress)).transpose()?.unwrap_or_default();
                        let (right_text, right_lex) =
                            right.map(|l| b.window_styled(l, column, width, &progress)).transpose()?.unwrap_or_default();
                        let ending = [left.map_or("", |l| pair.left.line_ending(l)), right.map_or("", |l| pair.right.line_ending(l))];
                        lines.push(Rendered {
                            aligned: row,
                            folded: None,
                            source: [left, right],
                            text: [left_text, right_text],
                            equal,
                            ignored: pair.row_ignored(row),
                            inline,
                            lex: [left_lex, right_lex],
                            ending,
                        });
                    }
                    if epoch.load(Relaxed) == serial {
                        post(h, &replies, Reply::Frame(Frame { serial, top, column, lines, coarse, mapping }));
                    }
                }
                Request::Find { serial, query, from } => {
                    let mut found = None;
                    for (side, doc) in [&pair.left, &pair.right].into_iter().enumerate() {
                        match doc.find(&query, from[side], &progress, || epoch.load(Relaxed) != serial) {
                            Ok(Some((line, column))) => {
                                if let Some(row) = pair.row_of(side == 1, line)
                                    && found.is_none_or(|(old, _)| row < old)
                                {
                                    found = Some((row, column));
                                }
                            }
                            Err(e) if e.kind() == io::ErrorKind::Interrupted && !progress.cancel.load(Relaxed) => break,
                            Err(e) => return Err(e),
                            _ => {}
                        }
                    }
                    if epoch.load(Relaxed) == serial {
                        post(h, &replies, Reply::Found(serial, found));
                    }
                }
                Request::Copy { serial, selection } => {
                    let doc = if selection.side == 0 { &pair.left } else { &pair.right };
                    let result = doc.selection_utf16(selection.anchor, selection.end, 64 * 1024 * 1024, &progress);
                    if epoch.load(Relaxed) == serial {
                        post(h, &replies, Reply::Copied(serial, result));
                    }
                }
            }
        }
        Ok(())
    };
    let result =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).unwrap_or_else(|_| Err(io::Error::other("comparison worker failed")));
    if let Err(e) = result
        && !mailbox.closed.load(Relaxed)
    {
        post(h, &replies, Reply::Error(if progress.cancel.load(Relaxed) { io::ErrorKind::Interrupted.into() } else { e }));
    }
}

unsafe fn fonts(h: HWND, dpi: u32) -> (HFONT, HFONT, i32, i32, i32) {
    unsafe {
        let font = crate::ui::font(dpi);
        let mut lf = LOGFONTW { lfHeight: -scale(dpi, 14), lfPitchAndFamily: FIXED_PITCH | FF_MODERN, ..std::mem::zeroed() };
        let face = wide("Consolas");
        lf.lfFaceName[..face.len()].copy_from_slice(&face);
        let code = CreateFontIndirectW(&lf);
        let dc = GetDC(h);
        let old = SelectObject(dc, code);
        let mut tm = TEXTMETRICW::default();
        GetTextMetricsW(dc, &mut tm);
        SelectObject(dc, font);
        let mut ui = TEXTMETRICW::default();
        GetTextMetricsW(dc, &mut ui);
        SelectObject(dc, old);
        ReleaseDC(h, dc);
        (font, code, tm.tmAveCharWidth.max(1), tm.tmHeight.max(1) + scale(dpi, 3), ui.tmHeight.max(1))
    }
}

unsafe fn create(h: HWND, input: &Input) -> bool {
    unsafe {
        let dpi = GetDpiForWindow(h).max(96);
        set_window_icons(h, dpi);
        let (font, code, cell, line, text_height) = fonts(h, dpi);
        if font.is_null() || code.is_null() {
            DeleteObject(font);
            DeleteObject(code);
            return false;
        }
        let mut controls = [null_mut(); 7];
        for (i, id) in [(0, PREVIOUS), (1, NEXT), (2, SYNTAX), (6, COMPARE_OPTIONS), (3, STOP), (4, SEARCH), (5, FIND)] {
            controls[i] = CreateWindowExW(
                0,
                if id == SEARCH { WC_EDITW } else { WC_BUTTONW },
                wide("").as_ptr(),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | if id == SEARCH { ES_AUTOHSCROLL as u32 } else { BS_OWNERDRAW as u32 },
                0,
                0,
                10,
                10,
                h,
                id as HMENU,
                GetModuleHandleW(null()),
                null(),
            );
            if controls[i].is_null() {
                for c in controls {
                    if !c.is_null() {
                        DestroyWindow(c);
                    }
                }
                DeleteObject(font);
                DeleteObject(code);
                return false;
            }
            send(controls[i], WM_SETFONT, font as usize, 0);
        }
        send(controls[4], EM_SETLIMITTEXT, 4096, 0);
        let progress = Arc::new(Progress::default());
        let mailbox = Arc::new(Mailbox { request: Mutex::new(None), wake: Condvar::new(), closed: AtomicBool::new(false) });
        let replies = Arc::new(Mutex::new(None));
        let epoch = Arc::new(AtomicU64::new(0));
        let state = State {
            paths: input.paths.clone(),
            pal: input.pal,
            dpi,
            font,
            code,
            cell,
            line,
            text_height,
            search_rect: RECT::default(),
            controls,
            progress: progress.clone(),
            mailbox: mailbox.clone(),
            replies: replies.clone(),
            epoch: epoch.clone(),
            pair: None,
            frame: None,
            top: 0,
            column: 0,
            selection: None,
            syntax: true,
            ignore_crlf: input.ignore_crlf,
            rules: input.rules,
            encodings: input.encodings,
            loading: true,
            status: t(S::IndexingText).into(),
            summary: String::new(),
            detail: String::new(),
            mapping: Arc::new(RowMap::direct(0)),
            map_bins: Vec::new(),
            fold: false,
            expanded_folds: Vec::new(),
            text_selection: None,
            selecting: false,
            active_side: usize::from(input.paths[0].is_none()),
            copying: false,
            query: String::new(),
            searching: false,
            hit: None,
            wheel: 0,
            worker: None,
            drop_target: None,
            drop_hint: None,
        };
        let raw = Box::into_raw(Box::new(RefCell::new(state)));
        SetWindowLongPtrW(h, GWLP_USERDATA, raw as isize);
        VIEWS.with(|v| v.borrow_mut().push(h));
        labels(h);
        theme::set_title_dark(h, input.pal.dark);
        theme::style_control(controls[4], input.pal.dark, theme::Kind::Cfd);
        layout(h);
        match crate::drop_target::Registration::new(h, crate::drop_target::Surface::Text) {
            Ok(target) => {
                with(h, |s| s.drop_target = Some(target));
            }
            Err(e) => message(h, &lang::error(&e), true),
        }
        launch(h, None);
        true
    }
}

fn labels(h: HWND) {
    with(h, |s| unsafe {
        for (i, label) in
            [S::PreviousChange, S::NextChange, S::Syntax, S::Stop, S::FindText, S::FindNext, S::TextOptions].into_iter().enumerate()
        {
            if i == 4 {
                send(s.controls[i], EM_SETCUEBANNER, 1, wide(t(label)).as_ptr() as isize);
            } else {
                SetWindowTextW(s.controls[i], wide(t(label)).as_ptr());
            }
        }
        let changes = s.pair.as_ref().is_some_and(|p| !p.result(s.ignore_crlf).change_rows.is_empty());
        EnableWindow(s.controls[0], i32::from(changes));
        EnableWindow(s.controls[1], i32::from(changes));
        EnableWindow(s.controls[5], i32::from(s.pair.is_some()));
        EnableWindow(s.controls[3], i32::from(s.loading));
        SetWindowTextW(h, wide(&format!("FerrumCompare - {}", t(S::TextTitle))).as_ptr());
    });
}

fn layout(h: HWND) {
    with(h, |s| unsafe {
        let c = client(h);
        let d = s.dpi;
        let pad = scale(d, 8);
        let gap = scale(d, 6);
        let height = toolbar_height(s);
        let dc = GetDC(h);
        let old = SelectObject(dc, s.font);
        let mut x = pad;
        for (i, label) in [(0, S::PreviousChange), (1, S::NextChange), (2, S::Syntax), (6, S::TextOptions), (3, S::Stop)] {
            let label = t(label);
            let wide = wide(label);
            let mut size = SIZE::default();
            GetTextExtentPoint32W(dc, wide.as_ptr(), wide.len() as i32 - 1, &mut size);
            let width = (size.cx + scale(d, 20)).max(scale(d, 64));
            MoveWindow(s.controls[i], x, pad, width, height, 1);
            x += width + gap;
        }
        let find_label = wide(t(S::FindNext));
        let mut size = SIZE::default();
        GetTextExtentPoint32W(dc, find_label.as_ptr(), find_label.len() as i32 - 1, &mut size);
        let find_width = size.cx + scale(d, 20);
        let search_width = (c.right - pad - find_width - gap - x).max(scale(d, 60));
        s.search_rect = rect(x, pad, x + search_width, pad + height);
        MoveWindow(
            s.controls[4],
            x + scale(d, 4),
            pad + (height - s.text_height - 2) / 2,
            (search_width - scale(d, 8)).max(1),
            s.text_height + 2,
            1,
        );
        MoveWindow(s.controls[5], c.right - pad - find_width, pad, find_width, height, 1);
        SelectObject(dc, old);
        ReleaseDC(h, dc);
    });
    request_view(h);
}

fn scroll_limit(total: usize, page: usize) -> usize {
    total.saturating_sub(page.max(1))
}
fn to_bar(position: usize, limit: usize) -> i32 {
    if limit == 0 { 0 } else { (position as u128 * limit.min(SCROLL_MAX) as u128 / limit as u128) as i32 }
}
fn from_bar(position: i32, limit: usize) -> usize {
    if limit == 0 { 0 } else { (position.max(0) as u128 * limit as u128 / limit.min(SCROLL_MAX) as u128) as usize }
}

fn request_view(h: HWND) {
    with(h, |s| unsafe {
        let Some(pair) = &s.pair else {
            InvalidateRect(h, null(), 0);
            return;
        };
        if s.searching {
            s.searching = false;
            s.status = s.summary.clone();
        }
        let (count, width) = visible(h, s);
        if s.copying {
            InvalidateRect(h, null(), 0);
            return;
        }
        let row_limit = scroll_limit(s.mapping.rows, count.saturating_sub(1));
        let col_limit = pair.left.max_columns.max(pair.right.max_columns).saturating_sub(width as u64) as usize;
        s.top = s.top.min(row_limit);
        s.column = s.column.min(col_limit as u64);
        for (bar, pos, limit, page) in [(SB_VERT, s.top, row_limit, count), (SB_HORZ, s.column as usize, col_limit, width)] {
            let page = page.min(4096) as u32;
            let info = SCROLLINFO {
                cbSize: size_of::<SCROLLINFO>() as u32,
                fMask: SIF_RANGE | SIF_PAGE | SIF_POS | SIF_DISABLENOSCROLL,
                nMin: 0,
                nMax: limit.min(SCROLL_MAX) as i32 + page as i32 - 1,
                nPage: page,
                nPos: to_bar(pos, limit),
                ..Default::default()
            };
            SetScrollInfo(h, bar, &info, 1);
        }
        let serial = s.epoch.fetch_add(1, Relaxed) + 1;
        s.mailbox.send(Request::View {
            serial,
            top: s.top.saturating_sub(count / 2),
            column: s.column,
            count: count * 2,
            width,
            mapping: s.mapping.clone(),
        });
        InvalidateRect(h, null(), 0);
    });
}

fn navigate(h: HWND, next: bool) {
    with(h, |s| {
        let Some(p) = &s.pair else { return };
        let changes = p.result(s.ignore_crlf).change_rows;
        let at = s.selection.unwrap_or_else(|| s.mapping.at(s.top).map_or(0, |r| r.0));
        if changes.is_empty() {
            return;
        }
        let i = if next {
            let i = changes.partition_point(|&r| r < at || r == at && s.selection.is_some());
            if i == changes.len() { 0 } else { i }
        } else {
            changes.partition_point(|&r| r < at).checked_sub(1).unwrap_or(changes.len() - 1)
        };
        if let Some(&row) = changes.get(i) {
            reveal_aligned(s, row);
            s.selection = Some(row);
            s.text_selection = None;
        }
    });
    request_view(h);
}

fn find(h: HWND) {
    with(h, |s| {
        if s.copying {
            return;
        }
        let Some(pair) = &s.pair else { return };
        let query = edit_path(s.controls[4]).to_string_lossy().into_owned();
        if query.is_empty() {
            return;
        }
        let again = query == s.query;
        let aligned = s.mapping.at(s.top).map_or(0, |r| r.0);
        let from = if again { s.hit.map(|(r, c)| (r, c + 1)).unwrap_or((aligned, 0)) } else { (aligned, 0) };
        s.query = query.clone();
        s.searching = true;
        let sources = pair.row(from.0).map(|(a, b, _)| [a, b]).unwrap_or([None, None]);
        let mut starts = [(0, 0); 2];
        for side in 0..2 {
            starts[side] = (
                sources[side].unwrap_or_else(|| {
                    let doc = if side == 0 { &pair.left } else { &pair.right };
                    pair.blocks
                        .iter()
                        .find(|b| b.row > from.0)
                        .map_or(doc.line_count(), |b| if side == 0 { b.left.start } else { b.right.start })
                }),
                from.1,
            );
        }
        let serial = s.epoch.fetch_add(1, Relaxed) + 1;
        s.status = t(S::FindText).into();
        s.mailbox.send(Request::Find { serial, query, from: starts });
    });
    unsafe {
        InvalidateRect(h, null(), 0);
    }
}

fn update_summary(s: &mut State) {
    let Some(pair) = &s.pair else { return };
    let result = pair.result(s.ignore_crlf);
    let outcome = if result.matches_with_settings {
        t(S::SettingsTextMatch)
    } else if pair.blocks.iter().any(|b| !b.equal) {
        lang::state(fc_core::State::Different)
    } else if result.format_differs {
        t(if pair.left.encoding == pair.right.encoding && pair.left.bom == pair.right.bom { S::EndingsText } else { S::FormatText })
    } else {
        t(S::ExactTextMatch)
    };
    s.summary = format!(
        "{} / {} {}. {}: {}. {:.3} {}. {}{}{}",
        lang::number(pair.left.line_count() as u64),
        lang::number(pair.right.line_count() as u64),
        t(S::Lines),
        t(S::Differences),
        result.change_rows.len(),
        pair.elapsed.as_secs_f64(),
        t(S::Seconds),
        outcome,
        if s.ignore_crlf && !result.matches_with_settings { format!(". {}", t(S::IgnoringCrlf)) } else { String::new() },
        if pair.coarse { format!(". {}", t(S::CoarseText)) } else { String::new() }
    );
    s.status = s.summary.clone();
    let active: Vec<_> = [
        (s.rules.ignore_spaces, t(S::SpacesRule)),
        (s.rules.ignore_tabs, t(S::TabsRule)),
        (s.rules.ignore_blank_lines, t(S::BlankLinesRule)),
        (s.rules.ignore_case, t(S::CaseRule)),
        (s.rules.ignore_line_endings, t(S::EndingsRule)),
        (s.ignore_crlf && !s.rules.ignore_line_endings, "CRLF/LF"),
    ]
    .into_iter()
    .filter_map(|(on, label)| on.then_some(label))
    .collect();
    s.detail = format!(
        "{}{} / {}{}. {}",
        pair.left.encoding.name(),
        if pair.left.bom { " BOM" } else { "" },
        pair.right.encoding.name(),
        if pair.right.bom { " BOM" } else { "" },
        if active.is_empty() { t(S::ReadOnlyText).to_owned() } else { format!("{}: {}", t(S::IgnoredRules), active.join(", ")) }
    );
}

fn rebuild_mapping(s: &mut State) {
    let Some(pair) = &s.pair else { return };
    let aligned = s.mapping.at(s.top).map_or(0, |r| r.0);
    if s.fold {
        match RowMap::folded(pair, 3, s.ignore_crlf, &s.expanded_folds, &s.progress) {
            Ok(map) => s.mapping = Arc::new(map),
            Err(e) => {
                s.status = lang::error(&e);
                s.mapping = Arc::new(RowMap::direct(pair.rows));
            }
        }
    } else if s.mapping.rows != pair.rows {
        s.mapping = Arc::new(RowMap::direct(pair.rows));
    }
    s.top = s.mapping.display_of(aligned);
    s.map_bins = pair.change_map(s.ignore_crlf, 512, &s.progress).unwrap_or_default();
}

fn reveal_aligned(s: &mut State, row: usize) {
    if let Some((_, Some(range))) = s.mapping.at(s.mapping.display_of(row)) {
        s.expanded_folds.push(range.start);
        rebuild_mapping(s);
    }
    s.top = s.mapping.display_of(row).saturating_sub(2);
}

fn toggle_folding(h: HWND) {
    with(h, |s| {
        s.fold = !s.fold;
        s.expanded_folds.clear();
        rebuild_mapping(s);
    });
    request_view(h);
}

fn go_line(h: HWND) {
    let Some((side, count, current)) = with(h, |s| {
        let p = s.pair.as_ref()?;
        let side = s.active_side;
        let doc = if side == 0 { &p.left } else { &p.right };
        let aligned = s.mapping.at(s.top).map_or(0, |r| r.0);
        let sources = p.row(aligned).map(|(a, b, _)| [a, b]).unwrap_or([None, None]);
        Some((side, doc.line_count(), sources[side].unwrap_or(0) + 1))
    })
    .flatten() else {
        return;
    };
    match crate::prompt::fields(h, t(S::GoLine), &[t(S::LineNumber)], &[current.to_string()], t(S::GoLineHelp)) {
        Ok(Some(values)) => match values[0].trim().parse::<usize>() {
            Ok(line) if line > 0 && line <= count => {
                with(h, |s| {
                    if let Some(row) = s.pair.as_ref().and_then(|p| p.row_of(side == 1, line - 1)) {
                        reveal_aligned(s, row);
                        s.selection = Some(row);
                        s.text_selection = None;
                    }
                });
                request_view(h);
            }
            _ => message(h, t(S::InvalidLine), false),
        },
        Ok(None) => {}
        Err(e) => message(h, &lang::error(&e), true),
    }
}

fn set_clipboard(h: HWND, value: &[u16]) -> io::Result<()> {
    use windows_sys::Win32::System::{DataExchange::*, Memory::*};
    unsafe {
        if OpenClipboard(h) == 0 {
            return Err(io::Error::last_os_error());
        }
        struct Clipboard;
        impl Drop for Clipboard {
            fn drop(&mut self) {
                unsafe {
                    CloseClipboard();
                }
            }
        }
        let _clipboard = Clipboard;
        let memory = GlobalAlloc(GMEM_MOVEABLE, std::mem::size_of_val(value));
        if memory.is_null() {
            return Err(io::Error::last_os_error());
        }
        let pointer = GlobalLock(memory);
        if pointer.is_null() {
            let e = io::Error::last_os_error();
            GlobalFree(memory);
            return Err(e);
        }
        std::ptr::copy_nonoverlapping(value.as_ptr(), pointer.cast::<u16>(), value.len());
        GlobalUnlock(memory);
        if EmptyClipboard() == 0 || SetClipboardData(windows_sys::Win32::System::Ole::CF_UNICODETEXT as u32, memory).is_null() {
            let e = io::Error::last_os_error();
            GlobalFree(memory);
            return Err(e);
        }
        Ok(())
    }
}

fn copy_selection(h: HWND) {
    with(h, |s| {
        if s.loading || s.copying {
            return;
        }
        let Some(selection) = s.text_selection.filter(|r| r.anchor != r.end) else { return };
        s.copying = true;
        let serial = s.epoch.fetch_add(1, Relaxed) + 1;
        s.mailbox.send(Request::Copy { serial, selection });
    });
}

fn selection_point(h: HWND, s: &State, x: i32, y: i32, side: usize) -> Option<(usize, u64)> {
    let area = body(h, s);
    let row = s.top + ((y - area.top).max(0) / s.line.max(1)) as usize;
    let frame = s.frame.as_ref()?;
    if !Arc::ptr_eq(&frame.mapping, &s.mapping) {
        return None;
    }
    let rendered = frame.lines.get(row.checked_sub(frame.top)?)?;
    let line = rendered.source[side]?;
    let col = (x - side as i32 * (area.right / 2) - gutter(s)).max(0) / s.cell.max(1);
    Some((line, s.column + col as u64))
}

fn drag_selection(h: HWND, x: i32, y: i32) {
    let outside = with(h, |s| {
        if !s.selecting {
            return false;
        }
        let area = body(h, s);
        let outside = y < area.top || y >= area.bottom;
        if y < area.top {
            s.top = s.top.saturating_sub(1);
        } else if y >= area.bottom {
            s.top = s.top.saturating_add(1);
        }
        if let Some(mut range) = s.text_selection
            && let Some(point) = selection_point(h, s, x, y.clamp(area.top, area.bottom.max(area.top + 1) - 1), range.side)
        {
            range.end = point;
            s.text_selection = Some(range);
        }
        outside
    })
    .unwrap_or(false);
    unsafe {
        if outside {
            SetTimer(h, 2, 75, None);
        } else {
            KillTimer(h, 2);
        }
        InvalidateRect(h, null(), 0);
    }
    if outside {
        request_view(h);
    }
}

fn set_ignore_crlf(ignore: bool) {
    crate::app::with(|a| a.settings.ignore_crlf = ignore);
    let windows = VIEWS.with(|v| v.borrow().clone());
    for h in windows {
        with(h, |s| {
            s.ignore_crlf = ignore;
            s.selection = None;
            rebuild_mapping(s);
            update_summary(s);
        });
        labels(h);
        if with(h, |s| s.fold).unwrap_or(false) {
            request_view(h);
        }
        unsafe {
            InvalidateRect(h, null(), 0);
        }
    }
}

fn views_busy() -> bool {
    VIEWS.with(|v| v.borrow().iter().any(|&h| with(h, |s| s.loading || s.copying).unwrap_or(false)))
}

fn set_text_rules(rules: TextRules) {
    if views_busy() {
        return;
    }
    crate::app::with(|a| a.settings.text = rules);
    let windows = VIEWS.with(|v| v.borrow().clone());
    for h in windows {
        let paths = with(h, |s| {
            if s.rules == rules {
                return None;
            }
            s.rules = rules;
            Some(s.paths.clone())
        })
        .flatten();
        if let Some(paths) = paths {
            replace_sources(h, paths);
        }
    }
}

fn comparison_command(h: HWND, id: usize) {
    if id == IGNORE_CRLF {
        if let Some(ignore) = with(h, |s| !s.ignore_crlf) {
            set_ignore_crlf(ignore);
        }
    } else if id == EXACT_RULES {
        if !views_busy() {
            set_ignore_crlf(false);
            set_text_rules(TextRules::default());
        }
    } else if let Some(mut rules) = with(h, |s| s.rules) {
        match id {
            IGNORE_SPACES => rules.ignore_spaces = !rules.ignore_spaces,
            IGNORE_TABS => rules.ignore_tabs = !rules.ignore_tabs,
            IGNORE_BLANK_LINES => rules.ignore_blank_lines = !rules.ignore_blank_lines,
            IGNORE_CASE => rules.ignore_case = !rules.ignore_case,
            IGNORE_ENDINGS => rules.ignore_line_endings = !rules.ignore_line_endings,
            _ => return,
        }
        set_text_rules(rules);
    }
}

fn encoding_command(h: HWND, id: usize) {
    let (side, index) = if (ENCODING_LEFT..ENCODING_LEFT + 5).contains(&id) {
        (0, id - ENCODING_LEFT)
    } else if (ENCODING_RIGHT..ENCODING_RIGHT + 5).contains(&id) {
        (1, id - ENCODING_RIGHT)
    } else {
        return;
    };
    let paths = with(h, |s| {
        if s.loading || s.copying || s.encodings[side] == EncodingChoice::ALL[index] {
            return None;
        }
        s.encodings[side] = EncodingChoice::ALL[index];
        if let Some(pair) = &s.pair {
            let doc = if side == 0 { &pair.left } else { &pair.right };
            let effective = s.encodings[side].encoding().unwrap_or(if doc.bom { doc.encoding } else { fc_core::text::Encoding::Utf8 });
            if effective == doc.encoding {
                return None;
            }
        }
        Some(s.paths.clone())
    })
    .flatten();
    if let Some(paths) = paths {
        replace_sources(h, paths);
    }
}

fn comparison_menu(h: HWND) {
    let Some((button, ignore, rules)) = with(h, |s| (s.controls[6], s.ignore_crlf, s.rules)) else { return };
    let busy = views_busy();
    unsafe {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        AppendMenuW(menu, MF_STRING | if ignore { MF_CHECKED } else { MF_UNCHECKED }, IGNORE_CRLF, wide(t(S::IgnoreCrlf)).as_ptr());
        for (id, label, on) in [
            (IGNORE_ENDINGS, S::IgnoreLineEndings, rules.ignore_line_endings),
            (IGNORE_SPACES, S::IgnoreSpaces, rules.ignore_spaces),
            (IGNORE_TABS, S::IgnoreTabs, rules.ignore_tabs),
            (IGNORE_BLANK_LINES, S::IgnoreBlankLines, rules.ignore_blank_lines),
            (IGNORE_CASE, S::IgnoreTextCase, rules.ignore_case),
        ] {
            AppendMenuW(menu, MF_STRING | if on { MF_CHECKED } else { 0 } | if busy { MF_GRAYED } else { 0 }, id, wide(t(label)).as_ptr());
        }
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        AppendMenuW(menu, MF_STRING | if busy { MF_GRAYED } else { 0 }, EXACT_RULES, wide(t(S::ExactTextRules)).as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        let folded = with(h, |s| s.fold).unwrap_or(false);
        AppendMenuW(menu, MF_STRING | if folded { MF_CHECKED } else { 0 }, FOLD, wide(t(S::FoldUnchanged)).as_ptr());
        AppendMenuW(menu, MF_STRING, GO_LINE, wide(t(S::GoLine)).as_ptr());
        AppendMenuW(menu, MF_STRING, COPY, wide(t(S::CopyText)).as_ptr());
        if let Some((encodings, loading)) = with(h, |s| (s.encodings, s.loading || s.copying)) {
            for (side, label, base) in [(0, S::EncodingLeft, ENCODING_LEFT), (1, S::EncodingRight, ENCODING_RIGHT)] {
                let choices = CreatePopupMenu();
                if choices.is_null() {
                    continue;
                }
                for (i, choice) in EncodingChoice::ALL.into_iter().enumerate() {
                    let label = choice.encoding().map_or(t(S::Auto), |e| e.name());
                    AppendMenuW(
                        choices,
                        MF_STRING | if encodings[side] == choice { MF_CHECKED } else { 0 } | if loading { MF_GRAYED } else { 0 },
                        base + i,
                        wide(label).as_ptr(),
                    );
                }
                AppendMenuW(menu, MF_POPUP, choices as usize, wide(t(label)).as_ptr());
            }
        }
        let mut r = RECT::default();
        GetWindowRect(button, &mut r);
        let id = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_LEFTALIGN, r.left, r.bottom, 0, h, null());
        DestroyMenu(menu);
        match id as usize {
            FOLD => toggle_folding(h),
            GO_LINE => go_line(h),
            COPY => copy_selection(h),
            0 => {}
            n if (ENCODING_LEFT..ENCODING_LEFT + 5).contains(&n) || (ENCODING_RIGHT..ENCODING_RIGHT + 5).contains(&n) => {
                encoding_command(h, n)
            }
            n => comparison_command(h, n),
        }
    }
}

fn receive(h: HWND) {
    let reply = with(h, |s| s.replies.lock().unwrap_or_else(|e| e.into_inner()).take()).flatten();
    let mut render = false;
    match reply {
        Some(Reply::Ready(pair)) => {
            with(h, |s| {
                s.loading = false;
                s.pair = Some(pair);
                rebuild_mapping(s);
                update_summary(s);
            });
            unsafe {
                KillTimer(h, 1);
            }
            labels(h);
            render = true;
        }
        Some(Reply::Frame(frame)) => {
            with(h, |s| {
                if frame.serial == s.epoch.load(Relaxed) {
                    s.frame = Some(frame);
                }
            });
        }
        Some(Reply::Found(serial, hit)) => {
            with(h, |s| {
                if serial == s.epoch.load(Relaxed) {
                    s.searching = false;
                    if let Some((row, column)) = hit {
                        reveal_aligned(s, row);
                        s.column = column.saturating_sub(12);
                        s.selection = Some(row);
                        s.hit = hit;
                        s.text_selection = None;
                        s.status = s.summary.clone();
                        render = true;
                    } else {
                        s.status = format!("{}. {}", t(S::NotFound), s.summary);
                        s.hit = None;
                    }
                }
            });
        }
        Some(Reply::Error(e)) => {
            with(h, |s| {
                s.loading = false;
                s.status = lang::error(&e);
                s.pair = None;
                s.frame = None;
            });
            unsafe {
                KillTimer(h, 1);
            }
            labels(h);
        }
        Some(Reply::Copied(serial, value)) => {
            with(h, |s| s.copying = false);
            if with(h, |s| s.epoch.load(Relaxed) == serial).unwrap_or(false)
                && let Err(e) = value.and_then(|v| set_clipboard(h, &v))
            {
                message(h, &lang::error(&e), true);
            }
            render = true;
        }
        None => {}
    }
    if render {
        request_view(h)
    }
    unsafe {
        InvalidateRect(h, null(), 0);
    }
}

fn colors(pal: theme::Palette, side: usize) -> (u32, u32) {
    if pal.dark {
        if side == 0 { (0x24223c, 0x38336b) } else { (0x293426, 0x355735) }
    } else if side == 0 {
        (0xe9e9ff, 0xb5b5ff)
    } else {
        (0xe8f6e6, 0xb4e4b0)
    }
}

unsafe fn code_text(dc: HDC, x: i32, y: i32, text: &str, color: u32, cell: i32) {
    unsafe {
        let mut units = Vec::with_capacity(text.len());
        let mut advance = Vec::with_capacity(text.len());
        for c in text.chars() {
            let mut b = [0; 2];
            let encoded = c.encode_utf16(&mut b);
            for (i, &u) in encoded.iter().enumerate() {
                units.push(u);
                advance.push(if i == 0 { cell } else { 0 });
            }
        }
        SetTextColor(dc, color);
        if !units.is_empty() {
            ExtTextOutW(dc, x, y, 0, null(), units.as_ptr(), units.len() as u32, advance.as_ptr());
        }
    }
}

unsafe fn paint(h: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let target = BeginPaint(h, &mut ps);
        let c = client(h);
        let dc = CreateCompatibleDC(target);
        let bitmap = CreateCompatibleBitmap(target, c.right.max(1), c.bottom.max(1));
        if dc.is_null() || bitmap.is_null() {
            if !dc.is_null() {
                DeleteDC(dc);
            }
            if !bitmap.is_null() {
                DeleteObject(bitmap);
            }
            EndPaint(h, &ps);
            return;
        }
        let old_bitmap = SelectObject(dc, bitmap);
        with(h, |s| {
            fill(dc, &c, s.pal.bg);
            fill(dc, &s.search_rect, s.pal.input);
            frame_border(dc, &s.search_rect, if GetFocus() == s.controls[4] { s.pal.accent } else { s.pal.border });
            SetBkMode(dc, TRANSPARENT as i32);
            let old_font = SelectObject(dc, s.font);
            let area = body(h, s);
            fill(dc, &area, s.pal.list);
            let mid = area.right / 2;
            let path_y = toolbar_height(s) + scale(s.dpi, 12);
            for side in 0..2 {
                let x = side as i32 * mid;
                let name = s.paths[side].as_ref().map(|p| p.display().to_string()).unwrap_or_default();
                text(dc, &name, &rect(x + scale(s.dpi, 8), path_y, x + mid - scale(s.dpi, 8), area.top), s.pal.text, 0);
            }
            let clip = SaveDC(dc);
            IntersectClipRect(dc, area.left, area.top, area.right, area.bottom);
            SelectObject(dc, s.code);
            let gutter = gutter(s);
            let count = visible(h, s).0;
            let current = s.frame.as_ref().filter(|f| {
                Arc::ptr_eq(&f.mapping, &s.mapping)
                    && f.top <= s.top
                    && (s.top < f.top + f.lines.len() || f.lines.is_empty())
                    && f.column == s.column
            });
            if let Some(frame) = current {
                for (i, row) in frame.lines.iter().skip(s.top - frame.top).take(count).enumerate() {
                    let y = area.top + i as i32 * s.line;
                    if let Some(range) = &row.folded {
                        for side in 0..2 {
                            let pane = rect(side * mid, y, (side + 1) * mid, y + s.line);
                            fill(dc, &pane, s.pal.panel);
                            text(dc, &format!("{} {}", lang::number(range.len() as u64), t(S::FoldedLines)), &pane, s.pal.dim, DT_CENTER);
                        }
                        continue;
                    }
                    for side in 0..2 {
                        let x = side as i32 * mid;
                        let pane = rect(x, y, if side == 0 { mid } else { area.right }, y + s.line);
                        let saved = SaveDC(dc);
                        IntersectClipRect(dc, pane.left, pane.top, pane.right, pane.bottom);
                        let (background, inline) = colors(s.pal, side);
                        if !row.equal {
                            fill(dc, &pane, if row.source[side].is_some() { background } else { s.pal.panel });
                        }
                        if let Some(line) = row.source[side] {
                            text(
                                dc,
                                &(line + 1).to_string(),
                                &rect(x + scale(s.dpi, 4), y, x + gutter - scale(s.dpi, 8), y + s.line),
                                s.pal.dim,
                                DT_RIGHT,
                            );
                            let start = x + gutter;
                            let len = row.text[side].chars().count();
                            if let Some(diff) = &row.inline {
                                for span in &diff.spans[side] {
                                    let first = span.start.saturating_sub(s.column).min(len as u64) as i32;
                                    let end = span.end.saturating_sub(s.column).min(len as u64) as i32;
                                    if first < end {
                                        fill(dc, &rect(start + first * s.cell, y, start + end * s.cell, y + s.line), inline);
                                    }
                                }
                            }
                            if !s.query.is_empty() {
                                for (offset, found) in row.text[side].match_indices(&s.query) {
                                    let first = row.text[side][..offset].chars().count();
                                    let n = found.chars().count();
                                    fill(
                                        dc,
                                        &rect(start + first as i32 * s.cell, y, start + (first + n) as i32 * s.cell, y + s.line),
                                        if s.pal.dark { 0x30566a } else { 0x99eeff },
                                    );
                                }
                            }
                            if let Some(selected) = s.text_selection.filter(|r| r.side == side) {
                                let (first, last) = if selected.anchor <= selected.end {
                                    (selected.anchor, selected.end)
                                } else {
                                    (selected.end, selected.anchor)
                                };
                                if line >= first.0 && line <= last.0 {
                                    let left = if line == first.0 { first.1 } else { 0 };
                                    let right = if line == last.0 { last.1 } else { u64::MAX };
                                    let a = left.saturating_sub(s.column).min(len as u64) as i32;
                                    let b = right.saturating_sub(s.column).min(len as u64 + 1) as i32;
                                    if a < b {
                                        fill(dc, &rect(start + a * s.cell, y, start + b * s.cell, y + s.line), s.pal.sel);
                                    }
                                }
                            }
                            let language = if s.syntax {
                                s.paths[side].as_deref().map(syntax::language).unwrap_or(syntax::Language::Plain)
                            } else {
                                syntax::Language::Plain
                            };
                            let mut dx = 0;
                            for token in syntax::tokens_from(&row.text[side], language, row.lex[side]) {
                                let part = &row.text[side][token.range];
                                let color = match token.kind {
                                    Kind::Text => s.pal.text,
                                    Kind::Keyword => {
                                        if s.pal.dark {
                                            0xe6b38b
                                        } else {
                                            0x9b3000
                                        }
                                    }
                                    Kind::String => {
                                        if s.pal.dark {
                                            0x85b8ce
                                        } else {
                                            0x2442a0
                                        }
                                    }
                                    Kind::Number => {
                                        if s.pal.dark {
                                            0x95ceb5
                                        } else {
                                            0x506b00
                                        }
                                    }
                                    Kind::Comment => {
                                        if s.pal.dark {
                                            0x7caf79
                                        } else {
                                            0x367030
                                        }
                                    }
                                };
                                code_text(dc, start + dx, y + scale(s.dpi, 1), part, color, s.cell);
                                dx += part.chars().count() as i32 * s.cell;
                            }
                            let ending = rect(pane.right - 6 * s.cell, y, pane.right - scale(s.dpi, 4), y + s.line);
                            let ending_changed = row.source.iter().all(Option::is_some) && row.ending[0] != row.ending[1];
                            let ignored = row.ignored
                                || s.ignore_crlf && matches!((row.ending[0], row.ending[1]), ("CRLF", "LF") | ("LF", "CRLF"))
                                || s.rules.ignore_line_endings && row.ending.iter().all(|&e| e != "EOF");
                            let (marker_bg, marker_text) = if ending_changed && !ignored {
                                if s.pal.dark { (0x24404D, 0x70BFFF) } else { (0xD9F1FF, 0x006090) }
                            } else {
                                (if row.equal { s.pal.list } else { background }, s.pal.dim)
                            };
                            fill(dc, &ending, marker_bg);
                            text(dc, row.ending[side], &ending, marker_text, DT_RIGHT);
                        }
                        if s.selection == Some(row.aligned) {
                            frame_border(dc, &pane, s.pal.accent);
                        }
                        RestoreDC(dc, saved);
                    }
                }
            } else {
                SelectObject(dc, s.font);
                text(dc, if s.pair.is_none() { &s.status } else { t(S::LoadingText) }, &area, s.pal.dim, DT_CENTER);
            }
            fill(dc, &rect(mid, area.top, mid + 1, area.bottom), s.pal.border);
            RestoreDC(dc, clip);
            let map = rect(area.right, area.top, c.right, area.bottom);
            fill(dc, &map, s.pal.panel);
            let height = (area.bottom - area.top).max(1) as usize;
            for (i, &changed) in s.map_bins.iter().enumerate() {
                if changed {
                    let y = area.top + (i * height / s.map_bins.len()) as i32;
                    let end = area.top + ((i + 1) * height / s.map_bins.len()) as i32;
                    fill(dc, &rect(map.left + 2, y, map.right - 2, end.max(y + 1)), s.pal.accent);
                }
            }
            if let Some(pair) = &s.pair
                && pair.rows != 0
            {
                let a = s.mapping.at(s.top).map_or(0, |r| r.0);
                let b = s.mapping.at((s.top + count).min(s.mapping.rows.saturating_sub(1))).map_or(pair.rows, |r| r.0 + 1);
                let y = area.top + (a as u128 * height as u128 / pair.rows as u128) as i32;
                let end = area.top + (b as u128 * height as u128 / pair.rows as u128) as i32;
                frame_border(dc, &rect(map.left, y, map.right, end.max(y + 3)), s.pal.text);
            }
            if let Some(hint) = s.drop_hint.filter(|h| h.accepted()) {
                for side in 0..2 {
                    if hint.zone.contains(side) {
                        crate::util::frame(
                            dc,
                            &rect(side as i32 * mid, area.top, if side == 0 { mid } else { area.right }, area.bottom),
                            s.pal.accent,
                            scale(s.dpi, 2).max(1),
                        );
                    }
                }
            }
            SelectObject(dc, s.font);
            let mut status = s.status.clone();
            if s.loading {
                status = format!(
                    "{}: {}",
                    t(if s.progress.phase.load(Relaxed) == 6 { S::AligningText } else { S::IndexingText }),
                    lang::size(s.progress.bytes.load(Relaxed))
                );
            }
            if let Some(hint) = s.drop_hint {
                status = hint.text().into();
            } else if current.is_some_and(|f| f.coarse) {
                status.push_str(&format!(". {}", t(S::CoarseInline)));
            }
            if s.fold && s.mapping.limited {
                status.push_str(&format!(". {}", t(S::FoldLimit)));
            }
            text(
                dc,
                &status,
                &rect(scale(s.dpi, 8), area.bottom + scale(s.dpi, 2), c.right - scale(s.dpi, 8), area.bottom + status_height(s)),
                s.pal.text,
                0,
            );
            text(dc, &s.detail, &rect(scale(s.dpi, 8), area.bottom + status_height(s), c.right - scale(s.dpi, 8), c.bottom), s.pal.dim, 0);
            SelectObject(dc, old_font);
        });
        BitBlt(target, 0, 0, c.right, c.bottom, dc, 0, 0, SRCCOPY);
        SelectObject(dc, old_bitmap);
        DeleteObject(bitmap);
        DeleteDC(dc);
        EndPaint(h, &ps);
    }
}
fn frame_border(dc: HDC, r: &RECT, color: u32) {
    crate::util::frame(dc, r, color, 1);
}

fn scroll(h: HWND, bar: i32, command: u32) {
    with(h, |s| unsafe {
        let Some(pair) = &s.pair else { return };
        let (count, width) = visible(h, s);
        let vertical = bar == SB_VERT;
        let limit = if vertical {
            scroll_limit(s.mapping.rows, count.saturating_sub(1))
        } else {
            pair.left.max_columns.max(pair.right.max_columns).saturating_sub(width as u64) as usize
        };
        let position = if vertical { s.top } else { s.column as usize };
        let page = if vertical { count.saturating_sub(2).max(1) } else { width.saturating_sub(4).max(1) };
        let next = match command as i32 {
            SB_LINEUP => position.saturating_sub(if vertical { 1 } else { 4 }),
            SB_LINEDOWN => position.saturating_add(if vertical { 1 } else { 4 }),
            SB_PAGEUP => position.saturating_sub(page),
            SB_PAGEDOWN => position.saturating_add(page),
            SB_TOP => 0,
            SB_BOTTOM => limit,
            SB_THUMBTRACK | SB_THUMBPOSITION => {
                let mut info = SCROLLINFO { cbSize: size_of::<SCROLLINFO>() as u32, fMask: SIF_TRACKPOS, ..Default::default() };
                GetScrollInfo(h, bar, &mut info);
                from_bar(info.nTrackPos, limit)
            }
            _ => position,
        }
        .min(limit);
        if vertical { s.top = next } else { s.column = next as u64 }
    });
    request_view(h);
}

unsafe extern "system" fn procedure(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_CREATE => {
                let info = &*(l as *const CREATESTRUCTW);
                return if create(h, &*(info.lpCreateParams as *const Input)) { 0 } else { -1 };
            }
            WM_ERASEBKGND => return 1,
            WM_PAINT => {
                paint(h);
                return 0;
            }
            WM_SIZE => {
                layout(h);
                return 0;
            }
            WORK => {
                receive(h);
                return 0;
            }
            crate::drop_target::WM_FILES => {
                accept_drop(h);
                return 0;
            }
            WM_DROPFILES => {
                match crate::drop_target::legacy(h, crate::drop_target::Surface::Text, w as _) {
                    Ok(pending) => {
                        if let Some((current, loading)) = with(h, |s| (s.paths.clone(), s.loading)) {
                            accept_pending(h, pending, current, loading);
                        }
                    }
                    Err(reason) => message(h, crate::drop_target::Feedback { zone: crate::drop_target::Zone::Both, reason }.text(), false),
                }
                return 0;
            }
            WM_TIMER => {
                if w == 1 {
                    InvalidateRect(h, null(), 0);
                }
                if w == 2 {
                    let mut p = POINT::default();
                    GetCursorPos(&mut p);
                    ScreenToClient(h, &mut p);
                    drag_selection(h, p.x, p.y);
                }
                return 0;
            }
            WM_COMMAND => {
                match w & 0xffff {
                    PREVIOUS => navigate(h, false),
                    NEXT => navigate(h, true),
                    SYNTAX => {
                        with(h, |s| s.syntax = !s.syntax);
                        InvalidateRect(h, null(), 0);
                    }
                    COMPARE_OPTIONS => comparison_menu(h),
                    FOLD => toggle_folding(h),
                    GO_LINE => go_line(h),
                    COPY => copy_selection(h),
                    id @ IGNORE_CRLF..=EXACT_RULES => comparison_command(h, id),
                    STOP => {
                        with(h, |s| {
                            s.progress.cancel.store(true, Relaxed);
                            if let Some(worker) = &s.worker {
                                windows_sys::Win32::System::IO::CancelSynchronousIo(worker.as_raw_handle().cast());
                            }
                            s.status = t(S::Stopping).into();
                        });
                    }
                    FIND => find(h),
                    SEARCH => {
                        if matches!((w >> 16) as u32, EN_SETFOCUS | EN_KILLFOCUS) {
                            InvalidateRect(h, null(), 0);
                        }
                    }
                    _ => {}
                }
                return 0;
            }
            WM_DRAWITEM => {
                let d = &*(l as *const DRAWITEMSTRUCT);
                with(h, |s| {
                    let disabled = d.itemState & ODS_DISABLED != 0;
                    let selected = d.CtlID as usize == SYNTAX && s.syntax
                        || d.CtlID as usize == COMPARE_OPTIONS && (s.ignore_crlf || s.rules.active());
                    fill(d.hDC, &d.rcItem, if selected || d.itemState & ODS_SELECTED != 0 { s.pal.sel } else { s.pal.panel });
                    frame_border(d.hDC, &d.rcItem, if d.itemState & ODS_FOCUS != 0 { s.pal.accent } else { s.pal.border });
                    let label = match d.CtlID as usize {
                        PREVIOUS => S::PreviousChange,
                        NEXT => S::NextChange,
                        SYNTAX => S::Syntax,
                        STOP => S::Stop,
                        COMPARE_OPTIONS => S::TextOptions,
                        _ => S::FindNext,
                    };
                    let old = SelectObject(d.hDC, s.font);
                    SetBkMode(d.hDC, TRANSPARENT as i32);
                    text(d.hDC, t(label), &d.rcItem, if disabled { s.pal.dim } else { s.pal.text }, DT_CENTER);
                    SelectObject(d.hDC, old);
                });
                return 1;
            }
            WM_CTLCOLOREDIT => {
                if let Some(p) = with(h, |s| s.pal) {
                    SetTextColor(w as HDC, p.text);
                    SetBkColor(w as HDC, p.input);
                    SetDCBrushColor(w as HDC, p.input);
                    return GetStockObject(DC_BRUSH) as isize;
                }
            }
            WM_VSCROLL | WM_HSCROLL => {
                scroll(h, if msg == WM_VSCROLL { SB_VERT } else { SB_HORZ }, (w & 0xffff) as u32);
                return 0;
            }
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                let steps = with(h, |s| {
                    s.wheel += (w >> 16) as i16 as i32;
                    let n = s.wheel / 120;
                    s.wheel %= 120;
                    n
                })
                .unwrap_or(0);
                let horizontal = msg == WM_MOUSEHWHEEL || GetKeyState(0x10) < 0;
                with(h, |s| {
                    let n = steps as i64 * 3;
                    if horizontal {
                        s.column = s.column.saturating_add_signed(if msg == WM_MOUSEHWHEEL { n } else { -n });
                    } else {
                        s.top = s.top.saturating_add_signed(-n as isize);
                    }
                });
                request_view(h);
                return 0;
            }
            WM_LBUTTONDOWN => {
                SetFocus(h);
                let x = l as i16 as i32;
                let y = (l >> 16) as i16 as i32;
                let capture = with(h, |s| {
                    let r = body(h, s);
                    if y < r.top || y >= r.bottom {
                        return false;
                    }
                    if x >= r.right {
                        if let Some(pair) = &s.pair {
                            let row = ((y - r.top) as u128 * pair.rows as u128 / (r.bottom - r.top).max(1) as u128) as usize;
                            reveal_aligned(s, row.min(s.pair.as_ref().unwrap().rows.saturating_sub(1)));
                        }
                        return true;
                    }
                    let displayed = s.top + ((y - r.top) / s.line) as usize;
                    if let Some((aligned, folded)) = s.mapping.at(displayed) {
                        s.selection = Some(aligned);
                        if let Some(range) = folded {
                            s.expanded_folds.push(range.start);
                            rebuild_mapping(s);
                            return false;
                        }
                    }
                    let side = usize::from(x >= r.right / 2);
                    s.active_side = side;
                    if let Some(point) = selection_point(h, s, x, y, side) {
                        let anchor = if GetKeyState(0x10) < 0 {
                            s.text_selection.filter(|r| r.side == side).map_or(point, |r| r.anchor)
                        } else {
                            point
                        };
                        s.text_selection = Some(Selection { side, anchor, end: point });
                        s.selecting = true;
                        return true;
                    }
                    false
                })
                .unwrap_or(false);
                if capture {
                    SetCapture(h);
                }
                request_view(h);
                InvalidateRect(h, null(), 0);
                return 0;
            }
            WM_MOUSEMOVE => {
                let x = l as i16 as i32;
                let y = (l >> 16) as i16 as i32;
                if with(h, |s| s.selecting).unwrap_or(false) {
                    drag_selection(h, x, y);
                } else if w & 1 != 0 {
                    // MK_LBUTTON; only the overview strip initiates map dragging.
                    with(h, |s| {
                        let r = body(h, s);
                        if x >= r.right
                            && y >= r.top
                            && y < r.bottom
                            && let Some(pair) = &s.pair
                        {
                            let row = ((y - r.top) as u128 * pair.rows as u128 / (r.bottom - r.top).max(1) as u128) as usize;
                            reveal_aligned(s, row);
                        }
                    });
                    request_view(h);
                }
                return 0;
            }
            WM_LBUTTONUP => {
                with(h, |s| s.selecting = false);
                KillTimer(h, 2);
                ReleaseCapture();
                return 0;
            }
            WM_CAPTURECHANGED => {
                with(h, |s| s.selecting = false);
                KillTimer(h, 2);
                return 0;
            }
            WM_GETMINMAXINFO => {
                let dpi = GetDpiForWindow(h).max(96);
                let info = &mut *(l as *mut MINMAXINFO);
                info.ptMinTrackSize = POINT { x: scale(dpi, 840), y: scale(dpi, 400) };
                return 0;
            }
            WM_DPICHANGED => {
                let dpi = (w & 0xffff) as u32;
                set_window_icons(h, dpi);
                let (font, code, cell, line, text_height) = fonts(h, dpi);
                if font.is_null() || code.is_null() {
                    DeleteObject(font);
                    DeleteObject(code);
                    return 0;
                }
                with(h, |s| {
                    let old = (s.font, s.code);
                    s.font = font;
                    s.code = code;
                    s.cell = cell;
                    s.line = line;
                    s.text_height = text_height;
                    s.dpi = dpi;
                    for c in s.controls {
                        send(c, WM_SETFONT, font as usize, 1);
                    }
                    DeleteObject(old.0);
                    DeleteObject(old.1);
                });
                let r = &*(l as *const RECT);
                SetWindowPos(h, null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                layout(h);
                return 0;
            }
            WM_SETTINGCHANGE | WM_THEMECHANGED => {
                if let Some(pal) = crate::app::with(|a| a.pal) {
                    with(h, |s| s.pal = pal);
                    theme::set_title_dark(h, pal.dark);
                    InvalidateRect(h, null(), 0);
                }
            }
            WM_CLOSE => {
                DestroyWindow(h);
                return 0;
            }
            WM_DESTROY => {
                drop(with(h, |s| s.drop_target.take()));
            }
            WM_NCDESTROY => {
                KillTimer(h, 1);
                KillTimer(h, 2);
                VIEWS.with(|v| v.borrow_mut().retain(|&window| window != h));
                let p = GetWindowLongPtrW(h, GWLP_USERDATA) as *mut RefCell<State>;
                SetWindowLongPtrW(h, GWLP_USERDATA, 0);
                if !p.is_null() {
                    drop(Box::from_raw(p));
                }
            }
            _ => {}
        }
        DefWindowProcW(h, msg, w, l)
    }
}

pub fn open(owner: HWND, paths: [Option<PathBuf>; 2]) {
    if paths.iter().all(Option::is_none) {
        return;
    }
    let pal = crate::app::with(|a| a.pal).unwrap_or(theme::LIGHT);
    let ignore_crlf = crate::app::with(|a| a.settings.ignore_crlf).unwrap_or(false);
    let rules = crate::app::with(|a| a.settings.text).unwrap_or_default();
    let input = Input { paths, pal, ignore_crlf, rules, encodings: [EncodingChoice::Auto; 2] };
    let existing = VIEWS.with(|v| v.borrow().iter().copied().find(|&h| with(h, |s| s.paths == input.paths).unwrap_or(false)));
    if let Some(h) = existing {
        unsafe {
            ShowWindow(h, SW_RESTORE);
            SetForegroundWindow(h);
        }
        return;
    }
    unsafe {
        let class = wide(CLASS);
        let instance = GetModuleHandleW(null());
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(procedure),
            hInstance: instance,
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hIcon: LoadIconW(instance, std::ptr::without_provenance(1)),
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassExW(&wc);
        let dpi = GetDpiForWindow(owner).max(96);
        let mut work = RECT::default();
        let monitor = MonitorFromWindow(owner, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..std::mem::zeroed() };
        if GetMonitorInfoW(monitor, &mut info) != 0 {
            work = info.rcWork;
        } else {
            SystemParametersInfoW(SPI_GETWORKAREA, 0, (&mut work as *mut RECT).cast(), 0);
        }
        let width = scale(dpi, 1180).min(work.right - work.left);
        let height = scale(dpi, 720).min(work.bottom - work.top);
        let h = CreateWindowExW(
            WS_EX_CONTROLPARENT,
            class.as_ptr(),
            wide("FerrumCompare").as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN | WS_VSCROLL | WS_HSCROLL,
            work.left + (work.right - work.left - width) / 2,
            work.top + (work.bottom - work.top - height) / 2,
            width,
            height,
            owner,
            null_mut(),
            instance,
            (&input as *const Input).cast(),
        );
        if h.is_null() {
            message(owner, &io::Error::last_os_error().to_string(), true);
        } else {
            ShowWindow(h, SW_SHOW);
        }
    }
}

/// Route modeless child controls before the folder window's accelerators.
pub fn route(msg: &MSG) -> bool {
    let root = unsafe { GetAncestor(msg.hwnd, GA_ROOT) };
    if !VIEWS.with(|v| v.borrow().contains(&root)) {
        return false;
    }
    unsafe {
        if msg.message == WM_KEYDOWN {
            let shift = GetKeyState(0x10) < 0;
            let ctrl = GetKeyState(0x11) < 0;
            match msg.wParam as u32 {
                0x47 if ctrl => {
                    go_line(root);
                    return true;
                }
                0x43 if ctrl && msg.hwnd == root => {
                    copy_selection(root);
                    return true;
                }
                0x41 if ctrl && msg.hwnd == root => {
                    with(root, |s| {
                        if let Some(p) = &s.pair {
                            let count = if s.active_side == 0 { p.left.line_count() } else { p.right.line_count() };
                            s.text_selection = Some(Selection { side: s.active_side, anchor: (0, 0), end: (count, 0) });
                        }
                    });
                    InvalidateRect(root, null(), 0);
                    return true;
                }
                0x76 => {
                    navigate(root, !shift);
                    return true;
                }
                0x72 => {
                    find(root);
                    return true;
                }
                0x46 if ctrl => {
                    if let Some(edit) = with(root, |s| s.controls[4]) {
                        SetFocus(edit);
                        send(edit, EM_SETSEL, 0, -1);
                    }
                    return true;
                }
                13 if with(root, |s| msg.hwnd == s.controls[4]).unwrap_or(false) => {
                    find(root);
                    return true;
                }
                27 => {
                    if with(root, |s| s.loading).unwrap_or(false) {
                        with(root, |s| {
                            s.progress.cancel.store(true, Relaxed);
                            if let Some(worker) = &s.worker {
                                windows_sys::Win32::System::IO::CancelSynchronousIo(worker.as_raw_handle().cast());
                            }
                        });
                    } else {
                        DestroyWindow(root);
                    }
                    return true;
                }
                0x74 => return true,
                key if msg.hwnd == root => {
                    if shift && matches!(key, 35..=40) {
                        with(root, |s| {
                            let Some(p) = &s.pair else { return };
                            let doc = if s.active_side == 0 { &p.left } else { &p.right };
                            let aligned = s.mapping.at(s.top).map_or(0, |r| r.0);
                            let sources = p.row(aligned).map(|(a, b, _)| [a, b]).unwrap_or([None, None]);
                            let point = (sources[s.active_side].unwrap_or(0), s.column);
                            let mut selection = s.text_selection.unwrap_or(Selection { side: s.active_side, anchor: point, end: point });
                            selection.end.0 = selection.end.0.min(doc.line_count().saturating_sub(1));
                            selection.end.1 = selection.end.1.min(doc.max_columns);
                            match key {
                                37 => selection.end.1 = selection.end.1.saturating_sub(1),
                                39 => selection.end.1 += 1,
                                38 => selection.end.0 = selection.end.0.saturating_sub(1),
                                40 => selection.end.0 = (selection.end.0 + 1).min(doc.line_count().saturating_sub(1)),
                                36 => selection.end.1 = 0,
                                35 => selection.end.1 = u64::MAX,
                                _ => {}
                            }
                            let row = p.row_of(s.active_side == 1, selection.end.0);
                            s.text_selection = Some(selection);
                            if let Some(row) = row {
                                reveal_aligned(s, row);
                            }
                        });
                        request_view(root);
                        return true;
                    }
                    let command = match key {
                        38 => Some((SB_VERT, SB_LINEUP)),
                        40 => Some((SB_VERT, SB_LINEDOWN)),
                        33 => Some((SB_VERT, SB_PAGEUP)),
                        34 => Some((SB_VERT, SB_PAGEDOWN)),
                        37 => Some((SB_HORZ, SB_LINEUP)),
                        39 => Some((SB_HORZ, SB_LINEDOWN)),
                        36 => Some((if ctrl { SB_VERT } else { SB_HORZ }, SB_TOP)),
                        35 => Some((if ctrl { SB_VERT } else { SB_HORZ }, SB_BOTTOM)),
                        _ => None,
                    };
                    if let Some((bar, command)) = command {
                        scroll(root, bar, command as u32);
                        return true;
                    }
                }
                _ => {}
            }
        }
        IsDialogMessageW(root, msg) != 0
    }
}

pub fn refresh_preferences() {
    let windows = VIEWS.with(|v| v.borrow().clone());
    for h in windows {
        if let Some(pal) = crate::app::with(|a| a.pal) {
            with(h, |s| {
                s.pal = pal;
                theme::style_control(s.controls[4], pal.dark, theme::Kind::Cfd);
                update_summary(s);
            });
            theme::set_title_dark(h, pal.dark);
        }
        labels(h);
        layout(h);
    }
}
pub fn close_all() {
    let windows = VIEWS.with(|v| v.borrow().clone());
    for h in windows {
        unsafe {
            DestroyWindow(h);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn hidden_window_renders_latest_resize_and_releases_its_worker() {
        let root = std::env::temp_dir().join(format!("fc-view-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let paths = [root.join("left.rs"), root.join("right.rs")];
        let tail: String = (0..80).map(|i| format!("// unchanged line {i}\n")).collect();
        std::fs::write(&paths[0], format!("fn main() {{\r\n    let x = 1;\r\n}}\r\n{}", tail.replace('\n', "\r\n"))).unwrap();
        std::fs::write(&paths[1], format!("fn main() {{\n    let x = 2;\n}}\n{tail}")).unwrap();
        let input = Input {
            paths: paths.clone().map(Some),
            pal: theme::DARK,
            ignore_crlf: false,
            rules: TextRules::default(),
            encodings: [EncodingChoice::Auto; 2],
        };
        unsafe {
            assert!(windows_sys::Win32::System::Ole::OleInitialize(null()) >= 0);
            struct Ole;
            impl Drop for Ole {
                fn drop(&mut self) {
                    unsafe {
                        windows_sys::Win32::System::Ole::OleUninitialize();
                    }
                }
            }
            let _ole = Ole;
            let instance = GetModuleHandleW(null());
            let class = wide("FerrumCompareTextTest");
            let wc = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(procedure),
                hInstance: instance,
                lpszClassName: class.as_ptr(),
                ..std::mem::zeroed()
            };
            RegisterClassExW(&wc);
            // No WS_VISIBLE, ShowWindow or input injection: this exercises the
            // real window procedure without touching the user's desktop.
            let h = CreateWindowExW(
                0,
                class.as_ptr(),
                wide("test").as_ptr(),
                WS_OVERLAPPEDWINDOW | WS_VSCROLL | WS_HSCROLL,
                0,
                0,
                1180,
                720,
                null_mut(),
                null_mut(),
                instance,
                (&input as *const Input).cast(),
            );
            assert!(!h.is_null());
            struct Window(HWND);
            impl Drop for Window {
                fn drop(&mut self) {
                    unsafe {
                        DestroyWindow(self.0);
                    }
                }
            }
            let window = Window(h);
            assert!(with(h, |s| GetNextDlgTabItem(h, s.controls[2], 0) == s.controls[6]).unwrap());
            assert!(with(h, |s| GetNextDlgTabItem(h, s.controls[6], 0) == s.controls[3]).unwrap());
            let deadline = Instant::now() + Duration::from_secs(3);
            let pump = || {
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            };
            while !with(h, |s| s.frame.is_some()).unwrap_or(false) {
                assert!(Instant::now() < deadline, "viewer worker did not return a frame");
                pump();
                std::thread::sleep(Duration::from_millis(2));
            }
            assert_eq!(with(h, |s| s.pair.as_ref().unwrap().changes()), Some(3));
            assert!(
                with(h, |s| {
                    let row = &s.frame.as_ref().unwrap().lines[0];
                    row.equal && row.ending == ["CRLF", "LF"]
                })
                .unwrap()
            );
            let pair = with(h, |s| Arc::downgrade(s.pair.as_ref().unwrap())).unwrap();
            let bytes = with(h, |s| s.progress.bytes.load(Relaxed)).unwrap();
            send(h, WM_COMMAND, IGNORE_CRLF, 0);
            assert_eq!(with(h, |s| s.pair.as_ref().unwrap().result(s.ignore_crlf).change_rows.len()), Some(1));
            assert!(with(h, |s| Arc::ptr_eq(s.pair.as_ref().unwrap(), &pair.upgrade().unwrap())).unwrap());
            assert_eq!(with(h, |s| s.progress.bytes.load(Relaxed)), Some(bytes));
            send(h, WM_COMMAND, IGNORE_CRLF, 0);
            assert_eq!(with(h, |s| s.pair.as_ref().unwrap().result(s.ignore_crlf).change_rows.len()), Some(3));
            send(h, WM_COMMAND, IGNORE_CRLF, 0);
            for (width, height) in [(900, 500), (1400, 800), (1180, 720)] {
                SetWindowPos(h, null_mut(), 0, 0, width, height, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
            }
            while !with(h, |s| s.frame.as_ref().is_some_and(|f| f.serial == s.epoch.load(Relaxed))).unwrap_or(false) {
                assert!(Instant::now() < deadline, "latest viewport was not rendered");
                pump();
                std::thread::sleep(Duration::from_millis(2));
            }
            InvalidateRect(h, null(), 0);
            send(h, WM_PAINT, 0, 0);
            navigate(h, true);
            assert_eq!(with(h, |s| s.selection), Some(Some(1)));
            let target = with(h, |s| s.drop_target.as_ref().unwrap().test_ref()).unwrap();
            let mut point = POINT { x: client(h).right - 8, y: 120 };
            ClientToScreen(h, &mut point);
            let point = POINTL { x: point.x, y: point.y };
            let reversed = [paths[1].clone(), paths[0].clone()];
            assert_eq!(target.drag(&reversed, point, true), windows_sys::Win32::System::Ole::DROPEFFECT_COPY);
            while !with(h, |s| s.paths == reversed.clone().map(Some) && s.frame.is_some()).unwrap_or(false) {
                assert!(Instant::now() < deadline, "drop did not replace the pair");
                pump();
                std::thread::sleep(Duration::from_millis(2));
            }
            assert_eq!(target.drag(&paths[..1], point, false), windows_sys::Win32::System::Ole::DROPEFFECT_NONE);
            assert!(with(h, |s| s.drop_hint.is_none() && s.paths == reversed.clone().map(Some)).unwrap());
            assert_eq!(
                target.drag(&[paths[0].clone(), paths[1].clone(), root.clone()], point, true),
                windows_sys::Win32::System::Ole::DROPEFFECT_NONE
            );
            assert!(
                crate::cmds::validate_drop(&crate::drop_target::Pending {
                    paths: vec![paths[0].clone(), root.clone()],
                    zone: crate::drop_target::Zone::Both
                })
                .is_err()
            );
            assert_eq!(target.drag(&paths[1..], point, true), windows_sys::Win32::System::Ole::DROPEFFECT_COPY);
            while !with(h, |s| s.paths == [Some(paths[1].clone()), Some(paths[1].clone())] && s.frame.is_some()).unwrap_or(false) {
                assert!(Instant::now() < deadline, "single drop did not replace the right side");
                pump();
                std::thread::sleep(Duration::from_millis(2));
            }
            assert_eq!(with(h, |s| s.pair.as_ref().unwrap().changes()), Some(0));
            assert!(with(h, |s| s.status.contains(t(S::SettingsTextMatch))).unwrap());
            send(h, WM_COMMAND, IGNORE_SPACES, 0);
            while !with(h, |s| s.frame.is_some()).unwrap_or(false) {
                assert!(Instant::now() < deadline, "text rules were not applied");
                pump();
                std::thread::sleep(Duration::from_millis(2));
            }
            assert!(with(h, |s| s.pair.as_ref().unwrap().rules.ignore_spaces && s.detail.contains(t(S::SpacesRule))).unwrap());
            send(h, WM_COMMAND, EXACT_RULES, 0);
            while !with(h, |s| s.frame.is_some()).unwrap_or(false) {
                assert!(Instant::now() < deadline, "exact text rules were not restored");
                pump();
                std::thread::sleep(Duration::from_millis(2));
            }
            assert!(with(h, |s| !s.rules.active() && !s.ignore_crlf && s.status.contains(t(S::ExactTextMatch))).unwrap());
            let unchanged_decoder = with(h, |s| Arc::downgrade(s.pair.as_ref().unwrap())).unwrap();
            encoding_command(h, ENCODING_LEFT + 1);
            assert!(with(h, |s| Arc::ptr_eq(s.pair.as_ref().unwrap(), &unchanged_decoder.upgrade().unwrap())).unwrap());
            send(h, WM_COMMAND, FOLD, 0);
            while !with(h, |s| s.frame.as_ref().is_some_and(|f| Arc::ptr_eq(&f.mapping, &s.mapping))).unwrap_or(false) {
                assert!(Instant::now() < deadline, "folded viewport was not rendered");
                pump();
                std::thread::sleep(Duration::from_millis(2));
            }
            assert!(
                with(h, |s| s.mapping.rows < s.pair.as_ref().unwrap().rows
                    && s.frame.as_ref().unwrap().lines.iter().any(|r| r.folded.is_some()))
                .unwrap()
            );
            with(h, |s| reveal_aligned(s, 40));
            assert!(with(h, |s| s.mapping.at(s.mapping.display_of(40)).unwrap().1.is_none()).unwrap());
            let weak = with(h, |s| Arc::downgrade(&s.mailbox)).unwrap();
            drop(window);
            while weak.upgrade().is_some() {
                assert!(Instant::now() < deadline, "closed viewer retained its worker");
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        for path in paths {
            std::fs::remove_file(path).unwrap();
        }
        std::fs::remove_dir(root).unwrap();
    }
}
