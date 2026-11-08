use crate::{
    lang::{self, S, t},
    settings::Settings,
    theme::Palette,
};
use fc_core::{Comparison, NONE, Progress};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, mpsc};
use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::HFONT;

pub const WM_WORK: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 1;
pub enum Event {
    Compared(Result<Box<Comparison>, std::io::Error>),
    Exported(Result<(), std::io::Error>),
}
#[derive(Clone, Copy)]
pub struct Windows {
    pub main: HWND,
    pub left: HWND,
    pub right: HWND,
    pub browse: [HWND; 2],
    pub compare: HWND,
    pub contents: HWND,
    pub stop: HWND,
    pub swap: HWND,
    pub backend: HWND,
    pub search: HWND,
    pub filter: HWND,
    pub lists: [HWND; 2],
}
#[derive(Clone, Default)]
pub struct Layout {
    pub menus: [RECT; 3],
    pub path_labels: [RECT; 2],
    pub paths: [RECT; 2],
    pub browse: [RECT; 2],
    pub toolbar: [RECT; 5],
    pub search: RECT,
    pub filter: RECT,
    pub lists: [RECT; 2],
    pub split: RECT,
    pub status: RECT,
    pub progress: RECT,
    pub footer: RECT,
}
#[derive(Clone, Copy)]
pub struct Row {
    pub id: u32,
    pub depth: u16,
}
pub struct App {
    pub w: Windows,
    pub settings: Settings,
    pub pal: Palette,
    pub font: HFONT,
    pub dpi: u32,
    pub layout: Layout,
    pub metrics: crate::ui::Metrics,
    pub pair: Option<Arc<Comparison>>,
    pub rows: Vec<Row>,
    pub expanded: HashSet<u32>,
    pub filter: u8,
    pub query: String,
    pub display_scope: Option<fc_core::filter::Scope>,
    pub progress: Option<Arc<Progress>>,
    pub exporting: bool,
    pub status: String,
    pub closing: bool,
    pub dragging: bool,
    pub drop_target: Option<crate::drop_target::Registration>,
    pub drop_hint: Option<crate::drop_target::Feedback>,
    pub icons: HashMap<(String, bool), i32>,
    pub syncing: bool,
    pub summary: fc_core::Summary,
    pub sort: (usize, i32, bool),
    pub tx: mpsc::Sender<Event>,
    pub rx: mpsc::Receiver<Event>,
}
thread_local! { static APP: RefCell<Option<App>> = const { RefCell::new(None) }; }
pub fn install(app: App) {
    APP.with(|a| *a.borrow_mut() = Some(app));
}
pub fn with<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok()?.as_mut().map(f))
}
pub fn take() -> Option<App> {
    APP.with(|a| a.borrow_mut().take())
}

