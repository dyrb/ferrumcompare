use fc_core::{Backend, Case, Comparison, NONE, Options, Progress, State};
use std::ffi::OsString;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, atomic::Ordering::Relaxed};

static PROGRESS: OnceLock<Arc<Progress>> = OnceLock::new();
unsafe extern "system" fn cancel(_: u32) -> i32 {
    if let Some(progress) = PROGRESS.get() {
        progress.cancel.store(true, Relaxed);
    }
    1
}

fn help() {
    println!(
        "FerrumCompare 0.1.0\n\nUsage: ferrumcompare LEFT RIGHT [OPTIONS]\n\n  --contents             Compare file bytes, not just size and last-write time\n  --backend auto|walk|mft Select enumeration; MFT falls back to walking if unavailable\n  --case auto|sensitive|insensitive\n  --threads N            Content workers (1..32, default 2)\n  --read-cloud           Allow reading offline/cloud placeholders\n  --include MASKS        Analyze file name masks, e.g. *.cpp;*.hpp\n  --exclude MASKS        Skip matching names/subtrees, e.g. .git;build;target\n  --show-masks MASKS     Filter the console listing only (analysis unchanged)\n  --changes              Print only differences and uncertain results\n  --json                 Write a JSON report to stdout\n  --output FILE          Save a new CSV report (JSON with --json) outside both roots\n  --help                 Show help\n\nExit codes: 0 no differences in the chosen comparison, 1 differences, 2 error or uncertain results, 130 cancelled.\nReparse points are compared without following them. Content checking covers unnamed file data; ACLs and alternate streams are outside its scope."
    );
    println!(
        "\nTwo regular files select text comparison automatically.\n  --text                 Compare text files by lines (UTF-8, BOM UTF-16)\n  --ignore-crlf          Ignore CRLF/LF differences in text comparison\n  --ignore-spaces        Ignore ASCII spaces in text\n  --ignore-tabs          Ignore tabs in text\n  --ignore-blank-lines   Ignore lines containing only ASCII spaces/tabs\n  --ignore-case          Compare Unicode lowercase mappings (no locale or normalization)\n  --ignore-line-endings  Ignore CR/LF/CRLF styles; final newline still matters\n  --encoding-left NAME   auto|utf8|utf16le|utf16be|windows1251\n  --encoding-right NAME  Choose the right text decoder independently\n  --inline               Include changed character ranges in text output\n  --context N            Unchanged context lines (default 3)\n  --text-memory N        Text index budget in MiB (default 192)\nText output is a unified line diff; encoding and line endings are reported separately.\nAlignment has a bounded work budget; large unrelated regions may be shown as a whole."
    );
}

fn text(pair: &Comparison, changes: bool, display: Option<&fc_core::filter::Masks>) -> io::Result<()> {
    let mut out = io::BufWriter::new(io::stdout().lock());
    for tree in [&pair.left, &pair.right] {
        if tree.nodes[0].error != 0 {
            writeln!(out, "{}: {}", tree.root.display(), io::Error::from_raw_os_error(tree.nodes[0].error as i32))?;
        }
    }
    for (id, row) in pair.items.iter().enumerate().skip(1) {
        if !pair.is_dir(id as u32) && display.is_some_and(|m| !m.matches(pair.name(id as u32))) {
            continue;
        }
        if changes && !row.own.differs() && !row.own.uncertain() {
            continue;
        }
        let path = if row.left != NONE { pair.left.relative(row.left) } else { pair.right.relative(row.right) };
        if row.own != State::Directory || !changes {
            writeln!(out, "{:<28} {}", row.own.code(), path.display())?;
        }
        if row.error != 0 {
            writeln!(out, "  {}", io::Error::from_raw_os_error(row.error as i32))?;
        }
        let _ = id;
    }
    let s = pair.summary();
    if pair.scope.active() {
        writeln!(out, "Analysis filters: include={:?}, exclude={:?}", pair.scope.include(), pair.scope.exclude())?;
    }
    if display.is_some() {
        writeln!(out, "Display masks affect the listing only; the summary covers the complete analysis scope.")?;
    }
    writeln!(
        out,
        "\n{} files, {} directories; {} differences, {} uncertain; {} verified equal, {} size/time matches.\nCompleted in {:.3} s. Backends: {:?}, {:?}.",
        s.files,
        s.dirs,
        s.different,
        s.uncertain,
        s.verified,
        s.unverified,
        pair.elapsed.as_secs_f64(),
        pair.left.backend,
        pair.right.backend
    )?;
    out.flush()
}

