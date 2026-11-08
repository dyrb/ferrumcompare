use std::cmp::Ordering;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::{
    ffi::{OsStrExt, OsStringExt},
    fs::OpenOptionsExt,
    io::AsRawHandle,
};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering::Relaxed},
};

use windows_sys::Win32::Foundation::{ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Globalization::{CSTR_GREATER_THAN, CSTR_LESS_THAN, CompareStringOrdinal};
use windows_sys::Win32::Storage::FileSystem::*;

pub const NONE: u32 = u32::MAX;
const CASE_SENSITIVE: u32 = 1 << 31;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Backend {
    #[default]
    Auto,
    Walk,
    Mft,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Case {
    #[default]
    Auto,
    Sensitive,
    Insensitive,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub contents: bool,
    pub threads: usize,
    pub backend: Backend,
    pub case: Case,
    pub skip_cloud: bool,
    pub scope: crate::filter::Scope,
}

impl Default for Options {
    fn default() -> Self {
        Self { contents: false, threads: 2, backend: Backend::Auto, case: Case::Auto, skip_cloud: true, scope: Default::default() }
    }
}

#[derive(Default)]
pub struct Progress {
    pub cancel: Arc<AtomicBool>,
    pub phase: AtomicU8,
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    pub total: AtomicU64,
    pub done: AtomicU64,
    pub bytes: AtomicU64,
}

impl Progress {
    pub fn check(&self) -> io::Result<()> {
        if self.cancel.load(Relaxed) { Err(io::Error::from(io::ErrorKind::Interrupted)) } else { Ok(()) }
    }
}

/// Names retain native UTF-16; timestamps retain all FILETIME bits.
#[derive(Clone, Copy, Debug)]
pub struct Node {
    pub parent: u32,
    pub first: u32,
    pub next: u32,
    off: u32,
    len: u32,
    pub attrs: u32,
    pub error: u32,
    pub size: u64,
    pub mtime: u64,
}

impl Node {
    pub fn dir(&self) -> bool {
        self.attrs & FILE_ATTRIBUTE_DIRECTORY != 0
    }
    pub fn link(&self) -> bool {
        self.attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    pub fn case_sensitive(&self) -> bool {
        self.attrs & CASE_SENSITIVE != 0
    }
}

pub struct Tree {
    pub root: PathBuf,
    pub nodes: Vec<Node>,
    names: Vec<u16>,
    pub backend: Backend,
}

pub fn ordinal(a: &[u16], b: &[u16], ignore_case: bool) -> Ordering {
    if !ignore_case {
        return a.cmp(b);
    }
    // Lengths are bounded by Windows path components, not entire paths.
    match unsafe { CompareStringOrdinal(a.as_ptr(), a.len() as i32, b.as_ptr(), b.len() as i32, 1) } {
        CSTR_LESS_THAN => Ordering::Less,
        CSTR_GREATER_THAN => Ordering::Greater,
        2 => Ordering::Equal,
        _ => a.cmp(b),
    }
}

impl Tree {
    pub fn name(&self, id: u32) -> &[u16] {
        let n = self.nodes[id as usize];
        &self.names[n.off as usize..(n.off + n.len) as usize]
    }

    pub fn relative(&self, mut id: u32) -> PathBuf {
        let mut chain = Vec::new();
        while id != 0 && id != NONE {
            chain.push(id);
            id = self.nodes[id as usize].parent;
        }
        let mut path = PathBuf::new();
        for id in chain.into_iter().rev() {
            path.push(OsString::from_wide(self.name(id)));
        }
        path
    }

    pub fn path(&self, id: u32) -> PathBuf {
        self.root.join(self.relative(id))
    }

    pub fn children(&self, id: u32) -> Vec<u32> {
        if id == NONE {
            return Vec::new();
        }
        let mut children = Vec::new();
        let mut c = self.nodes[id as usize].first;
        while c != NONE {
            children.push(c);
            c = self.nodes[c as usize].next;
        }
        children
    }

    fn new(root: PathBuf, backend: Backend) -> io::Result<Self> {
        let mut tree = Self { root, nodes: Vec::new(), names: Vec::new(), backend };
        let (_, info) = open_info(&tree.root, true)?;
        if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
            return Err(io::Error::from(io::ErrorKind::NotADirectory));
        }
        tree.add(NONE, &[], info.dwFileAttributes, 0, filetime(info.ftLastWriteTime))?;
        Ok(tree)
    }

    fn add(&mut self, parent: u32, name: &[u16], attrs: u32, size: u64, mtime: u64) -> io::Result<u32> {
        if self.nodes.len() >= NONE as usize || self.names.len().saturating_add(name.len()) > u32::MAX as usize {
            return Err(io::Error::new(io::ErrorKind::OutOfMemory, "directory index limit exceeded"));
        }
        let id = self.nodes.len() as u32;
        let next = if parent == NONE { NONE } else { self.nodes[parent as usize].first };
        self.nodes.push(Node {
            parent,
            first: NONE,
            next,
            off: self.names.len() as u32,
            len: name.len() as u32,
            attrs,
            error: 0,
            size,
            mtime,
        });
        self.names.extend_from_slice(name);
        if parent != NONE {
            self.nodes[parent as usize].first = id;
        }
        Ok(id)
    }

    pub fn errors(&self) -> usize {
        self.nodes.iter().filter(|n| n.error != 0).count()
    }
}

pub(crate) fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain([0]).collect()
}

pub(crate) fn filetime(t: windows_sys::Win32::Foundation::FILETIME) -> u64 {
    (t.dwHighDateTime as u64) << 32 | t.dwLowDateTime as u64
}

pub(crate) fn open_info(path: &Path, share_write: bool) -> io::Result<(File, BY_HANDLE_FILE_INFORMATION)> {
    let file = OpenOptions::new()
        .read(true)
        .access_mode(if share_write { FILE_READ_ATTRIBUTES } else { windows_sys::Win32::Foundation::GENERIC_READ })
        .share_mode(if share_write { 7 } else { FILE_SHARE_READ })
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_SEQUENTIAL_SCAN)
        .open(path)?;
    let mut info = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((file, info))
}