impl App {
    pub fn busy(&self) -> bool {
        self.progress.is_some() || self.exporting
    }
    pub fn scale(&self, n: i32) -> i32 {
        n * self.dpi as i32 / 96
    }
    pub fn rebuild_rows(&mut self) {
        self.rows.clear();
        let Some(pair) = &self.pair else { return };
        let filtered = self.filter != 0 || !self.query.is_empty();
        let mut keep = if filtered { vec![false; pair.items.len()] } else { Vec::new() };
        let mut hidden = if self.display_scope.is_some() { vec![false; pair.items.len()] } else { Vec::new() };
        if filtered {
            for (id, row) in pair.items.iter().enumerate().skip(1) {
                if let Some(scope) = &self.display_scope {
                    hidden[id] = hidden[row.parent as usize] || scope.excludes(pair.name(id as u32));
                    if hidden[id] {
                        continue;
                    }
                }
                let state = row.own;
                let accepted = match self.filter {
                    1 => state.differs() || state.uncertain(),
                    2 => row.verified,
                    3 => state.uncertain(),
                    _ => true,
                };
                if !accepted {
                    continue;
                }
                let matched = if self.query.is_empty() {
                    true
                } else if let Some(scope) = &self.display_scope {
                    scope.allows(pair.name(id as u32), false)
                } else {
                    let path = if row.left != NONE { pair.left.relative(row.left) } else { pair.right.relative(row.right) };
                    path.to_string_lossy().to_lowercase().contains(&self.query)
                };
                keep[id] = matched;
            }
            for id in (1..keep.len()).rev() {
                if keep[id] {
                    keep[pair.items[id].parent as usize] = true;
                }
            }
        }
        let mut stack = vec![(0u32, 0u16)];
        while let Some((id, depth)) = stack.pop() {
            if id != 0 {
                if filtered && !keep[id as usize] {
                    continue;
                }
                self.rows.push(Row { id, depth: depth.saturating_sub(1) });
            }
            if id == 0 || filtered || self.expanded.contains(&id) {
                let mut children = pair.children(id);
                let (side, col, descending) = self.sort;
                let tree = if side == 0 { &pair.left } else { &pair.right };
                let node = |id: u32| {
                    let row = pair.items[id as usize];
                    let id = if side == 0 { row.left } else { row.right };
                    (id != NONE).then(|| tree.nodes[id as usize])
                };
                children.sort_unstable_by(|&a, &b| {
                    let order = match col {
                        1 => {
                            let state = |id: u32| {
                                let state = pair.items[id as usize].state;
                                if state.content_differs() { fc_core::State::Different } else { state }
                            };
                            (state(a) as u8)
                                .cmp(&(state(b) as u8))
                                .then_with(|| fc_core::name_order(pair.name(a), pair.name(b), true))
                                .then_with(|| pair.name(a).cmp(pair.name(b)))
                        }
                        2 => node(a).map(|n| n.size).cmp(&node(b).map(|n| n.size)),
                        3 => node(a).map(|n| n.mtime).cmp(&node(b).map(|n| n.mtime)),
                        _ => fc_core::name_order(pair.name(a), pair.name(b), true).then_with(|| pair.name(a).cmp(pair.name(b))),
                    };
                    pair.is_dir(b).cmp(&pair.is_dir(a)).then(if descending { order.reverse() } else { order })
                });
                stack.extend(children.into_iter().rev().map(|id| (id, depth.saturating_add(1))));
            }
        }
    }

    pub fn cell(&self, row: usize, side: usize, col: i32) -> String {
        let Some(pair) = &self.pair else { return String::new() };
        let Some(row) = self.rows.get(row) else { return String::new() };
        let diff = pair.items[row.id as usize];
        let (tree, node) = if side == 0 { (&pair.left, diff.left) } else { (&pair.right, diff.right) };
        if col == 1 {
            return lang::state(diff.state).into();
        }
        if node == NONE {
            return String::new();
        }
        let n = tree.nodes[node as usize];
        match col {
            0 => String::from_utf16_lossy(tree.name(node)),
            2 if !n.dir() => lang::size(n.size),
            3 => crate::util::filetime(n.mtime),
            _ => String::new(),
        }
    }

    pub fn footer(&self) -> String {
        if self.pair.is_none() {
            return "FerrumCompare 0.1.0".into();
        }
        let s = &self.summary;
        let mut text = format!(
            "{}: {}    {}: {}    {}: {}    {}: {}    {}: {}    {}: {}    {}: {}",
            t(S::Files),
            lang::number(s.files),
            t(S::Dirs),
            lang::number(s.dirs),
            t(S::Differences),
            lang::number(s.different),
            t(S::Verified),
            lang::number(s.verified),
            t(S::Unverified),
            lang::number(s.unverified),
            t(S::Unknown),
            lang::number(s.uncertain),
            t(S::Visible),
            lang::number(self.rows.len() as u64)
        );
        if self.pair.as_ref().is_some_and(|p| p.scope.active()) {
            text.push_str(&format!("    {}", t(S::AnalysisLimited)));
        }
        text
    }
}
