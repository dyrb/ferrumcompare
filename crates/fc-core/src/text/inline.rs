use super::{TextReader, TextRules};
use crate::Progress;
use std::io;
use std::ops::Range;

const TOKENS: usize = 16_384;
const DISTANCE: usize = 128;
const WORK: usize = 128_000;

#[derive(Clone, Copy)]
struct Token {
    c: char,
    start: u64,
    end: u64,
}

/// Changed display-column ranges in the original lines (tabs occupy four-column stops).
/// Memory is bounded independently of source length. `coarse` discloses a resource fallback.
#[derive(Default)]
pub struct InlineDiff {
    pub spans: [Vec<Range<u64>>; 2],
    pub coarse: bool,
}

fn tokens(reader: &mut TextReader<'_>, line: Option<usize>, rules: TextRules, progress: &Progress) -> io::Result<Option<Vec<Token>>> {
    let Some(line) = line else { return Ok(Some(Vec::new())) };
    let source = reader.doc.lines[line];
    let (mut pos, mut column, mut checked, mut decoded) = (source.start, 0, source.start, 0);
    let mut out = Vec::new();
    while pos < source.start + source.len {
        let c = reader.scalar(&mut pos, source.start + source.len)?;
        decoded += 1;
        if decoded > TOKENS {
            return Ok(None);
        }
        let end = column + if c == '\t' { 4 - column % 4 } else { 1 };
        if !rules.skip(c) {
            let mut add = |c| -> io::Result<bool> {
                if out.len() == TOKENS {
                    return Ok(false);
                }
                if out.len() == out.capacity() {
                    out.try_reserve_exact(1024.min(TOKENS - out.len())).map_err(io::Error::other)?;
                }
                out.push(Token { c, start: column, end });
                Ok(true)
            };
            if rules.ignore_case {
                for c in c.to_lowercase() {
                    if !add(c)? {
                        return Ok(None);
                    }
                }
            } else if !add(c)? {
                return Ok(None);
            }
        }
        column = end;
        if pos - checked >= 4096 {
            progress.check()?;
            checked = pos;
        }
    }
    progress.check()?;
    Ok(Some(out))
}

fn ranges(tokens: &[Token], changed: &[bool]) -> Vec<Range<u64>> {
    let mut out: Vec<Range<u64>> = Vec::new();
    for (t, &changed) in tokens.iter().zip(changed) {
        if !changed {
            continue;
        }
        if let Some(last) = out.last_mut()
            && last.end >= t.start
        {
            last.end = last.end.max(t.end);
        } else {
            out.push(t.start..t.end);
        }
    }
    out
}

impl InlineDiff {
    pub fn between(
        left: &mut TextReader<'_>,
        a: Option<usize>,
        right: &mut TextReader<'_>,
        b: Option<usize>,
        rules: TextRules,
        progress: &Progress,
    ) -> io::Result<Self> {
        progress.check()?;
        let (Some(a_tokens), Some(b_tokens)) = (tokens(left, a, rules, progress)?, tokens(right, b, rules, progress)?) else {
            return Ok(Self {
                spans: [a.map(|_| 0..u64::MAX).into_iter().collect(), b.map(|_| 0..u64::MAX).into_iter().collect()],
                coarse: true,
            });
        };
        let prefix = a_tokens.iter().zip(&b_tokens).take_while(|(a, b)| a.c == b.c).count();
        let suffix = a_tokens[prefix..].iter().rev().zip(b_tokens[prefix..].iter().rev()).take_while(|(a, b)| a.c == b.c).count();
        let a = &a_tokens[prefix..a_tokens.len() - suffix];
        let b = &b_tokens[prefix..b_tokens.len() - suffix];
        let (mut ac, mut bc) = (vec![true; a.len()], vec![true; b.len()]);
        let mut coarse = false;
        if !a.is_empty() && !b.is_empty() {
            let max = DISTANCE.min(a.len().saturating_add(b.len()));
            let stride = 2 * max + 3;
            let offset = max as i32 + 1;
            let mut v = vec![-1i32; stride];
            v[offset as usize + 1] = 0;
            let mut trace = Vec::new();
            trace.try_reserve_exact(stride * (max + 1)).map_err(io::Error::other)?;
            let (mut work, mut found) = (WORK, None);
            'search: for d in 0..=max as i32 {
                progress.check()?;
                trace.extend_from_slice(&v);
                for k in (-d..=d).step_by(2) {
                    if work == 0 {
                        break 'search;
                    }
                    work -= 1;
                    let i = (offset + k) as usize;
                    let mut x = if k == -d || k != d && v[i - 1] < v[i + 1] { v[i + 1] } else { v[i - 1] + 1 };
                    let mut y = x - k;
                    if x < 0 || y < 0 || x as usize > a.len() || y as usize > b.len() {
                        continue;
                    }
                    while x >= 0 && y >= 0 && (x as usize) < a.len() && (y as usize) < b.len() && a[x as usize].c == b[y as usize].c {
                        if work == 0 {
                            break 'search;
                        }
                        work -= 1;
                        x += 1;
                        y += 1;
                    }
                    v[i] = x;
                    if x as usize == a.len() && y as usize == b.len() {
                        found = Some(d);
                        break 'search;
                    }
                }
            }
            if let Some(depth) = found {
                let (mut x, mut y) = (a.len() as i32, b.len() as i32);
                for d in (1..=depth).rev() {
                    let previous = &trace[d as usize * stride..(d as usize + 1) * stride];
                    let k = x - y;
                    let i = (offset + k) as usize;
                    let pk = if k == -d || k != d && previous[i - 1] < previous[i + 1] { k + 1 } else { k - 1 };
                    let px = previous[(offset + pk) as usize];
                    let py = px - pk;
                    while x > px && y > py {
                        x -= 1;
                        y -= 1;
                        ac[x as usize] = false;
                        bc[y as usize] = false;
                    }
                    (x, y) = (px, py);
                }
                while x > 0 && y > 0 {
                    x -= 1;
                    y -= 1;
                    ac[x as usize] = false;
                    bc[y as usize] = false;
                }
            } else {
                coarse = true;
            }
        }
        Ok(Self { spans: [ranges(a, &ac), ranges(b, &bc)], coarse })
    }
}
