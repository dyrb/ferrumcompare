//! Bounded text indexing and disk-backed line access. Source handles deny writes
//! and deletion for the lifetime of a comparison; no memory-mapped I/O faults.
mod diff;
mod inline;
mod navigation;
mod rules;
pub mod syntax;
pub use diff::{Block, TextDiff, TextOptions, TextResult};
pub use inline::InlineDiff;
pub use navigation::RowMap;
pub use rules::TextRules;

use crate::Progress;
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, Read};
use std::os::windows::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::Ordering::Relaxed;
use windows_sys::Win32::Storage::FileSystem::*;

const BUFFER: usize = 128 * 1024;
const SEED: u64 = 0xcbf29ce484222325;
const CHECKPOINT: u64 = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf16Le,
    Utf16Be,
    Windows1251,
}
impl Encoding {
    pub fn name(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Utf16Le => "UTF-16 LE",
            Self::Utf16Be => "UTF-16 BE",
            Self::Windows1251 => "Windows-1251",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum EncodingChoice {
    #[default]
    Auto,
    Utf8,
    Utf16Le,
    Utf16Be,
    Windows1251,
}
impl EncodingChoice {
    pub const ALL: [Self; 5] = [Self::Auto, Self::Utf8, Self::Utf16Le, Self::Utf16Be, Self::Windows1251];
    pub fn encoding(self) -> Option<Encoding> {
        match self {
            Self::Auto => None,
            Self::Utf8 => Some(Encoding::Utf8),
            Self::Utf16Le => Some(Encoding::Utf16Le),
            Self::Utf16Be => Some(Encoding::Utf16Be),
            Self::Windows1251 => Some(Encoding::Windows1251),
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "auto" => Self::Auto,
            "utf8" | "utf-8" => Self::Utf8,
            "utf16le" => Self::Utf16Le,
            "utf16be" => Self::Utf16Be,
            "1251" | "cp1251" | "windows1251" => Self::Windows1251,
            _ => return None,
        })
    }
}

fn windows1251(byte: u8) -> io::Result<char> {
    if byte < 128 {
        return Ok(byte as char);
    }
    if byte >= 0xC0 {
        return Ok(char::from_u32(byte as u32 + 0x350).unwrap());
    }
    if byte == 0x98 {
        return Err(invalid());
    }
    // Windows supplies the punctuation/special-letter table once, not once per character.
    static TABLE: OnceLock<Result<[u16; 256], i32>> = OnceLock::new();
    let table = TABLE
        .get_or_init(|| {
            let source: Vec<u8> = (0..=255).collect();
            let mut table = [0u16; 256];
            let n =
                unsafe { windows_sys::Win32::Globalization::MultiByteToWideChar(1251, 0, source.as_ptr(), 256, table.as_mut_ptr(), 256) };
            if n != 256 { Err(io::Error::last_os_error().raw_os_error().unwrap_or(1)) } else { Ok(table) }
        })
        .as_ref()
        .map_err(|&e| io::Error::from_raw_os_error(e))?;
    char::from_u32(table[byte as usize] as u32).ok_or_else(invalid)
}

#[derive(Clone, Copy, Debug)]
struct Line {
    start: u64,
    len: u64,
    hash: u64,
    lex: syntax::LexState,
    ending: u8,
}
impl Line {
    fn ending(self) -> u8 {
        self.ending & 3
    }
    fn blank(self) -> bool {
        self.ending & 0x80 != 0
    }
}
#[derive(Clone, Copy)]
struct Checkpoint {
    line: usize,
    column: u64,
    offset: u64,
    lex: syntax::LexState,
}

pub struct Document {
    pub path: PathBuf,
    pub encoding: Encoding,
    pub bom: bool,
    pub bytes: u64,
    /// CRLF, LF, CR. Line endings are reported separately from line contents.
    pub endings: [u64; 3],
    pub max_columns: u64,
    pub language: syntax::Language,
    file: Option<File>,
    stamp: u64,
    lines: Vec<Line>,
    checkpoints: Vec<Checkpoint>,
}

struct At<'a> {
    file: &'a File,
    pos: u64,
    end: u64,
}
impl Read for At<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let n = (buffer.len() as u64).min(self.end.saturating_sub(self.pos)) as usize;
        if n == 0 {
            return Ok(0);
        }
        let n = self.file.seek_read(&mut buffer[..n], self.pos)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "text source became shorter"));
        }
        self.pos += n as u64;
        Ok(n)
    }
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "file is not valid in the selected text encoding")
}
fn limit() -> io::Error {
    io::Error::other("text index memory limit exceeded")
}