fn with_progress<T>(progress: &Progress, job: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    use std::sync::{Condvar, Mutex, atomic::AtomicBool};
    let finished = AtomicBool::new(false);
    let wake = (Mutex::new(()), Condvar::new());
    struct Done<'a> {
        finished: &'a AtomicBool,
        wake: &'a (Mutex<()>, Condvar),
    }
    impl Drop for Done<'_> {
        fn drop(&mut self) {
            let _lock = self.wake.0.lock().unwrap_or_else(|e| e.into_inner());
            self.finished.store(true, Relaxed);
            self.wake.1.notify_all();
        }
    }
    std::thread::scope(|scope| {
        let _done = Done { finished: &finished, wake: &wake };
        if io::stderr().is_terminal() {
            scope.spawn(|| {
                let mut lock = wake.0.lock().unwrap_or_else(|e| e.into_inner());
                while !finished.load(Relaxed) {
                    lock = wake.1.wait_timeout(lock, std::time::Duration::from_millis(250)).unwrap_or_else(|e| e.into_inner()).0;
                    if finished.load(Relaxed) {
                        break;
                    }
                    let phase = progress.phase.load(Relaxed);
                    let label = match phase {
                        1 => "Reading folders",
                        2 => "Matching paths",
                        3 => "Checking contents",
                        5 => "Indexing text",
                        6 => "Aligning lines",
                        _ => "Preparing",
                    };
                    if phase >= 5 {
                        eprint!("\r{label}: {} bytes read    ", progress.bytes.load(Relaxed));
                    } else {
                        eprint!(
                            "\r{label}: {} files, {} directories; {} / {} checked    ",
                            progress.files.load(Relaxed),
                            progress.dirs.load(Relaxed),
                            progress.done.load(Relaxed),
                            progress.total.load(Relaxed)
                        );
                    }
                }
                eprint!("\r{:120}\r", "");
            });
        }
        job()
    })
}

