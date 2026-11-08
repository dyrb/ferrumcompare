use super::{Document, EncodingChoice, TextReader, TextRules};
use crate::Progress;
use std::io;
use std::ops::Range;
use std::path::Path;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
pub struct TextOptions {
    pub index_bytes: usize,
    pub frontier_bytes: usize,
    pub work: u64,
    /// Only CRLF versus LF is ignored. CR-only endings and a missing final newline still differ.
    pub ignore_crlf: bool,
    pub rules: TextRules,
    pub encodings: [EncodingChoice; 2],
}
impl Default for TextOptions {
    fn default() -> Self {
        Self {
            index_bytes: 192 * 1024 * 1024,
            frontier_bytes: 32 * 1024 * 1024,
            work: 16_000_000,
            ignore_crlf: false,
            rules: TextRules::default(),
            encodings: [EncodingChoice::Auto; 2],
        }
    }
}

#[derive(Clone, Debug)]
pub struct Block {
    pub left: Range<usize>,
    pub right: Range<usize>,
    /// Contents match under the selected text rules; original lines remain available.
    pub equal: bool,
    /// Source blank lines retained for display but excluded from alignment.
    pub ignored: bool,
    pub row: usize,
}
impl Block {
    pub fn rows(&self) -> usize {
        self.left.len().max(self.right.len())
    }
}

/// A borrowed result under the selected rules; obtaining it never reads the source files.
pub struct TextResult<'a> {
    pub change_rows: &'a [usize],
    pub format_differs: bool,
    pub matches_with_settings: bool,
}

pub struct TextDiff {
    pub left: Document,
    pub right: Document,
    pub blocks: Vec<Block>,
    pub rows: usize,
    change_rows: Vec<usize>,
    crlf_change_rows: Vec<usize>,
    /// Every equal line has been checked against source data, even when true.
    /// Some changed regions have coarser alignment after a resource limit.
    pub coarse: bool,
    pub elapsed: Duration,
    pub rules: TextRules,
    format_changed: bool,
    format_without_crlf: bool,
    ignore_crlf: bool,
}

struct Engine<'a> {
    a: TextReader<'a>,
    b: TextReader<'a>,
    progress: &'a Progress,
    work: u64,
    limited: bool,
    maps: &'a [LineMap; 2],
    rules: TextRules,
}
impl Engine<'_> {
    fn same(&mut self, a: usize, b: usize) -> io::Result<Option<bool>> {
        let (a, b) = (self.maps[0].source(a), self.maps[1].source(b));
        if self.work == 0 {
            self.limited = true;
            return Ok(None);
        }
        self.work -= 1;
        if self.work.is_multiple_of(1024) {
            self.progress.check()?
        }
        let (left, right) = (self.a.doc.lines[a], self.b.doc.lines[b]);
        if left.hash == right.hash {
            let cost = left.len.saturating_add(right.len) / 4096;
            if cost > self.work {
                self.limited = true;
                return Ok(None);
            }
            self.work -= cost;
        }
        self.a.equal(a, &mut self.b, b, self.rules, self.progress).map(Some)
    }
    /// Myers middle split with two frontiers. No edit-history matrix or recursion.
    fn split(&mut self, a: &Range<usize>, b: &Range<usize>, memory: usize) -> io::Result<Option<(usize, usize)>> {
        let (n, m) = (a.len() as isize, b.len() as isize);
        let depth = (n + m + 1) / 2;
        let len = (2 * depth + 3) as usize;
        if len > memory / (2 * size_of::<isize>()) {
            self.limited = true;
            return Ok(None);
        }
        let mut f = Vec::new();
        let mut r = Vec::new();
        f.try_reserve_exact(len).map_err(io::Error::other)?;
        r.try_reserve_exact(len).map_err(io::Error::other)?;
        f.resize(len, -1isize);
        r.resize(len, -1isize);
        let offset = depth + 1;
        f[(offset + 1) as usize] = 0;
        r[(offset + 1) as usize] = 0;
        let delta = n - m;
        let odd = delta % 2 != 0;
        for d in 0..=depth {
            self.progress.check()?;
            for reverse in [false, true] {
                let (v, other) = if reverse { (&mut r, &f) } else { (&mut f, &r) };
                for k in (-d..=d).step_by(2) {
                    if self.work == 0 {
                        self.limited = true;
                        return Ok(None);
                    }
                    self.work -= 1;
                    let index = (offset + k) as usize;
                    let mut x = if k == -d || k != d && v[index - 1] < v[index + 1] { v[index + 1] } else { v[index - 1] + 1 };
                    let mut y = x - k;
                    if x < 0 || y < 0 || x > n || y > m {
                        continue;
                    }
                    while x < n && y < m {
                        let (ai, bi) = if reverse {
                            (a.end - 1 - x as usize, b.end - 1 - y as usize)
                        } else {
                            (a.start + x as usize, b.start + y as usize)
                        };
                        match self.same(ai, bi)? {
                            Some(true) => {
                                x += 1;
                                y += 1
                            }
                            Some(false) => break,
                            None => return Ok(None),
                        }
                    }
                    v[index] = x;
                    if odd != reverse {
                        let other_k = delta - k;
                        let other_index = offset + other_k;
                        if other_index >= 0 && (other_index as usize) < other.len() && other[other_index as usize] >= 0 {
                            let other_x = other[other_index as usize];
                            if x + other_x >= n {
                                return Ok(Some(if reverse {
                                    (a.start + (n - x) as usize, b.start + (m - y) as usize)
                                } else {
                                    (a.start + x as usize, b.start + y as usize)
                                }));
                            }
                        }
                    }
                }
            }
        }
        Ok(None)
    }
}