struct Decoder<R> {
    read: R,
    encoding: Encoding,
    pos: u64,
}
impl<R: Read> Decoder<R> {
    fn byte(&mut self) -> io::Result<Option<u8>> {
        let mut b = [0];
        loop {
            match self.read.read(&mut b) {
                Ok(0) => return Ok(None),
                Ok(_) => {
                    self.pos += 1;
                    return Ok(Some(b[0]));
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }
    fn required(&mut self) -> io::Result<u8> {
        self.byte()?.ok_or_else(invalid)
    }
    fn next(&mut self) -> io::Result<Option<char>> {
        let Some(first) = self.byte()? else { return Ok(None) };
        let c = match self.encoding {
            Encoding::Windows1251 => windows1251(first)?,
            Encoding::Utf8 => {
                if first < 128 {
                    first as char
                } else {
                    let n = match first {
                        0xc2..=0xdf => 2,
                        0xe0..=0xef => 3,
                        0xf0..=0xf4 => 4,
                        _ => return Err(invalid()),
                    };
                    let mut b = [first, 0, 0, 0];
                    for byte in &mut b[1..n] {
                        *byte = self.required()?
                    }
                    std::str::from_utf8(&b[..n]).map_err(|_| invalid())?.chars().next().unwrap()
                }
            }
            encoding => {
                let second = self.required()?;
                let unit = |a, b| if encoding == Encoding::Utf16Le { u16::from_le_bytes([a, b]) } else { u16::from_be_bytes([a, b]) };
                let u = unit(first, second);
                let scalar = if (0xd800..=0xdbff).contains(&u) {
                    let a = self.required()?;
                    let b = self.required()?;
                    let v = unit(a, b);
                    if !(0xdc00..=0xdfff).contains(&v) {
                        return Err(invalid());
                    }
                    0x10000 + ((u as u32 - 0xd800) << 10) + v as u32 - 0xdc00
                } else {
                    u as u32
                };
                char::from_u32(scalar).ok_or_else(invalid)?
            }
        };
        if c == '\0' {
            return Err(invalid());
        }
        Ok(Some(c))
    }
}

fn push<T>(v: &mut Vec<T>, item: T, remaining: &mut usize) -> io::Result<()> {
    if v.len() == v.capacity() {
        let count = (v.capacity() / 2).max(1024).min(*remaining / size_of::<T>());
        if count == 0 {
            return Err(limit());
        }
        let old = v.capacity();
        v.try_reserve_exact(count).map_err(io::Error::other)?;
        *remaining = remaining.saturating_sub((v.capacity() - old) * size_of::<T>());
    }
    v.push(item);
    Ok(())
}
fn compact<T>(v: &mut Vec<T>, remaining: &mut usize) {
    let old = v.capacity();
    v.shrink_to_fit();
    *remaining += (old - v.capacity()) * size_of::<T>();
}

impl Document {
    /// Copy a source selection using display columns, retaining original tabs and line endings.
    /// The caller supplies a byte limit; an oversized selection fails without truncation.
    pub fn selection_utf16(
        &self,
        mut start: (usize, u64),
        mut end: (usize, u64),
        bytes: usize,
        progress: &Progress,
    ) -> io::Result<Vec<u16>> {
        if end < start {
            std::mem::swap(&mut start, &mut end);
        }
        if end.0 > self.line_count() {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        let mut out = Vec::new();
        let mut append = |c: char| -> io::Result<()> {
            let mut units = [0; 2];
            let s = c.encode_utf16(&mut units);
            if out.len().saturating_add(s.len() + 1) > bytes / 2 {
                return Err(io::Error::other("selection exceeds the clipboard memory limit"));
            }
            if out.capacity() - out.len() < s.len() {
                out.try_reserve_exact(4096.min(bytes / 2 - out.len())).map_err(io::Error::other)?;
            }
            out.extend_from_slice(s);
            Ok(())
        };
        let mut reader = self.reader();
        for line in start.0..=end.0.min(self.line_count()) {
            progress.check()?;
            let Some(source) = self.lines.get(line).copied() else { break };
            let first = if line == start.0 { start.1 } else { 0 };
            let last = if line == end.0 { end.1 } else { u64::MAX };
            let (mut pos, mut col, _) = self.seek_lex(line, first);
            let mut checked = pos;
            while pos < source.start + source.len && col < last {
                let c = reader.scalar(&mut pos, source.start + source.len)?;
                let next = col + if c == '\t' { 4 - col % 4 } else { 1 };
                if next > first {
                    append(c)?;
                }
                col = next;
                if pos - checked >= 4096 {
                    progress.check()?;
                    checked = pos;
                }
            }
            if line < end.0 {
                match source.ending() {
                    3 => {
                        append('\r')?;
                        append('\n')?;
                    }
                    2 => append('\r')?,
                    1 => append('\n')?,
                    _ => {}
                }
            }
        }
        if out.len() >= bytes / 2 {
            return Err(io::Error::other("selection exceeds the clipboard memory limit"));
        }
        out.try_reserve_exact(1).map_err(io::Error::other)?;
        out.push(0);
        Ok(out)
    }
    pub fn empty(path: &Path) -> Self {
        Self {
            path: path.into(),
            encoding: Encoding::Utf8,
            bom: false,
            bytes: 0,
            endings: [0; 3],
            max_columns: 0,
            language: syntax::Language::Plain,
            file: None,
            stamp: 0,
            lines: Vec::new(),
            checkpoints: Vec::new(),
        }
    }
    fn open(path: &Path, rules: TextRules, choice: EncodingChoice, remaining: &mut usize, progress: &Progress) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let meta = file.metadata()?;
        if !meta.is_file()
            || meta.file_attributes() & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS) != 0
        {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "text comparison requires local regular files"));
        }
        let mut head = [0u8; 3];
        let n = file.seek_read(&mut head, 0)?;
        let (detected, skip) = if n >= 3 && head == [0xef, 0xbb, 0xbf] {
            (Encoding::Utf8, 3)
        } else if n >= 2 && head[..2] == [0xff, 0xfe] {
            (Encoding::Utf16Le, 2)
        } else if n >= 2 && head[..2] == [0xfe, 0xff] {
            (Encoding::Utf16Be, 2)
        } else {
            (Encoding::Utf8, 0)
        };
        let encoding = choice.encoding().unwrap_or(detected);
        if skip != 0 && encoding != detected {
            return Err(io::Error::other("selected encoding conflicts with BOM"));
        }
        let mut doc = Self {
            path: path.into(),
            encoding,
            bom: skip != 0,
            bytes: meta.len(),
            endings: [0; 3],
            max_columns: 0,
            language: syntax::language(path),
            file: Some(file),
            stamp: meta.last_write_time(),
            lines: Vec::new(),
            checkpoints: Vec::new(),
        };
        let read = BufReader::with_capacity(BUFFER, At { file: doc.file.as_ref().unwrap(), pos: skip, end: meta.len() });
        let mut decoder = Decoder { read, encoding, pos: skip };
        let mut lexer = syntax::Lexer::new(doc.language, syntax::LexState::default());
        let mut line_state = lexer.state;
        let (mut start, mut hash, mut column, mut next_checkpoint) = (skip, SEED, 0u64, CHECKPOINT);
        let (mut cr, mut checked) = (false, skip);
        let mut blank = true;
        loop {
            let offset = decoder.pos;
            let Some(c) = decoder.next()? else { break };
            if offset - checked >= BUFFER as u64 {
                progress.check()?;
                progress.bytes.fetch_add(offset - checked, Relaxed);
                checked = offset;
            }
            if cr {
                cr = false;
                if c == '\n' {
                    doc.endings[2] -= 1;
                    doc.endings[0] += 1;
                    doc.lines.last_mut().unwrap().ending |= 3;
                    start = decoder.pos;
                    lexer.feed(c);
                    line_state = lexer.state;
                    continue;
                }
            }
            if c == '\r' || c == '\n' {
                push(
                    &mut doc.lines,
                    Line {
                        start,
                        len: offset - start,
                        hash,
                        lex: line_state,
                        ending: (if c == '\r' { 2 } else { 1 }) | if blank { 0x80 } else { 0 },
                    },
                    remaining,
                )?;
                lexer.feed(c);
                line_state = lexer.state;
                doc.max_columns = doc.max_columns.max(column);
                doc.endings[if c == '\r' { 2 } else { 1 }] += 1;
                cr = c == '\r';
                start = decoder.pos;
                hash = SEED;
                blank = true;
                column = 0;
                next_checkpoint = CHECKPOINT;
            } else {
                if column >= next_checkpoint {
                    push(&mut doc.checkpoints, Checkpoint { line: doc.lines.len(), column, offset, lex: lexer.state }, remaining)?;
                    next_checkpoint = column + CHECKPOINT;
                }
                rules.hash(&mut hash, c);
                blank &= matches!(c, ' ' | '\t');
                lexer.feed(c);
                column += if c == '\t' { 4 - column % 4 } else { 1 };
            }
        }
        if start < meta.len() {
            push(
                &mut doc.lines,
                Line { start, len: meta.len() - start, hash, lex: line_state, ending: if blank { 0x80 } else { 0 } },
                remaining,
            )?;
            doc.max_columns = doc.max_columns.max(column);
        }
        progress.bytes.fetch_add(meta.len() - checked, Relaxed);
        progress.check()?;
        compact(&mut doc.lines, remaining);
        compact(&mut doc.checkpoints, remaining);
        doc.validate()?;
        Ok(doc)
    }
    fn validate(&self) -> io::Result<()> {
        if let Some(file) = &self.file {
            let m = file.metadata()?;
            if m.len() != self.bytes || m.last_write_time() != self.stamp {
                return Err(io::Error::other("file changed during text comparison"));
            }
        }
        Ok(())
    }
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }
    pub fn line_ending(&self, line: usize) -> &'static str {
        match self.lines.get(line).map(|l| l.ending()) {
            Some(1) => "LF",
            Some(2) => "CR",
            Some(3) => "CRLF",
            Some(0) => "EOF",
            _ => "",
        }
    }
    pub fn has_final_newline(&self) -> bool {
        self.lines.last().is_some_and(|l| l.start + l.len < self.bytes)
    }
    pub fn reader(&self) -> TextReader<'_> {
        TextReader { doc: self, cache: vec![0; BUFFER], start: 0, len: 0 }
    }
    /// Literal, case-sensitive search. Carries partial matches across read blocks
    /// and checks for a superseding UI request without a polling thread.
    pub fn find(
        &self,
        needle: &str,
        from: (usize, u64),
        progress: &Progress,
        superseded: impl Fn() -> bool,
    ) -> io::Result<Option<(usize, u64)>> {
        let needle: Vec<char> = needle.chars().collect();
        if needle.is_empty() || needle.len() > 4096 {
            return Ok(None);
        }
        let mut failure = vec![0usize; needle.len()];
        for i in 1..needle.len() {
            let mut j = failure[i - 1];
            while j > 0 && needle[i] != needle[j] {
                j = failure[j - 1]
            }
            if needle[i] == needle[j] {
                j += 1
            }
            failure[i] = j;
        }
        let mut reader = self.reader();
        let mut positions = vec![0u64; needle.len()];
        for line in from.0..self.line_count() {
            progress.check()?;
            if superseded() {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let l = self.lines[line];
            let (mut pos, mut col) = self.seek_column(line, if line == from.0 { from.1 } else { 0 });
            let (mut matched, mut count) = (0usize, 0usize);
            while pos < l.start + l.len {
                let c = reader.scalar(&mut pos, l.start + l.len)?;
                if line != from.0 || col >= from.1 {
                    positions[count % needle.len()] = col;
                    count += 1;
                    while matched > 0 && c != needle[matched] {
                        matched = failure[matched - 1]
                    }
                    if c == needle[matched] {
                        matched += 1
                    }
                    if matched == needle.len() {
                        return Ok(Some((line, positions[(count - needle.len()) % needle.len()])));
                    }
                }
                col += if c == '\t' { 4 - col % 4 } else { 1 };
                if col.is_multiple_of(4096) {
                    progress.check()?;
                    if superseded() {
                        return Err(io::ErrorKind::Interrupted.into());
                    }
                }
            }
        }
        Ok(None)
    }
    /// Streams a complete logical line without allocating it or its encoded bytes.
    pub fn write_line(&self, line: usize, out: &mut impl io::Write, progress: &Progress) -> io::Result<()> {
        self.reader().write_line(line, out, progress)
    }
    fn seek_column(&self, line: usize, column: u64) -> (u64, u64) {
        let (pos, col, _) = self.seek_lex(line, column);
        (pos, col)
    }
    fn seek_lex(&self, line: usize, column: u64) -> (u64, u64, syntax::LexState) {
        let i = self.checkpoints.partition_point(|p| (p.line, p.column) <= (line, column));
        i.checked_sub(1).and_then(|i| self.checkpoints.get(i)).filter(|p| p.line == line).map(|p| (p.offset, p.column, p.lex)).unwrap_or((
            self.lines[line].start,
            0,
            self.lines[line].lex,
        ))
    }
}