fn run() -> Result<i32, Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let mut paths: Vec<OsString> = Vec::new();
    let mut options = Options::default();
    let (mut json, mut changes, mut output) = (false, false, None);
    let mut positional = false;
    let mut text_mode = false;
    let (mut include_masks, mut exclude_masks, mut display_masks) = (String::new(), String::new(), None);
    let mut text_inline = false;
    let mut context = 3usize;
    let mut text_options = fc_core::text::TextOptions::default();
    while let Some(arg) = args.next() {
        if !positional && arg == "--" {
            positional = true;
            continue;
        }
        if !positional && arg.to_string_lossy().starts_with('-') {
            let mut value = || args.next().ok_or("missing option value");
            match arg.to_str().ok_or("option is not Unicode")? {
                "--help" | "-h" => {
                    help();
                    return Ok(0);
                }
                "--contents" => options.contents = true,
                "--include" => include_masks = value()?.into_string().map_err(|_| "mask is not Unicode")?,
                "--exclude" => exclude_masks = value()?.into_string().map_err(|_| "mask is not Unicode")?,
                "--show-masks" => display_masks = Some(fc_core::filter::Masks::parse(value()?.to_str().ok_or("mask is not Unicode")?)?),
                "--text" => text_mode = true,
                "--encoding-left" | "--encoding-right" => {
                    let side = usize::from(arg == "--encoding-right");
                    let encoding = value()?;
                    text_options.encodings[side] =
                        fc_core::text::EncodingChoice::parse(encoding.to_str().ok_or("invalid encoding")?).ok_or("invalid encoding")?;
                }
                "--inline" => text_inline = true,
                "--ignore-crlf" => text_options.ignore_crlf = true,
                "--ignore-spaces" => text_options.rules.ignore_spaces = true,
                "--ignore-tabs" => text_options.rules.ignore_tabs = true,
                "--ignore-blank-lines" => text_options.rules.ignore_blank_lines = true,
                "--ignore-case" => text_options.rules.ignore_case = true,
                "--ignore-line-endings" => text_options.rules.ignore_line_endings = true,
                "--context" => context = value()?.to_str().ok_or("invalid context")?.parse()?,
                "--text-memory" => {
                    let mib: usize = value()?.to_str().ok_or("invalid text memory limit")?.parse()?;
                    if !(1..=4096).contains(&mib) {
                        return Err("text memory limit must be 1..4096 MiB".into());
                    }
                    text_options.index_bytes = mib.checked_mul(1024 * 1024).ok_or("invalid text memory limit")?;
                }
                "--json" => json = true,
                "--changes" => changes = true,
                "--read-cloud" => options.skip_cloud = false,
                "--output" => output = Some(PathBuf::from(value()?)),
                "--threads" => {
                    options.threads = value()?.to_str().ok_or("invalid worker count")?.parse()?;
                    if !(1..=32).contains(&options.threads) {
                        return Err("worker count must be 1..32".into());
                    }
                }
                "--backend" => {
                    options.backend = match value()?.to_str() {
                        Some("auto") => Backend::Auto,
                        Some("walk") => Backend::Walk,
                        Some("mft") => Backend::Mft,
                        _ => return Err("invalid backend".into()),
                    }
                }
                "--case" => {
                    options.case = match value()?.to_str() {
                        Some("auto") => Case::Auto,
                        Some("sensitive") => Case::Sensitive,
                        Some("insensitive") => Case::Insensitive,
                        _ => return Err("invalid case policy".into()),
                    }
                }
                _ => return Err(format!("unknown option: {}", arg.to_string_lossy()).into()),
            }
        } else {
            paths.push(arg);
        }
    }
    if paths.len() != 2 {
        help();
        return Err("two folders or two files are required".into());
    }
    let progress = Arc::new(Progress::default());
    let _ = PROGRESS.set(progress.clone());
    unsafe {
        windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(cancel), 1);
    }
    let left = PathBuf::from(&paths[0]);
    let right = PathBuf::from(&paths[1]);
    options.scope = fc_core::filter::Scope::new(&include_masks, &exclude_masks)?;
    if text_mode || left.is_file() && right.is_file() {
        if options.scope.active() || display_masks.is_some() {
            return Err("folder masks require two directories".into());
        }
        if json {
            return Err("--json is available for folder reports; text comparison writes a unified diff".into());
        }
        let pair = match with_progress(&progress, || fc_core::text::TextDiff::open(Some(&left), Some(&right), text_options, &progress)) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(130),
            result => result?,
        };
        let write = |out: &mut dyn Write| -> io::Result<()> {
            let mut out = io::BufWriter::new(out);
            if text_inline {
                pair.write_inline(&mut out, &progress)?;
            }
            pair.write_unified(&mut out, context, &progress)?;
            out.flush()
        };
        if let Some(path) = output {
            let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&path)?;
            if let Err(e) = write(&mut file) {
                drop(file);
                let _ = std::fs::remove_file(&path);
                return Err(e.into());
            }
        } else {
            write(&mut io::stdout().lock())?
        }
        eprintln!(
            "{} / {} lines, {} changed regions; {:.3} s{}{}",
            pair.left.line_count(),
            pair.right.line_count(),
            pair.changes(),
            pair.elapsed.as_secs_f64(),
            if pair.coarse { " (coarse alignment)" } else { "" },
            if pair.matches_with_settings() { " (matches with comparison settings)" } else { "" }
        );
        return Ok(i32::from(pair.changes() != 0 || pair.format_differs()));
    }
    if text_options.ignore_crlf
        || text_options.rules.active()
        || text_inline
        || text_options.encodings.iter().any(|&e| e != fc_core::text::EncodingChoice::Auto)
    {
        return Err("text comparison rules require two text files".into());
    }
    if display_masks.is_some() && (json || output.is_some()) {
        return Err("--show-masks filters the console listing only; use --include/--exclude to limit reports".into());
    }
    let result = with_progress(&progress, || fc_core::compare(&left, &right, &options, &progress));
    let pair = match result {
        Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(130),
        r => r?,
    };
    if let Some(path) = output {
        fc_core::export::save(&pair, &path, json)?;
    } else if json {
        fc_core::export::json(&pair, &mut io::BufWriter::new(io::stdout().lock()))?;
    } else {
        text(&pair, changes, display_masks.as_ref())?;
    }
    let s = pair.summary();
    Ok(if s.uncertain != 0 {
        2
    } else if s.different != 0 {
        1
    } else {
        0
    })
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            if e.downcast_ref::<io::Error>().is_some_and(|e| e.kind() == io::ErrorKind::Interrupted) {
                std::process::exit(130)
            }
            eprintln!("FerrumCompare: {e}");
            std::process::exit(2);
        }
    }
}
