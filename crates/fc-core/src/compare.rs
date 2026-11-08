use crate::scan::{Case, NONE, Node, Progress, Tree, ordinal};
use std::cmp::Ordering;
use std::io;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum State {
    Directory,
    LeftOnly,
    RightOnly,
    Type,
    Size,
    Time,
    Name,
    Metadata,
    Equal,
    Different,
    Link,
    Error,
    Changed,
    Cloud,
    Children,
}

impl State {
    pub const COUNT: usize = 15;
    pub fn code(self) -> &'static str {
        match self {
            Self::Directory => "directory",
            Self::LeftOnly => "left_only",
            Self::RightOnly => "right_only",
            Self::Type => "type_differs",
            Self::Size => "size_differs",
            Self::Time => "time_differs",
            Self::Name => "name_differs",
            Self::Metadata => "size_time_match",
            Self::Equal => "content_equal",
            Self::Different => "content_differs",
            Self::Link => "link_unchecked",
            Self::Error => "unreadable",
            Self::Changed => "changed_during_comparison",
            Self::Cloud => "cloud_not_read",
            Self::Children => "children_differ",
        }
    }
    pub fn uncertain(self) -> bool {
        matches!(self, Self::Error | Self::Changed | Self::Cloud | Self::Link)
    }
    /// Unequal lengths prove unequal contents without reading the file data.
    pub fn content_differs(self) -> bool {
        matches!(self, Self::Size | Self::Different)
    }
    pub fn differs(self) -> bool {
        matches!(
            self,
            Self::LeftOnly | Self::RightOnly | Self::Type | Self::Size | Self::Time | Self::Name | Self::Different | Self::Children
        )
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Difference {
    pub left: u32,
    pub right: u32,
    pub parent: u32,
    pub first: u32,
    pub next: u32,
    pub state: State,
    pub own: State,
    /// Native errors belong to this row; unreadable ancestors are carried separately.
    pub error: u32,
    pub incomplete: u8,
    pub verified: bool,
}

pub struct Comparison {
    pub left: Tree,
    pub right: Tree,
    pub items: Vec<Difference>,
    pub elapsed: Duration,
    pub contents: bool,
    pub scope: crate::filter::Scope,
}

#[derive(Default, Debug)]
pub struct Summary {
    pub files: u64,
    pub dirs: u64,
    pub states: [u64; State::COUNT],
    pub different: u64,
    pub uncertain: u64,
    pub verified: u64,
    pub unverified: u64,
}

impl Comparison {
    pub fn name(&self, id: u32) -> &[u16] {
        let row = self.items[id as usize];
        if row.left != NONE { self.left.name(row.left) } else { self.right.name(row.right) }
    }
    pub fn is_dir(&self, id: u32) -> bool {
        let row = self.items[id as usize];
        row.left != NONE && self.left.nodes[row.left as usize].dir() || row.right != NONE && self.right.nodes[row.right as usize].dir()
    }
    pub fn children(&self, id: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut c = self.items[id as usize].first;
        while c != NONE {
            out.push(c);
            c = self.items[c as usize].next;
        }
        out
    }
    pub fn summary(&self) -> Summary {
        let mut summary = Summary::default();
        for id in 1..self.items.len() as u32 {
            let row = self.items[id as usize];
            if self.is_dir(id) {
                summary.dirs += 1;
            } else {
                summary.files += 1;
            }
            summary.states[row.own as usize] += 1;
            summary.different += row.own.differs() as u64;
            summary.uncertain += row.own.uncertain() as u64;
            summary.verified += row.verified as u64;
            summary.unverified += (row.own == State::Metadata) as u64;
        }
        summary.uncertain += self.items[0].own.uncertain() as u64;
        summary
    }

    fn add(&mut self, parent: u32, left: u32, right: u32) -> io::Result<u32> {
        if self.items.len() >= NONE as usize {
            return Err(io::Error::from(io::ErrorKind::OutOfMemory));
        }
        let ln = (left != NONE).then(|| self.left.nodes[left as usize]);
        let rn = (right != NONE).then(|| self.right.nodes[right as usize]);
        let mut incomplete = if parent == NONE { 0 } else { self.items[parent as usize].incomplete };
        if ln.is_some_and(|n| n.error != 0) {
            incomplete |= 1;
        }
        if rn.is_some_and(|n| n.error != 0) {
            incomplete |= 2;
        }
        let mut state = classify(ln, rn);
        if left == NONE && incomplete & 1 != 0 || right == NONE && incomplete & 2 != 0 {
            state = State::Error;
        }
        if state == State::Metadata && self.left.name(left) != self.right.name(right) {
            state = State::Name;
        }
        let id = self.items.len() as u32;
        let next = if parent == NONE { NONE } else { self.items[parent as usize].first };
        self.items.push(Difference {
            left,
            right,
            parent,
            first: NONE,
            next,
            state,
            own: state,
            error: ln.map_or(0, |n| n.error).max(rn.map_or(0, |n| n.error)),
            incomplete,
            verified: false,
        });
        if parent != NONE {
            self.items[parent as usize].first = id;
        }
        Ok(id)
    }

    pub(crate) fn build(left: Tree, right: Tree, case: Case, progress: &Progress) -> io::Result<Self> {
        let mut out = Self { left, right, items: Vec::new(), elapsed: Duration::ZERO, contents: false, scope: Default::default() };
        out.add(NONE, 0, 0)?;
        let mut at = 0;
        while at < out.items.len() {
            progress.check()?;
            let row = out.items[at];
            let enter = |tree: &Tree, id| id != NONE && tree.nodes[id as usize].dir() && !tree.nodes[id as usize].link();
            let mut l = if enter(&out.left, row.left) { out.left.children(row.left) } else { Vec::new() };
            let mut r = if enter(&out.right, row.right) { out.right.children(row.right) } else { Vec::new() };
            let sensitive = case == Case::Sensitive
                || case == Case::Auto
                    && ((row.left != NONE && out.left.nodes[row.left as usize].case_sensitive())
                        || (row.right != NONE && out.right.nodes[row.right as usize].case_sensitive()));
            let cmp = |tree: &Tree, a, b| ordinal(tree.name(a), tree.name(b), !sensitive).then_with(|| tree.name(a).cmp(tree.name(b)));
            l.sort_unstable_by(|&a, &b| cmp(&out.left, a, b));
            r.sort_unstable_by(|&a, &b| cmp(&out.right, a, b));
            let (mut i, mut j) = (0, 0);
            while i < l.len() || j < r.len() {
                let order = if i == l.len() {
                    Ordering::Greater
                } else if j == r.len() {
                    Ordering::Less
                } else {
                    ordinal(out.left.name(l[i]), out.right.name(r[j]), !sensitive)
                };
                match order {
                    Ordering::Less => {
                        out.add(at as u32, l[i], NONE)?;
                        i += 1;
                    }
                    Ordering::Greater => {
                        out.add(at as u32, NONE, r[j])?;
                        j += 1;
                    }
                    Ordering::Equal => {
                        let (si, sj) = (i, j);
                        while i < l.len() && ordinal(out.left.name(l[si]), out.left.name(l[i]), !sensitive).is_eq() {
                            i += 1;
                        }
                        while j < r.len() && ordinal(out.right.name(r[sj]), out.right.name(r[j]), !sensitive).is_eq() {
                            j += 1;
                        }
                        if i - si == 1 && j - sj == 1 {
                            out.add(at as u32, l[si], r[sj])?;
                            continue;
                        }
                        // Match exact spellings first when a case-folded key is ambiguous.
                        let mut unused: Vec<u32> = r[sj..j].to_vec();
                        let mut unmatched = Vec::new();
                        for &a in &l[si..i] {
                            if let Some(pos) = unused.iter().position(|&b| out.left.name(a) == out.right.name(b)) {
                                let b = unused.remove(pos);
                                out.add(at as u32, a, b)?;
                            } else {
                                unmatched.push(a);
                            }
                        }
                        if unmatched.len() == 1 && unused.len() == 1 {
                            out.add(at as u32, unmatched[0], unused[0])?;
                        } else {
                            for a in unmatched {
                                out.add(at as u32, a, NONE)?;
                            }
                            for b in unused {
                                out.add(at as u32, NONE, b)?;
                            }
                        }
                    }
                }
            }
            at += 1;
        }
        Ok(out)
    }

    pub(crate) fn aggregate(&mut self) {
        for row in &mut self.items {
            row.state = row.own;
        }
        for i in (1..self.items.len()).rev() {
            let row = self.items[i];
            let parent = &mut self.items[row.parent as usize];
            if row.state.uncertain() && matches!(parent.state, State::Directory | State::Children | State::Equal) {
                parent.state = State::Error;
            } else if row.state.differs() && parent.state == State::Directory {
                parent.state = State::Children;
            }
        }
    }
}

fn classify(left: Option<Node>, right: Option<Node>) -> State {
    if left.is_some_and(|n| n.error != 0) || right.is_some_and(|n| n.error != 0) {
        return State::Error;
    }
    let (l, r) = match (left, right) {
        (None, Some(_)) => return State::RightOnly,
        (Some(_), None) => return State::LeftOnly,
        (Some(l), Some(r)) => (l, r),
        _ => return State::Error,
    };
    if l.error != 0 || r.error != 0 {
        return State::Error;
    }
    if l.dir() != r.dir() || l.link() != r.link() {
        return State::Type;
    }
    if l.link() {
        return State::Link;
    }
    if l.dir() {
        return State::Directory;
    }
    if l.size != r.size {
        State::Size
    } else if l.mtime != r.mtime {
        State::Time
    } else {
        State::Metadata
    }
}