/// One cache per consumer; painting never performs source reads on the UI thread.
pub struct TextReader<'a> {
    doc: &'a Document,
    cache: Vec<u8>,
    start: u64,
    len: usize,
}
impl TextReader<'_> {
    fn byte(&mut self, pos: u64) -> io::Result<u8> {
        if pos < self.start || pos >= self.start + self.len as u64 {
            self.start = pos / BUFFER as u64 * BUFFER as u64;
            self.len = self.doc.file.as_ref().unwrap().seek_read(&mut self.cache, self.start)?;
            if self.start + self.len as u64 <= pos {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "text source became shorter"));
            }
        }
        Ok(self.cache[(pos - self.start) as usize])
    }
    fn scalar(&mut self, pos: &mut u64, end: u64) -> io::Result<char> {
        struct Slice<'a, 'b> {
            reader: &'a mut TextReader<'b>,
            pos: u64,
            end: u64,
        }
        impl Read for Slice<'_, '_> {
            fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
                if self.pos == self.end || b.is_empty() {
                    return Ok(0);
                }
                b[0] = self.reader.byte(self.pos)?;
                self.pos += 1;
                Ok(1)
            }
        }
        let encoding = self.doc.encoding;
        let mut decoder = Decoder { read: Slice { reader: self, pos: *pos, end }, encoding, pos: *pos };
        let c = decoder.next()?.ok_or_else(invalid)?;
        *pos = decoder.pos;
        Ok(c)
    }
    /// Columns use Unicode scalars and four-column tabs. Checkpoints bound the
    /// work needed to reach a viewport even inside multi-gigabyte single lines.
    pub fn window(&mut self, line: usize, column: u64, width: usize, progress: &Progress) -> io::Result<String> {
        self.window_styled(line, column, width, progress).map(|(text, _)| text)
    }
    pub fn window_styled(&mut self, line: usize, column: u64, width: usize, progress: &Progress) -> io::Result<(String, syntax::LexState)> {
        let Some(l) = self.doc.lines.get(line) else { return Ok((String::new(), Default::default())) };
        let end = l.start + l.len;
        let (mut pos, mut col, state) = self.doc.seek_lex(line, column);
        let mut lexer = syntax::Lexer::new(self.doc.language, state);
        let mut visible_state = state;
        let mut out = String::new();
        let width = width.min(32768) as u64;
        while pos < end && col < column.saturating_add(width) {
            let c = self.scalar(&mut pos, end)?;
            let n = if c == '\t' { 4 - col % 4 } else { 1 };
            if col + n <= column {
                lexer.feed(c);
                visible_state = lexer.state;
            }
            if col + n > column {
                if c == '\t' {
                    for _ in col.max(column)..(col + n).min(column + width) {
                        out.push(' ');
                    }
                } else {
                    out.push(if c.is_control() { '\u{fffd}' } else { c });
                }
            }
            col += n;
            if col.is_multiple_of(1024) {
                progress.check()?
            }
        }
        progress.check()?;
        Ok((out, visible_state))
    }
    pub fn write_line(&mut self, line: usize, out: &mut impl io::Write, progress: &Progress) -> io::Result<()> {
        let l = *self.doc.lines.get(line).ok_or_else(|| io::Error::other("invalid text line"))?;
        let mut pos = l.start;
        let mut checked = pos;
        while pos < l.start + l.len {
            let c = self.scalar(&mut pos, l.start + l.len)?;
            let mut b = [0; 4];
            out.write_all(c.encode_utf8(&mut b).as_bytes())?;
            if pos - checked >= BUFFER as u64 {
                progress.check()?;
                checked = pos;
            }
        }
        progress.check()
    }
    fn normalized(
        &mut self,
        pos: &mut u64,
        end: u64,
        pending: &mut Option<std::char::ToLowercase>,
        rules: TextRules,
        progress: &Progress,
    ) -> io::Result<Option<char>> {
        let mut checked = *pos;
        loop {
            if let Some(c) = pending.as_mut().and_then(Iterator::next) {
                return Ok(Some(c));
            }
            if *pos == end {
                return Ok(None);
            }
            let c = self.scalar(pos, end)?;
            if *pos - checked >= 4096 {
                progress.check()?;
                checked = *pos;
            }
            if rules.skip(c) {
                continue;
            }
            if rules.ignore_case {
                *pending = Some(c.to_lowercase());
            } else {
                return Ok(Some(c));
            }
        }
    }
    fn equal(&mut self, a: usize, other: &mut TextReader<'_>, b: usize, rules: TextRules, progress: &Progress) -> io::Result<bool> {
        let l = self.doc.lines[a];
        let r = other.doc.lines[b];
        if l.hash != r.hash {
            return Ok(false);
        }
        if rules.content_active() {
            let (mut ap, mut bp, mut ac, mut bc) = (l.start, r.start, None, None);
            let mut checked = ap;
            loop {
                let a = self.normalized(&mut ap, l.start + l.len, &mut ac, rules, progress)?;
                let b = other.normalized(&mut bp, r.start + r.len, &mut bc, rules, progress)?;
                if a != b {
                    return Ok(false);
                }
                if a.is_none() {
                    return Ok(true);
                }
                if ap - checked >= 4096 {
                    progress.check()?;
                    checked = ap;
                }
            }
        }
        if self.doc.encoding == other.doc.encoding {
            if l.len != r.len {
                return Ok(false);
            }
            let mut offset = 0;
            while offset < l.len {
                self.byte(l.start + offset)?;
                other.byte(r.start + offset)?;
                let ai = (l.start + offset - self.start) as usize;
                let bi = (r.start + offset - other.start) as usize;
                let n = (self.len - ai).min(other.len - bi).min((l.len - offset).min(usize::MAX as u64) as usize);
                if self.cache[ai..ai + n] != other.cache[bi..bi + n] {
                    return Ok(false);
                }
                offset += n as u64;
                progress.check()?;
            }
            Ok(true)
        } else {
            let (mut ap, mut bp) = (l.start, r.start);
            let mut checked = ap;
            while ap < l.start + l.len && bp < r.start + r.len {
                if self.scalar(&mut ap, l.start + l.len)? != other.scalar(&mut bp, r.start + r.len)? {
                    return Ok(false);
                }
                if ap - checked >= 4096 {
                    progress.check()?;
                    checked = ap;
                }
            }
            Ok(ap == l.start + l.len && bp == r.start + r.len)
        }
    }
}