enum LineMap {
    All(usize),
    Selected(Vec<usize>),
}
impl LineMap {
    fn new(doc: &Document, ignore_blank: bool, memory: &mut usize, progress: &Progress) -> io::Result<Self> {
        if !ignore_blank || !doc.lines.iter().any(|l| l.blank()) {
            return Ok(Self::All(doc.line_count()));
        }
        let mut lines = Vec::new();
        for (i, line) in doc.lines.iter().enumerate() {
            if i.is_multiple_of(4096) {
                progress.check()?;
            }
            if !line.blank() {
                super::push(&mut lines, i, memory)?;
            }
        }
        Ok(Self::Selected(lines))
    }
    fn len(&self) -> usize {
        match self {
            Self::All(n) => *n,
            Self::Selected(lines) => lines.len(),
        }
    }
    fn source(&self, i: usize) -> usize {
        match self {
            Self::All(_) => i,
            Self::Selected(lines) => lines[i],
        }
    }
    fn bytes(&self) -> usize {
        match self {
            Self::All(_) => 0,
            Self::Selected(lines) => lines.capacity() * size_of::<usize>(),
        }
    }
}

fn restore_blank_lines(
    old: Vec<Block>,
    maps: &[LineMap; 2],
    counts: [usize; 2],
    memory: &mut usize,
    progress: &Progress,
) -> io::Result<Vec<Block>> {
    let mut out = Vec::new();
    let (mut a, mut b) = (0, 0);
    for block in &old {
        progress.check()?;
        if block.equal {
            for (i, j) in block.left.clone().zip(block.right.clone()) {
                if i.is_multiple_of(4096) {
                    progress.check()?;
                }
                let (x, y) = (maps[0].source(i), maps[1].source(j));
                emit_kind(&mut out, a..x, b..y, true, true, memory)?;
                emit(&mut out, x..x + 1, y..y + 1, true, memory)?;
                (a, b) = (x + 1, y + 1);
            }
        } else {
            let x = if block.left.is_empty() { a } else { maps[0].source(block.left.start) };
            let y = if block.right.is_empty() { b } else { maps[1].source(block.right.start) };
            emit_kind(&mut out, a..x, b..y, true, true, memory)?;
            let ae = if block.left.is_empty() { x } else { maps[0].source(block.left.end - 1) + 1 };
            let be = if block.right.is_empty() { y } else { maps[1].source(block.right.end - 1) + 1 };
            emit_aligned(&mut out, x..ae, y..be, memory)?;
            (a, b) = (ae, be);
        }
    }
    emit_kind(&mut out, a..counts[0], b..counts[1], true, true, memory)?;
    *memory += old.capacity() * size_of::<Block>();
    Ok(out)
}

