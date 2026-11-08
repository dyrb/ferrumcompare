/// Text-only rules. Spaces mean U+0020 and tabs mean U+0009; other whitespace
/// stays significant. Case comparison uses Unicode lowercase mappings without
/// locale tailoring or canonical normalization. Original source text is retained.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextRules {
    pub ignore_spaces: bool,
    pub ignore_tabs: bool,
    /// Empty lines and lines containing only ASCII spaces or tabs.
    pub ignore_blank_lines: bool,
    pub ignore_case: bool,
    /// Ignore CR, LF and CRLF styles, but preserve a missing final newline.
    pub ignore_line_endings: bool,
}

impl TextRules {
    pub fn active(self) -> bool {
        self.content_active() || self.ignore_blank_lines || self.ignore_line_endings
    }
    pub(crate) fn content_active(self) -> bool {
        self.ignore_spaces || self.ignore_tabs || self.ignore_case
    }
    pub(crate) fn skip(self, c: char) -> bool {
        c == ' ' && self.ignore_spaces || c == '\t' && self.ignore_tabs
    }
    pub(crate) fn hash(self, hash: &mut u64, c: char) {
        if self.skip(c) {
            return;
        }
        let mut add = |c: char| *hash = (*hash ^ c as u64).wrapping_mul(0x100000001b3);
        if self.ignore_case {
            for c in c.to_lowercase() {
                add(c);
            }
        } else {
            add(c);
        }
    }
}
