use crate::scan::ordinal;
use std::io;

#[derive(Clone, Debug, Default)]
pub struct Masks(Vec<Vec<u16>>);
impl Masks {
    pub fn parse(text: &str) -> io::Result<Self> {
        if text.len() > 4096 {
            return Err(io::Error::other("file masks exceed the 4096-byte limit"));
        }
        let mut masks = Vec::new();
        for pattern in text.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            if masks.len() == 64 || pattern.contains(['/', '\\', '\r', '\n']) {
                return Err(io::Error::other("use up to 64 name masks separated by semicolons; paths are not masks"));
            }
            masks.push(if pattern == "*.*" { vec![42] } else { pattern.encode_utf16().collect() });
        }
        Ok(Self(masks))
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn matches(&self, name: &[u16]) -> bool {
        self.0.iter().any(|pattern| glob(pattern, name))
    }
}

fn literal(a: &[u16], b: &[u16]) -> bool {
    if a.iter().chain(b).all(|&c| c < 128) {
        a.iter().zip(b).all(|(&a, &b)| (a as u8).eq_ignore_ascii_case(&(b as u8)))
    } else {
        ordinal(a, b, true).is_eq()
    }
}

fn glob(pattern: &[u16], name: &[u16]) -> bool {
    let (mut p, mut n, mut star, mut retry) = (0, 0, None, 0);
    while n < name.len() {
        if pattern.get(p) == Some(&42) {
            p += 1;
            star = Some(p);
            retry = n;
            continue;
        }
        if pattern.get(p) == Some(&63) {
            p += 1;
            n += 1;
            continue;
        }
        let end = p + pattern[p..].iter().take_while(|&&c| c != 42 && c != 63).count();
        let len = end - p;
        if len != 0 && len <= name.len() - n && literal(&pattern[p..end], &name[n..n + len]) {
            p = end;
            n += len;
            continue;
        }
        let Some(back) = star else { return false };
        retry += 1;
        n = retry;
        p = back;
    }
    while pattern.get(p) == Some(&42) {
        p += 1;
    }
    p == pattern.len()
}

#[derive(Clone, Debug, Default)]
pub struct Scope {
    include: String,
    exclude: String,
    included: Masks,
    excluded: Masks,
}
impl Scope {
    pub fn include(&self) -> &str {
        &self.include
    }
    pub fn exclude(&self) -> &str {
        &self.exclude
    }
    pub fn excludes(&self, name: &[u16]) -> bool {
        self.excluded.matches(name)
    }
    pub fn from_display(text: &str) -> io::Result<Self> {
        let (mut include, mut exclude) = (Vec::new(), Vec::new());
        for part in text.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            if let Some(mask) = part.strip_prefix('!') {
                exclude.push(mask);
            } else {
                include.push(part);
            }
        }
        Self::new(&include.join(";"), &exclude.join(";"))
    }
    pub fn new(include: &str, exclude: &str) -> io::Result<Self> {
        Ok(Self { include: include.into(), exclude: exclude.into(), included: Masks::parse(include)?, excluded: Masks::parse(exclude)? })
    }
    pub fn active(&self) -> bool {
        !self.included.is_empty() || !self.excluded.is_empty()
    }
    pub fn allows(&self, name: &[u16], dir: bool) -> bool {
        !self.excluded.matches(name) && (dir || self.included.is_empty() || self.included.matches(name))
    }
}