fn signature(
    reader: &mut TextReader<'_>,
    line: usize,
    rules: TextRules,
    work: &mut u64,
    progress: &Progress,
) -> io::Result<Option<[u16; 64]>> {
    let source = reader.doc.lines[line];
    if source.len > 4096 || *work < 256 {
        return Ok(None);
    }
    let (mut pos, mut pending, mut previous) = (source.start, None, 0u32);
    let mut bins = [0u16; 64];
    for _ in 0..256 {
        *work -= 1;
        let Some(c) = reader.normalized(&mut pos, source.start + source.len, &mut pending, rules, progress)? else { break };
        let hash = previous.wrapping_mul(16777619) ^ c as u32;
        bins[(hash as usize ^ (hash >> 16) as usize) & 63] += 1;
        previous = c as u32;
    }
    Ok(Some(bins))
}

/// Bounded, sampled similarity changes pairing only, never the equality verdict.
fn refine_pairing(
    old: Vec<Block>,
    docs: [&Document; 2],
    maps: &[LineMap; 2],
    rules: TextRules,
    memory: &mut usize,
    mut work: u64,
    progress: &Progress,
) -> io::Result<Vec<Block>> {
    let (mut a, mut b) = (docs[0].reader(), docs[1].reader());
    let mut out = Vec::new();
    for block in &old {
        progress.check()?;
        let (n, m) = (block.left.len(), block.right.len());
        if block.equal || n == 0 || m == 0 || n > 32 || m > 32 || work < ((n + m) * 256 + n * m) as u64 {
            if block.equal {
                emit(&mut out, block.left.clone(), block.right.clone(), true, memory)?;
            } else {
                emit_aligned(&mut out, block.left.clone(), block.right.clone(), memory)?;
            }
            continue;
        }
        let mut signatures = [Vec::with_capacity(n), Vec::with_capacity(m)];
        for (side, reader) in [(0, &mut a), (1, &mut b)] {
            let lines = if side == 0 { &block.left } else { &block.right };
            for line in lines.clone() {
                let Some(s) = signature(reader, maps[side].source(line), rules, &mut work, progress)? else { break };
                signatures[side].push(s);
            }
        }
        if signatures[0].len() != n || signatures[1].len() != m {
            emit_aligned(&mut out, block.left.clone(), block.right.clone(), memory)?;
            continue;
        }
        let mut scores = vec![0u32; (n + 1) * (m + 1)];
        let similarity = |i: usize, j: usize| {
            let (a, b) = (&signatures[0][i], &signatures[1][j]);
            let total = a.iter().chain(b).map(|&n| n as u32).sum::<u32>();
            if total == 0 {
                return 0;
            }
            let common = a.iter().zip(b).map(|(&a, &b)| a.min(b) as u32).sum::<u32>();
            (2000 * common / total).saturating_sub(550)
        };
        for i in 0..n {
            for j in 0..m {
                work -= 1;
                scores[(i + 1) * (m + 1) + j + 1] =
                    scores[i * (m + 1) + j + 1].max(scores[(i + 1) * (m + 1) + j]).max(scores[i * (m + 1) + j] + similarity(i, j));
            }
        }
        let (mut i, mut j) = (n, m);
        let mut pairs = Vec::new();
        while i > 0 && j > 0 {
            let value = scores[i * (m + 1) + j];
            let score = similarity(i - 1, j - 1);
            if score > 0 && value == scores[(i - 1) * (m + 1) + j - 1] + score {
                pairs.push((i - 1, j - 1));
                i -= 1;
                j -= 1;
            } else if value == scores[(i - 1) * (m + 1) + j] {
                i -= 1;
            } else {
                j -= 1;
            }
        }
        if pairs.is_empty() {
            emit_aligned(&mut out, block.left.clone(), block.right.clone(), memory)?;
            continue;
        }
        let (mut ai, mut bi) = (block.left.start, block.right.start);
        for (i, j) in pairs.into_iter().rev() {
            let (x, y) = (block.left.start + i, block.right.start + j);
            emit_aligned(&mut out, ai..x, bi..y, memory)?;
            emit_aligned(&mut out, x..x + 1, y..y + 1, memory)?;
            (ai, bi) = (x + 1, y + 1);
        }
        emit_aligned(&mut out, ai..block.left.end, bi..block.right.end, memory)?;
    }
    *memory += old.capacity() * size_of::<Block>();
    Ok(out)
}

