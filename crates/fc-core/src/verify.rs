use crate::scan::{NONE, Options, Progress, filetime, open_info};
use crate::{Comparison, State};
use std::fs::File;
use std::io::{self, Read};
use std::os::windows::io::AsRawHandle;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering::Relaxed},
};
use windows_sys::Win32::Storage::FileSystem::*;

const BUFFER: usize = 1 << 20;
const CLOUD: u32 = FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS;

fn reparse(file: &File) -> io::Result<Vec<u8>> {
    let mut data = vec![0u8; 16 << 10];
    let mut read = 0;
    let ok = unsafe {
        windows_sys::Win32::System::IO::DeviceIoControl(
            file.as_raw_handle(),
            windows_sys::Win32::System::Ioctl::FSCTL_GET_REPARSE_POINT,
            std::ptr::null(),
            0,
            data.as_mut_ptr().cast(),
            data.len() as u32,
            &mut read,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    data.truncate(read as usize);
    Ok(data)
}

fn current(file: &File) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    let mut info = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 { Err(io::Error::last_os_error()) } else { Ok(info) }
}

fn stamp(info: &BY_HANDLE_FILE_INFORMATION) -> (u64, u64, u32, u32, u32, u32) {
    (
        (info.nFileSizeHigh as u64) << 32 | info.nFileSizeLow as u64,
        filetime(info.ftLastWriteTime),
        info.dwVolumeSerialNumber,
        info.nFileIndexHigh,
        info.nFileIndexLow,
        info.dwFileAttributes,
    )
}

fn volume(file: &File) -> Option<Vec<u16>> {
    let mut path = vec![0u16; 32768];
    let len = unsafe { GetFinalPathNameByHandleW(file.as_raw_handle(), path.as_mut_ptr(), path.len() as u32, VOLUME_NAME_GUID) } as usize;
    if len == 0 || len >= path.len() {
        return None;
    }
    let end = path[..len].iter().position(|&c| c == b'}' as u16)?;
    path.truncate(end + 1);
    Some(path)
}

fn read_chunk(file: &mut File, buffer: &mut [u8], progress: &Progress) -> io::Result<usize> {
    let mut done = 0;
    while done < buffer.len() {
        progress.check()?;
        match file.read(&mut buffer[done..]) {
            Ok(0) => break,
            Ok(n) => {
                done += n;
                progress.bytes.fetch_add(n as u64, Relaxed);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(done)
}

fn check(pair: &Comparison, id: u32, options: &Options, progress: &Progress, a: &mut [u8], b: &mut [u8]) -> io::Result<State> {
    let row = pair.items[id as usize];
    let ln = pair.left.nodes[row.left as usize];
    let rn = pair.right.nodes[row.right as usize];
    if options.skip_cloud && (ln.attrs | rn.attrs) & CLOUD != 0 {
        return Ok(State::Cloud);
    }
    let left = pair.left.path(row.left);
    let right = pair.right.path(row.right);
    if ln.link() {
        let (lf, li) = open_info(&left, false)?;
        let (rf, ri) = open_info(&right, false)?;
        if li.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0
            || ri.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0
            || filetime(li.ftLastWriteTime) != ln.mtime
            || filetime(ri.ftLastWriteTime) != rn.mtime
        {
            return Ok(State::Changed);
        }
        let equal = reparse(&lf)? == reparse(&rf)?;
        if stamp(&li) != stamp(&current(&lf)?) || stamp(&ri) != stamp(&current(&rf)?) {
            return Ok(State::Changed);
        }
        return Ok(if equal { State::Equal } else { State::Different });
    }
    let (mut lf, li) = open_info(&left, false)?;
    let (mut rf, ri) = open_info(&right, false)?;
    let ls = stamp(&li);
    let rs = stamp(&ri);
    if ls.0 != ln.size
        || rs.0 != rn.size
        || ls.1 != ln.mtime
        || rs.1 != rn.mtime
        || (ls.5 | rs.5) & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0
    {
        return Ok(State::Changed);
    }
    if ls.0 != rs.0 {
        return Ok(State::Size);
    }
    let mut equal = true;
    // Volume serial numbers alone are not globally unique. Identity shortcuts need
    // a nonzero file identifier and the actual volume GUID, otherwise compare bytes.
    let identical_id = (ls.3 != 0 || ls.4 != 0) && (ls.2, ls.3, ls.4) == (rs.2, rs.3, rs.4);
    let identical_file = identical_id && volume(&lf).is_some_and(|v| volume(&rf).as_ref() == Some(&v));
    if !identical_file {
        loop {
            let l = read_chunk(&mut lf, a, progress)?;
            let r = read_chunk(&mut rf, b, progress)?;
            if a[..l] != b[..r] {
                equal = false;
                break;
            }
            if l < a.len() {
                break;
            }
        }
    }
    if stamp(&current(&lf)?) != ls || stamp(&current(&rf)?) != rs {
        return Ok(State::Changed);
    }
    Ok(if equal { State::Equal } else { State::Different })
}

pub(crate) fn run(pair: &mut Comparison, options: &Options, progress: &Progress) -> io::Result<()> {
    let jobs: Vec<u32> = pair
        .items
        .iter()
        .enumerate()
        .filter_map(|(i, row)| {
            (row.left != NONE
                && row.right != NONE
                && (row.own == State::Link || options.contents && matches!(row.own, State::Metadata | State::Time | State::Name)))
            .then_some(i as u32)
        })
        .collect();
    pair.contents = options.contents;
    progress.total.store(jobs.len() as u64, Relaxed);
    if jobs.is_empty() {
        return Ok(());
    }
    progress.phase.store(3, Relaxed);
    let next = AtomicUsize::new(0);
    let results = Mutex::new(Vec::with_capacity(jobs.len()));
    std::thread::scope(|scope| {
        let view = &*pair;
        for _ in 0..options.threads.clamp(1, 32).min(jobs.len()) {
            let (jobs, next, results) = (&jobs, &next, &results);
            scope.spawn(move || {
                let mut a = vec![0u8; BUFFER];
                let mut b = vec![0u8; BUFFER];
                let mut local = Vec::with_capacity(256);
                loop {
                    if progress.check().is_err() {
                        break;
                    }
                    let n = next.fetch_add(1, Relaxed);
                    let Some(&id) = jobs.get(n) else { break };
                    let (state, error) = match check(view, id, options, progress, &mut a, &mut b) {
                        Ok(state) => (state, 0),
                        Err(e) => (State::Error, e.raw_os_error().unwrap_or(1) as u32),
                    };
                    local.push((id, state, error));
                    if local.len() == 256 {
                        results.lock().unwrap_or_else(|e| e.into_inner()).extend(local.drain(..));
                    }
                    progress.done.fetch_add(1, Relaxed);
                }
                results.lock().unwrap_or_else(|e| e.into_inner()).extend(local);
            });
        }
    });
    progress.check()?;
    for (id, state, error) in results.into_inner().unwrap_or_else(|e| e.into_inner()) {
        let row = &mut pair.items[id as usize];
        row.verified = state == State::Equal;
        row.own =
            if state == State::Equal && row.left != NONE && row.right != NONE && pair.left.name(row.left) != pair.right.name(row.right) {
                State::Name
            } else {
                state
            };
        row.error = error;
    }
    Ok(())
}
