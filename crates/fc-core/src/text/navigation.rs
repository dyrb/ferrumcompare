use super::TextDiff;
use crate::Progress;
use std::{io, ops::Range};

struct Span {
    source: Range<usize>,
    row: usize,
    folded: bool,
}

/// Display rows refer back to stable aligned rows; folding never removes source lines.
pub struct RowMap {
    spans: Vec<Span>,
    pub rows: usize,
    pub limited: bool,
}
impl RowMap {
    pub fn direct(rows: usize) -> Self {
        Self { spans: vec![Span { source: 0..rows, row: 0, folded: false }], rows, limited: false }
    }
    pub fn at(&self, row: usize) -> Option<(usize, Option<Range<usize>>)> {
        if row >= self.rows {
            return None;
        }
        let s = &self.spans[self.spans.partition_point(|s| s.row <= row) - 1];
        Some((if s.folded { s.source.start } else { s.source.start + row - s.row }, s.folded.then(|| s.source.clone())))
    }
    pub fn display_of(&self, source: usize) -> usize {
        if self.rows == 0 {
            return 0;
        }
        let i = self.spans.partition_point(|s| s.source.end <= source).min(self.spans.len() - 1);
        let s = &self.spans[i];
        if s.folded { s.row } else { (s.row + source.saturating_sub(s.source.start)).min(self.rows - 1) }
    }
    fn add(&mut self, range: Range<usize>, folded: bool) {
        if range.is_empty() {
            return;
        }
        let count = if folded { 1 } else { range.len() };
        if !folded
            && let Some(last) = self.spans.last_mut()
            && !last.folded
            && last.source.end == range.start
        {
            last.source.end = range.end;
        } else {
            self.spans.push(Span { source: range, row: self.rows, folded });
        }
        self.rows += count;
    }
    fn unchanged(&mut self, range: Range<usize>, context: usize, expanded: &[usize]) {
        if range.len() <= 2 * context + 1 || expanded.contains(&(range.start + context)) {
            self.add(range, false);
            return;
        }
        self.add(range.start..range.start + context, false);
        self.add(range.start + context..range.end - context, true);
        self.add(range.end - context..range.end, false);
    }
    pub fn folded(pair: &TextDiff, context: usize, ignore_crlf: bool, expanded: &[usize], progress: &Progress) -> io::Result<Self> {
        let mut out = Self { spans: Vec::new(), rows: 0, limited: false };
        let context = context.clamp(1, 100);
        let format = pair.result(ignore_crlf).format_differs;
        for block in &pair.blocks {
            progress.check()?;
            if !block.equal {
                out.add(block.row..block.row + block.rows(), false);
            } else if !format || block.ignored {
                out.unchanged(block.row..block.row + block.rows(), context, expanded);
            } else {
                let mut start = block.row;
                for n in 0..block.rows() {
                    if n.is_multiple_of(4096) {
                        progress.check()?;
                        if out.spans.len() > 65_536 {
                            let mut direct = Self::direct(pair.rows);
                            direct.limited = true;
                            return Ok(direct);
                        }
                    }
                    let row = block.row + n;
                    if pair.row_format_differs(row, ignore_crlf) {
                        out.unchanged(start..row, context, expanded);
                        out.add(row..row + 1, false);
                        start = row + 1;
                    }
                }
                out.unchanged(start..block.row + block.rows(), context, expanded);
            }
            if out.spans.len() > 65_536 {
                let mut direct = Self::direct(pair.rows);
                direct.limited = true;
                return Ok(direct);
            }
        }
        Ok(out)
    }
}

impl TextDiff {
    pub fn row_format_differs(&self, row: usize, ignore_crlf: bool) -> bool {
        if self.row_ignored(row) {
            return false;
        }
        let Some((Some(a), Some(b), _)) = self.row(row) else { return false };
        let (a, b) = (self.left.lines[a].ending(), self.right.lines[b].ending());
        a != b && !(a != 0 && b != 0 && (self.rules.ignore_line_endings || ignore_crlf && matches!((a, b), (1, 3) | (3, 1))))
    }
    pub fn change_map(&self, ignore_crlf: bool, bins: usize, progress: &Progress) -> io::Result<Vec<bool>> {
        let bins = bins.clamp(1, 4096);
        let mut out = vec![false; bins];
        if self.rows == 0 {
            return Ok(out);
        }
        let pos = |row: usize| (row as u128 * bins as u128 / self.rows as u128) as usize;
        let format = self.result(ignore_crlf).format_differs;
        for block in &self.blocks {
            progress.check()?;
            if !block.equal {
                let first = pos(block.row).min(bins - 1);
                let last = pos(block.row + block.rows() - 1).min(bins - 1);
                out[first..=last].fill(true);
            } else if format && !block.ignored {
                for n in 0..block.rows() {
                    if n.is_multiple_of(4096) {
                        progress.check()?;
                    }
                    if self.row_format_differs(block.row + n, ignore_crlf) {
                        out[pos(block.row + n).min(bins - 1)] = true;
                    }
                }
            }
        }
        Ok(out)
    }
}