impl TextDiff {
    pub fn open(left: Option<&Path>, right: Option<&Path>, options: TextOptions, progress: &Progress) -> io::Result<Self> {
        let started = Instant::now();
        progress.phase.store(5, Relaxed);
        progress.check()?;
        let mut memory = options.index_bytes;
        let left = match left {
            Some(p) => Document::open(p, options.rules, options.encodings[0], &mut memory, progress)?,
            None => Document::empty(Path::new("")),
        };
        let right = match right {
            Some(p) => Document::open(p, options.rules, options.encodings[1], &mut memory, progress)?,
            None => Document::empty(Path::new("")),
        };
        progress.phase.store(6, Relaxed);
        let maps = [
            LineMap::new(&left, options.rules.ignore_blank_lines, &mut memory, progress)?,
            LineMap::new(&right, options.rules.ignore_blank_lines, &mut memory, progress)?,
        ];
        let (mut blocks, mut coarse) = (Vec::<Block>::new(), false);
        let mut pairing_work = 0;
        {
            enum Task {
                Compare(Range<usize>, Range<usize>),
                Equal(Range<usize>, Range<usize>),
            }
            let mut tasks = Vec::new();
            super::push(&mut tasks, Task::Compare(0..maps[0].len(), 0..maps[1].len()), &mut memory)?;
            let mut engine = Engine {
                a: left.reader(),
                b: right.reader(),
                progress,
                work: options.work,
                limited: false,
                maps: &maps,
                rules: options.rules,
            };
            while let Some(task) = tasks.pop() {
                progress.check()?;
                let (mut a, mut b) = match task {
                    Task::Equal(a, b) => {
                        emit(&mut blocks, a, b, true, &mut memory)?;
                        continue;
                    }
                    Task::Compare(a, b) => (a, b),
                };
                let mut prefix = 0;
                while prefix < a.len().min(b.len()) && engine.same(a.start + prefix, b.start + prefix)? == Some(true) {
                    prefix += 1
                }
                if prefix > 0 {
                    emit(&mut blocks, a.start..a.start + prefix, b.start..b.start + prefix, true, &mut memory)?;
                    a.start += prefix;
                    b.start += prefix;
                }
                let mut suffix = 0;
                while suffix < a.len().min(b.len()) && engine.same(a.end - 1 - suffix, b.end - 1 - suffix)? == Some(true) {
                    suffix += 1
                }
                if suffix > 0 {
                    super::push(&mut tasks, Task::Equal(a.end - suffix..a.end, b.end - suffix..b.end), &mut memory)?;
                    a.end -= suffix;
                    b.end -= suffix;
                }
                if a.is_empty() || b.is_empty() {
                    emit(&mut blocks, a, b, false, &mut memory)?;
                } else if let Some((x, y)) = engine.split(&a, &b, options.frontier_bytes)?
                    && (x != a.start || y != b.start)
                    && (x != a.end || y != b.end)
                {
                    super::push(&mut tasks, Task::Compare(x..a.end, y..b.end), &mut memory)?;
                    super::push(&mut tasks, Task::Compare(a.start..x, b.start..y), &mut memory)?;
                } else {
                    coarse |= engine.limited;
                    emit(&mut blocks, a, b, false, &mut memory)?;
                }
            }
            if !coarse {
                pairing_work = engine.work.min(262_144);
            }
        }
        if pairing_work != 0 && blocks.iter().any(|b| !b.equal && !b.left.is_empty() && !b.right.is_empty()) {
            blocks = refine_pairing(blocks, [&left, &right], &maps, options.rules, &mut memory, pairing_work, progress)?;
        }
        if maps.iter().any(|m| matches!(m, LineMap::Selected(_))) {
            blocks = restore_blank_lines(blocks, &maps, [left.line_count(), right.line_count()], &mut memory, progress)?;
        }
        memory += maps.iter().map(LineMap::bytes).sum::<usize>();
        drop(maps);
        let mut rows = 0usize;
        for block in &mut blocks {
            block.row = rows;
            rows = rows.checked_add(block.rows()).ok_or_else(|| io::Error::other("text row limit exceeded"))?;
        }
        let (mut change_rows, mut crlf_change_rows) = (Vec::new(), Vec::new());
        let encoding_changed = left.encoding != right.encoding || left.bom != right.bom;
        let compare_styles = !options.rules.ignore_line_endings && !options.rules.ignore_blank_lines;
        let mut format_changed = encoding_changed || compare_styles && left.endings.map(|n| n != 0) != right.endings.map(|n| n != 0);
        let normalized_endings = |d: &Document| [d.endings[0] != 0 || d.endings[1] != 0, d.endings[2] != 0];
        let mut format_without_crlf = encoding_changed || compare_styles && normalized_endings(&left) != normalized_endings(&right);
        let mut last_changed_end = None;
        for block in &blocks {
            if block.ignored {
                continue;
            }
            if !block.equal {
                if last_changed_end != Some(block.row) {
                    super::push(&mut change_rows, block.row, &mut memory)?;
                    super::push(&mut crlf_change_rows, block.row, &mut memory)?;
                }
                last_changed_end = Some(block.row + block.rows());
                continue;
            }
            let (mut previous, mut previous_filtered) = (false, false);
            for (a, b) in block.left.clone().zip(block.right.clone()) {
                if a.is_multiple_of(4096) {
                    progress.check()?
                }
                let ends = (left.lines[a].ending(), right.lines[b].ending());
                let different = ends.0 != ends.1 && !(options.rules.ignore_line_endings && ends.0 != 0 && ends.1 != 0);
                let significant = different && !crlf_pair(ends.0, ends.1);
                format_changed |= different;
                format_without_crlf |= significant;
                if different && !previous {
                    super::push(&mut change_rows, block.row + a - block.left.start, &mut memory)?;
                }
                if significant && !previous_filtered {
                    super::push(&mut crlf_change_rows, block.row + a - block.left.start, &mut memory)?;
                }
                previous = different;
                previous_filtered = significant;
            }
        }
        left.validate()?;
        right.validate()?;
        progress.phase.store(4, Relaxed);
        Ok(Self {
            left,
            right,
            blocks,
            rows,
            change_rows,
            crlf_change_rows,
            coarse,
            elapsed: started.elapsed(),
            format_changed,
            format_without_crlf,
            ignore_crlf: options.ignore_crlf,
            rules: options.rules,
        })
    }
    pub fn row(&self, row: usize) -> Option<(Option<usize>, Option<usize>, bool)> {
        if row >= self.rows {
            return None;
        }
        let b = &self.blocks[self.blocks.partition_point(|b| b.row <= row) - 1];
        let n = row - b.row;
        Some(((n < b.left.len()).then_some(b.left.start + n), (n < b.right.len()).then_some(b.right.start + n), b.equal))
    }
    pub fn changes(&self) -> usize {
        self.result(self.ignore_crlf).change_rows.len()
    }
    pub fn row_ignored(&self, row: usize) -> bool {
        if row >= self.rows || !self.rules.ignore_blank_lines {
            return false;
        }
        self.blocks[self.blocks.partition_point(|b| b.row <= row) - 1].ignored
    }
    /// Both navigation indexes are prepared during alignment. Switching rules needs no I/O.
    pub fn result(&self, ignore_crlf: bool) -> TextResult<'_> {
        let change_rows = if ignore_crlf { &self.crlf_change_rows } else { &self.change_rows };
        let format_differs = if ignore_crlf { self.format_without_crlf } else { self.format_changed };
        TextResult {
            change_rows,
            format_differs,
            matches_with_settings: (ignore_crlf || self.rules.active()) && change_rows.is_empty() && !format_differs,
        }
    }
    pub fn row_of(&self, right: bool, line: usize) -> Option<usize> {
        let i = self.blocks.partition_point(|b| if right { b.right.end <= line } else { b.left.end <= line });
        let b = self.blocks.get(i)?;
        let range = if right { &b.right } else { &b.left };
        range.contains(&line).then_some(b.row + line - range.start)
    }
    pub fn format_differs(&self) -> bool {
        self.result(self.ignore_crlf).format_differs
    }
    pub fn matches_with_settings(&self) -> bool {
        self.result(self.ignore_crlf).matches_with_settings
    }
    /// Optional CLI diagnostics: independent changed ranges, in one-based display columns.
    pub fn write_inline(&self, out: &mut impl io::Write, progress: &Progress) -> io::Result<()> {
        let (mut a, mut b) = (self.left.reader(), self.right.reader());
        for block in self.blocks.iter().filter(|b| !b.equal) {
            for n in 0..block.rows() {
                progress.check()?;
                let left = (n < block.left.len()).then_some(block.left.start + n);
                let right = (n < block.right.len()).then_some(block.right.start + n);
                let changes = super::InlineDiff::between(&mut a, left, &mut b, right, self.rules, progress)?;
                write!(out, "# Inline -{} +{}", left.map_or(0, |n| n + 1), right.map_or(0, |n| n + 1))?;
                for (side, spans) in changes.spans.iter().enumerate() {
                    write!(out, " {}:", if side == 0 { "left" } else { "right" })?;
                    for span in spans {
                        if span.end == u64::MAX {
                            write!(out, " whole-line")?;
                        } else {
                            write!(out, " {}..{}", span.start + 1, span.end + 1)?;
                        }
                    }
                }
                writeln!(out, "{}", if changes.coarse { " (coarse)" } else { "" })?;
            }
        }
        Ok(())
    }
    /// Unified line-content diff. Encoding, BOM and newline differences are
    /// reported separately; the output is UTF-8 with LF separators.
    pub fn write_unified(&self, out: &mut impl io::Write, context: usize, progress: &Progress) -> io::Result<()> {
        let mut readers = [self.left.reader(), self.right.reader()];
        writeln!(out, "--- {}", self.left.path.display())?;
        writeln!(out, "+++ {}", self.right.path.display())?;
        for (active, name) in [
            (self.rules.ignore_spaces, "ASCII spaces"),
            (self.rules.ignore_tabs, "tabs"),
            (self.rules.ignore_blank_lines, "blank lines"),
            (self.rules.ignore_case, "case (Unicode lowercase)"),
            (self.rules.ignore_line_endings, "CR/LF/CRLF styles; final newline remains significant"),
        ] {
            if active {
                writeln!(out, "# Comparison: ignoring {name}.")?;
            }
        }
        if self.ignore_crlf {
            writeln!(out, "# Comparison: CRLF/LF differences ignored.")?;
        }
        if self.matches_with_settings() {
            writeln!(out, "# Result: matches with comparison settings.")?;
        }
        if self.format_differs() {
            writeln!(
                out,
                "# Format differs: {} BOM={} CRLF/LF/CR={:?}; {} BOM={} CRLF/LF/CR={:?}",
                self.left.encoding.name(),
                self.left.bom,
                self.left.endings,
                self.right.encoding.name(),
                self.right.bom,
                self.right.endings
            )?;
            for block in self.blocks.iter().filter(|b| b.equal && !b.ignored) {
                let mut offset = 0;
                while offset < block.left.len() {
                    progress.check()?;
                    let a = block.left.start + offset;
                    let b = block.right.start + offset;
                    let ends = (self.left.lines[a].ending(), self.right.lines[b].ending());
                    if ends.0 == ends.1
                        || self.ignore_crlf && crlf_pair(ends.0, ends.1)
                        || self.rules.ignore_line_endings && ends.0 != 0 && ends.1 != 0
                    {
                        offset += 1;
                        continue;
                    }
                    let start = offset;
                    offset += 1;
                    while offset < block.left.len()
                        && (self.left.lines[block.left.start + offset].ending(), self.right.lines[block.right.start + offset].ending())
                            == ends
                    {
                        offset += 1;
                        if offset.is_multiple_of(4096) {
                            progress.check()?
                        }
                    }
                    writeln!(
                        out,
                        "# Endings -{},{} +{},{}: {} -> {}",
                        a + 1,
                        offset - start,
                        b + 1,
                        offset - start,
                        self.left.line_ending(a),
                        self.right.line_ending(b)
                    )?;
                }
            }
        }
        if self.coarse {
            writeln!(out, "# Alignment limit reached; some changed regions are shown as a whole.")?;
        }
        let mut pending: Option<Range<usize>> = None;
        for b in self.blocks.iter().filter(|b| !b.equal) {
            let span = b.row.saturating_sub(context)..b.row.saturating_add(b.rows()).saturating_add(context).min(self.rows);
            if let Some(p) = pending.as_mut()
                && span.start <= p.end
            {
                p.end = p.end.max(span.end)
            } else {
                if let Some(p) = pending.take() {
                    self.write_hunk(p, out, progress, &mut readers)?
                }
                pending = Some(span);
            }
        }
        if let Some(p) = pending {
            self.write_hunk(p, out, progress, &mut readers)?
        }
        Ok(())
    }
    fn position(&self, row: usize) -> (usize, usize) {
        if row >= self.rows {
            return (self.left.line_count(), self.right.line_count());
        }
        let b = &self.blocks[self.blocks.partition_point(|b| b.row <= row) - 1];
        let n = row - b.row;
        (b.left.start + n.min(b.left.len()), b.right.start + n.min(b.right.len()))
    }
    fn write_hunk(
        &self,
        rows: Range<usize>,
        out: &mut impl io::Write,
        progress: &Progress,
        readers: &mut [TextReader<'_>; 2],
    ) -> io::Result<()> {
        let (a, b) = self.position(rows.start);
        let (ae, be) = self.position(rows.end);
        writeln!(out, "@@ -{},{} +{},{} @@", a + usize::from(ae > a), ae - a, b + usize::from(be > b), be - b)?;
        for block in self.blocks.iter().skip(self.blocks.partition_point(|b| b.row + b.rows() <= rows.start)) {
            if block.row >= rows.end {
                break;
            }
            progress.check()?;
            let start = rows.start.saturating_sub(block.row);
            let end = (rows.end - block.row).min(block.rows());
            let sides: &[(bool, u8)] = if block.equal { &[(false, b' ')] } else { &[(false, b'-'), (true, b'+')] };
            for &(right, prefix) in sides {
                let (doc, range) = if right { (&self.right, &block.right) } else { (&self.left, &block.left) };
                for i in start.min(range.len())..end.min(range.len()) {
                    out.write_all(&[prefix])?;
                    let line = range.start + i;
                    readers[usize::from(right)].write_line(line, out, progress)?;
                    out.write_all(b"\n")?;
                    if line + 1 == doc.line_count() && !doc.has_final_newline() {
                        out.write_all(b"\\ No newline at end of file\n")?
                    }
                }
            }
        }
        Ok(())
    }
}