fn case_flag(path: &Path) -> bool {
    let Ok((file, _)) = open_info(path, true) else { return true };
    let mut flags = 0u32;
    let ok = unsafe { GetFileInformationByHandleEx(file.as_raw_handle(), FileCaseSensitiveInfo, (&mut flags as *mut u32).cast(), 4) };
    // A query failure must not collapse distinct names into one comparison row.
    ok == 0 || flags & 1 != 0
}

fn count(progress: &Progress, dir: bool) {
    if dir {
        progress.dirs.fetch_add(1, Relaxed);
    } else {
        progress.files.fetch_add(1, Relaxed);
    }
}

struct Find(windows_sys::Win32::Foundation::HANDLE);
impl Drop for Find {
    fn drop(&mut self) {
        unsafe {
            FindClose(self.0);
        }
    }
}

fn walk(root: PathBuf, scope: &crate::filter::Scope, progress: &Progress) -> io::Result<Tree> {
    let mut tree = Tree::new(root, Backend::Walk)?;
    let mut stack = vec![0u32];
    while let Some(parent) = stack.pop() {
        progress.check()?;
        let path = tree.path(parent);
        if case_flag(&path) {
            tree.nodes[parent as usize].attrs |= CASE_SENSITIVE;
        }
        let query = wide(&path.join("*"));
        let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
        let h = unsafe {
            FindFirstFileExW(
                query.as_ptr(),
                FindExInfoBasic,
                (&mut data as *mut WIN32_FIND_DATAW).cast(),
                FindExSearchNameMatch,
                std::ptr::null(),
                FIND_FIRST_EX_LARGE_FETCH,
            )
        };
        if h == INVALID_HANDLE_VALUE {
            let e = io::Error::last_os_error().raw_os_error().unwrap_or(1) as u32;
            if e != 2 {
                tree.nodes[parent as usize].error = e;
            }
            continue;
        }
        let _find = Find(h);
        loop {
            progress.check()?;
            let len = data.cFileName.iter().position(|&c| c == 0).unwrap_or(data.cFileName.len());
            let name = &data.cFileName[..len];
            if name != [46] && name != [46, 46] && scope.allows(name, data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) {
                let id = tree.add(
                    parent,
                    name,
                    data.dwFileAttributes,
                    (data.nFileSizeHigh as u64) << 32 | data.nFileSizeLow as u64,
                    filetime(data.ftLastWriteTime),
                )?;
                let node = tree.nodes[id as usize];
                count(progress, node.dir());
                if node.dir() && !node.link() {
                    stack.push(id);
                }
            }
            if unsafe { FindNextFileW(h, &mut data) } == 0 {
                let e = io::Error::last_os_error().raw_os_error().unwrap_or(1) as u32;
                if e != ERROR_NO_MORE_FILES {
                    tree.nodes[parent as usize].error = e;
                }
                break;
            }
        }
    }
    Ok(tree)
}

fn drive(path: &Path) -> Option<char> {
    let w: Vec<u16> = path.as_os_str().encode_wide().collect();
    let w = if w.starts_with(&[92, 92, 63, 92]) { &w[4..] } else { &w[..] };
    (w.len() >= 3 && w[1] == 58 && (w[0] as u8).is_ascii_alphabetic()).then(|| (w[0] as u8 as char).to_ascii_uppercase())
}

fn is_root(path: &Path) -> bool {
    path.parent().is_none()
}

fn mft_scan(letter: char, options: &Options, progress: &Progress) -> io::Result<ft_ntfs::scan::Scan> {
    let volume = ft_ntfs::volume::Volume::open(&format!("{letter}:"), ft_ntfs::volume::IoMode::Direct)
        .map_err(|e| io::Error::other(e.to_string()))?;
    let scan = ft_ntfs::scan::scan(
        &volume,
        &ft_ntfs::scan::ScanOptions { threads: options.threads.max(1), cancel: Some(progress.cancel.clone()), ..Default::default() },
    )
    .map_err(|e| io::Error::other(e.to_string()))?;
    progress.check()?;
    if scan.stats.parse.corrupt != 0 || scan.stats.ext_dropped != 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "incomplete MFT snapshot"));
    }
    Ok(scan)
}

