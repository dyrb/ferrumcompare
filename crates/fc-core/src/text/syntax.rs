//! Incremental lexical highlighting. States are saved at lines and long-line
//! checkpoints, so scrolling never needs to tokenize a whole file again.
use std::ops::Range;
use std::path::Path;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Plain,
    Code,
    Json,
    Markup,
    Script,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    Keyword,
    String,
    Number,
    Comment,
}
pub struct Token {
    pub range: Range<usize>,
    pub kind: Kind,
}
#[derive(Clone, Copy, Default, Debug)]
pub struct LexState {
    mode: u8,
    quote: u8,
    escaped: bool,
    previous: u8,
}
pub struct Lexer {
    language: Language,
    pub state: LexState,
}
impl Lexer {
    pub fn new(language: Language, state: LexState) -> Self {
        Self { language, state }
    }
    pub fn feed(&mut self, c: char) {
        if self.language == Language::Plain {
            return;
        }
        let s = &mut self.state;
        match s.mode {
            1 => {
                if matches!(c, '\r' | '\n') {
                    s.mode = 0
                }
            }
            2 => {
                if s.previous == b'*' && c == '/' {
                    s.mode = 0;
                    s.previous = 0;
                    return;
                }
            }
            3 => {
                if s.escaped {
                    s.escaped = false
                } else if c == '\\' {
                    s.escaped = true
                } else if c as u32 == s.quote as u32 || matches!(c, '\r' | '\n') && s.quote != b'`' {
                    s.mode = 0
                }
            }
            _ => {
                if self.language == Language::Code && s.previous == b'/' && c == '/' {
                    s.mode = 1
                } else if self.language == Language::Code && s.previous == b'/' && c == '*' {
                    s.mode = 2;
                    s.previous = 0;
                    return;
                } else if self.language == Language::Script && c == '#' {
                    s.mode = 1
                } else if matches!(c, '"' | '\'' | '`') {
                    s.mode = 3;
                    s.quote = c as u8;
                    s.escaped = false
                }
            }
        }
        s.previous = if c.is_ascii() { c as u8 } else { 0 };
    }
}

pub fn language(path: &Path) -> Language {
    match path.extension().and_then(|s| s.to_str()).unwrap_or("").to_ascii_lowercase().as_str() {
        "rs" | "c" | "h" | "cpp" | "hpp" | "cc" | "cs" | "java" | "js" | "jsx" | "ts" | "tsx" | "go" | "swift" | "kt" => Language::Code,
        "json" | "jsonl" => Language::Json,
        "xml" | "html" | "htm" | "svg" => Language::Markup,
        "py" | "sh" | "ps1" | "toml" | "yaml" | "yml" | "ini" | "cfg" => Language::Script,
        _ => Language::Plain,
    }
}
pub fn tokens(text: &str, language: Language) -> Vec<Token> {
    tokens_from(text, language, LexState::default())
}
pub fn tokens_from(text: &str, language: Language, state: LexState) -> Vec<Token> {
    if language == Language::Plain {
        return vec![Token { range: 0..text.len(), kind: Kind::Text }];
    }
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut p = 0;
    let mut lexer = Lexer::new(language, state);
    while p < bytes.len() {
        let start = p;
        let c = bytes[p];
        let kind = if matches!(lexer.state.mode, 1 | 2)
            || language == Language::Code && (bytes[p..].starts_with(b"//") || bytes[p..].starts_with(b"/*"))
            || language == Language::Script && c == b'#'
        {
            let mut first = true;
            while p < bytes.len() && (first || matches!(lexer.state.mode, 1 | 2) || lexer.state.previous == b'/') {
                let c = text[p..].chars().next().unwrap();
                lexer.feed(c);
                p += c.len_utf8();
                first = false;
            }
            Kind::Comment
        } else if lexer.state.mode == 3 || c == b'"' || c == b'\'' || c == b'`' {
            let c = text[p..].chars().next().unwrap();
            lexer.feed(c);
            p += c.len_utf8();
            while p < bytes.len() && lexer.state.mode == 3 {
                let c = text[p..].chars().next().unwrap();
                lexer.feed(c);
                p += c.len_utf8();
            }
            Kind::String
        } else if c.is_ascii_digit() {
            p += 1;
            while p < bytes.len() && (bytes[p].is_ascii_alphanumeric() || b"._".contains(&bytes[p])) {
                p += 1
            }
            Kind::Number
        } else if c.is_ascii_alphabetic() || c == b'_' {
            p += 1;
            while p < bytes.len() && (bytes[p].is_ascii_alphanumeric() || bytes[p] == b'_') {
                p += 1
            }
            if matches!(
                &text[start..p],
                "fn" | "let"
                    | "mut"
                    | "pub"
                    | "use"
                    | "mod"
                    | "impl"
                    | "struct"
                    | "enum"
                    | "trait"
                    | "match"
                    | "if"
                    | "else"
                    | "for"
                    | "while"
                    | "loop"
                    | "return"
                    | "break"
                    | "continue"
                    | "const"
                    | "static"
                    | "class"
                    | "def"
                    | "import"
                    | "from"
                    | "async"
                    | "await"
                    | "function"
                    | "var"
                    | "new"
                    | "true"
                    | "false"
                    | "null"
                    | "None"
                    | "self"
                    | "Self"
                    | "int"
                    | "void"
                    | "char"
                    | "bool"
                    | "public"
                    | "private"
                    | "protected"
                    | "try"
                    | "catch"
                    | "throw"
            ) {
                Kind::Keyword
            } else {
                Kind::Text
            }
        } else {
            p += text[p..].chars().next().unwrap().len_utf8();
            Kind::Text
        };
        if !matches!(kind, Kind::Comment | Kind::String) {
            for c in text[start..p].chars() {
                lexer.feed(c)
            }
        }
        if let Some(last) = out.last_mut().filter(|t: &&mut Token| t.kind == kind) {
            last.range.end = p
        } else {
            out.push(Token { range: start..p, kind });
        }
    }
    out
}