fn crlf_pair(a: u8, b: u8) -> bool {
    matches!((a, b), (1, 3) | (3, 1))
}

fn emit(blocks: &mut Vec<Block>, a: Range<usize>, b: Range<usize>, equal: bool, memory: &mut usize) -> io::Result<()> {
    emit_kind(blocks, a, b, equal, false, memory)
}

fn emit_kind(blocks: &mut Vec<Block>, a: Range<usize>, b: Range<usize>, equal: bool, ignored: bool, memory: &mut usize) -> io::Result<()> {
    if a.is_empty() && b.is_empty() {
        return Ok(());
    }
    if let Some(last) = blocks.last_mut()
        && last.equal == equal
        && last.ignored == ignored
        && last.left.end == a.start
        && last.right.end == b.start
    {
        last.left.end = a.end;
        last.right.end = b.end;
    } else {
        super::push(blocks, Block { left: a, right: b, equal, ignored, row: 0 }, memory)?;
    }
    Ok(())
}

fn emit_aligned(blocks: &mut Vec<Block>, a: Range<usize>, b: Range<usize>, memory: &mut usize) -> io::Result<()> {
    if !a.is_empty() || !b.is_empty() {
        super::push(blocks, Block { left: a, right: b, equal: false, ignored: false, row: 0 }, memory)?;
    }
    Ok(())
}