fn from_mft(
    root: PathBuf,
    scan: &ft_ntfs::scan::Scan,
    first: &[u32],
    next: &[u32],
    scope: &crate::filter::Scope,
    progress: &Progress,
) -> io::Result<Tree> {
    let (_, info) = open_info(&root, true)?;
    let record = (((info.nFileIndexHigh as u64) << 32 | info.nFileIndexLow as u64) & 0x0000_ffff_ffff_ffff) as usize;
    let rec = scan.recs.get(record).ok_or_else(|| io::Error::other("directory changed during MFT scan"))?;
    if rec.seq != (info.nFileIndexHigh >> 16) as u16 || rec.flags & ft_ntfs::rflags::DIR == 0 {
        return Err(io::Error::other("directory changed during MFT scan"));
    }
    let mut tree = Tree::new(root, Backend::Mft)?;
    let mut stack = vec![(0u32, record as u32)];
    let mut seen = std::collections::HashSet::new();
    while let Some((parent, record)) = stack.pop() {
        progress.check()?;
        if !seen.insert(record) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "directory cycle in MFT"));
        }
        if case_flag(&tree.path(parent)) {
            tree.nodes[parent as usize].attrs |= CASE_SENSITIVE;
        }
        let mut at = first[record as usize];
        while at != NONE {
            let entry = scan.names[at as usize];
            at = next[at as usize];
            let Some(rec) = scan.recs.get(entry.record as usize) else { continue };
            if rec.flags & ft_ntfs::rflags::IN_USE == 0 {
                continue;
            }
            if entry.parent_seq != scan.recs[record as usize].seq {
                return Err(io::Error::other("stale parent in MFT"));
            }
            let name = scan.name(&entry);
            if name == "." || name == ".." {
                continue;
            }
            if name.contains('\u{fffd}') {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "name requires native directory enumeration"));
            }
            let attrs = rec.attrs
                | if rec.flags & ft_ntfs::rflags::DIR != 0 { FILE_ATTRIBUTE_DIRECTORY } else { 0 }
                | if rec.flags & ft_ntfs::rflags::REPARSE != 0 { FILE_ATTRIBUTE_REPARSE_POINT } else { 0 };
            let name: Vec<u16> = name.encode_utf16().collect();
            if !scope.allows(&name, attrs & FILE_ATTRIBUTE_DIRECTORY != 0) {
                continue;
            }
            let id = tree.add(parent, &name, attrs, rec.size, rec.mtime)?;
            let node = tree.nodes[id as usize];
            count(progress, node.dir());
            if node.dir() && !node.link() {
                stack.push((id, entry.record));
            }
        }
    }
    Ok(tree)
}

pub(crate) fn pair(left: &Path, right: &Path, options: &Options, progress: &Progress) -> io::Result<(Tree, Tree)> {
    let left = std::fs::canonicalize(left)?;
    let right = std::fs::canonicalize(right)?;
    let use_mft = options.backend == Backend::Mft || options.backend == Backend::Auto && is_root(&left) && is_root(&right);
    if use_mft && let (Some(a), Some(b)) = (drive(&left), drive(&right)) {
        let attempt = (|| {
            let scan = mft_scan(a, options, progress)?;
            let graph = |scan: &ft_ntfs::scan::Scan| {
                let mut first = vec![NONE; scan.recs.len()];
                let mut next = vec![NONE; scan.names.len()];
                for (i, name) in scan.names.iter().enumerate() {
                    if let Some(slot) = first.get_mut(name.parent as usize) {
                        next[i] = *slot;
                        *slot = i as u32;
                    }
                }
                (first, next)
            };
            let (first, next) = graph(&scan);
            let l = from_mft(left.clone(), &scan, &first, &next, &options.scope, progress)?;
            let r = if a == b {
                from_mft(right.clone(), &scan, &first, &next, &options.scope, progress)?
            } else {
                drop(first);
                drop(next);
                drop(scan);
                let scan = mft_scan(b, options, progress)?;
                let (first, next) = graph(&scan);
                from_mft(right.clone(), &scan, &first, &next, &options.scope, progress)?
            };
            Ok::<_, io::Error>((l, r))
        })();
        if attempt.is_ok() || progress.cancel.load(Relaxed) {
            return attempt;
        }
        progress.files.store(0, Relaxed);
        progress.dirs.store(0, Relaxed);
    }
    std::thread::scope(|s| {
        let l = s.spawn(|| walk(left, &options.scope, progress));
        let r = walk(right, &options.scope, progress);
        let l = l.join().map_err(|_| io::Error::other("directory worker failed"))??;
        Ok((l, r?))
    })
}
