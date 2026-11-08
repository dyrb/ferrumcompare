use fc_core::{
    Progress,
    text::{EncodingChoice, InlineDiff, RowMap, TextDiff, TextOptions, TextRules},
};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering::Relaxed},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static ID: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!("fc-text-{}-{}", std::process::id(), ID.fetch_add(1, Relaxed)));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn write(&self, name: &str, bytes: impl AsRef<[u8]>) -> PathBuf {
        let p = self.0.join(name);
        fs::write(&p, bytes).unwrap();
        p
    }
}

#[test]
fn manual_encodings_decode_legacy_text_without_replacement_or_bom_guessing() {
    let f = Fixture::new();
    let text = "Привет Ёё €\r\n";
    let left = f.write("legacy.cpp", [0xcf, 0xf0, 0xe8, 0xe2, 0xe5, 0xf2, 0x20, 0xa8, 0xb8, 0x20, 0x88, 13, 10]);
    let right = f.write("unicode.cpp", text);
    let p = Progress::default();
    assert!(TextDiff::open(Some(&left), Some(&right), TextOptions::default(), &p).is_err());
    let options = TextOptions { encodings: [EncodingChoice::Windows1251, EncodingChoice::Auto], ..Default::default() };
    {
        let pair = TextDiff::open(Some(&left), Some(&right), options, &p).unwrap();
        assert!(pair.blocks.iter().all(|b| b.equal));
        assert!(pair.format_differs());
        assert_eq!(pair.left.reader().window(0, 0, 100, &p).unwrap(), text.trim_end());
        let copy = pair.left.selection_utf16((0, 0), (1, 0), 4096, &p).unwrap();
        assert_eq!(String::from_utf16(&copy[..copy.len() - 1]).unwrap(), text);
    }
    for (choice, bytes) in [
        (EncodingChoice::Utf16Le, text.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<_>>()),
        (EncodingChoice::Utf16Be, text.encode_utf16().flat_map(u16::to_be_bytes).collect::<Vec<_>>()),
    ] {
        f.write("legacy.cpp", bytes);
        let pair =
            TextDiff::open(Some(&left), Some(&right), TextOptions { encodings: [choice, EncodingChoice::Auto], ..Default::default() }, &p)
                .unwrap();
        assert!(pair.blocks.iter().all(|b| b.equal));
        assert!(!pair.left.bom);
    }
    f.write("legacy.cpp", [0x98]);
    assert!(TextDiff::open(Some(&left), Some(&right), options, &p).is_err());
    f.write("legacy.cpp", [0xef, 0xbb, 0xbf, b'a']);
    assert_eq!(TextDiff::open(Some(&left), Some(&right), options, &p).err().unwrap().to_string(), "selected encoding conflicts with BOM");
}

#[test]
fn folding_maps_source_rows_and_selection_preserves_original_text() {
    let f = Fixture::new();
    let text: String = (0..200).map(|i| format!("line {i}\r\n")).collect();
    let left = f.write("left.txt", &text);
    let right = f.write("right.txt", text.replace("line 100\r\n", "line 100 changed\r\n"));
    let p = Progress::default();
    {
        let pair = TextDiff::open(Some(&left), Some(&right), TextOptions::default(), &p).unwrap();
        let map = RowMap::folded(&pair, 3, false, &[], &p).unwrap();
        assert!(map.rows < pair.rows);
        for source in 0..pair.rows {
            let (aligned, range) = map.at(map.display_of(source)).unwrap();
            if let Some(range) = range {
                assert!(range.contains(&source));
                assert_eq!(aligned, range.start);
            } else {
                assert_eq!(aligned, source);
            }
        }
        for &changed in pair.result(false).change_rows {
            assert!(map.at(map.display_of(changed)).unwrap().1.is_none());
        }
        let fold = (0..map.rows).find_map(|row| map.at(row).unwrap().1).unwrap();
        let expanded = RowMap::folded(&pair, 3, false, &[fold.start], &p).unwrap();
        assert!(expanded.at(expanded.display_of(fold.start + 1)).unwrap().1.is_none());
        assert!(pair.change_map(false, 512, &p).unwrap().iter().any(|&b| b));
    }
    f.write("right.txt", text.replace("\r\n", "\n"));
    {
        let pair = TextDiff::open(Some(&left), Some(&right), TextOptions::default(), &p).unwrap();
        assert_eq!(RowMap::folded(&pair, 3, false, &[], &p).unwrap().rows, pair.rows);
        assert!(RowMap::folded(&pair, 3, true, &[], &p).unwrap().rows < pair.rows);
    }
    let source = "\tПривет\r\n世界\n";
    f.write("left.txt", source);
    f.write("right.txt", source);
    let pair = TextDiff::open(Some(&left), Some(&right), TextOptions::default(), &p).unwrap();
    let selection = pair.left.selection_utf16((0, 4), (1, 1), 4096, &p).unwrap();
    assert_eq!(String::from_utf16(&selection[..selection.len() - 1]).unwrap(), "Привет\r\n世");
    let reverse = pair.left.selection_utf16((1, 1), (0, 4), 4096, &p).unwrap();
    assert_eq!(selection, reverse);
    let all = pair.left.selection_utf16((0, 0), (pair.left.line_count(), 0), 4096, &p).unwrap();
    assert_eq!(String::from_utf16(&all[..all.len() - 1]).unwrap(), source);
    assert!(pair.left.selection_utf16((0, 0), (2, 0), 2, &p).is_err());
}

#[test]
fn inline_ranges_preserve_separate_edits_and_match_a_small_lcs_oracle() {
    let f = Fixture::new();
    {
        let left = f.write("left.cpp", "void render() {\n    int alpha = 200;\n    int beta = 10;\n}\n");
        let right = f.write("right.cpp", "void render() {\n    // inserted note\n    int alpha = 255;\n    int beta = 20;\n}\n");
        let pair = TextDiff::open(Some(&left), Some(&right), TextOptions::default(), &Progress::default()).unwrap();
        assert_eq!(pair.row_of(false, 1), pair.row_of(true, 2));
        assert_eq!(pair.row_of(false, 2), pair.row_of(true, 3));
    }
    for (a, b, expected) in
        [("🦀a=1; b=2", "🦀a=3; b=4", vec![3..4, 8..9]), ("\tleft=1; right=2", "\tleft=3; right=4", vec![9..10, 18..19])]
    {
        let left = f.write("left.cpp", a);
        let right = f.write("right.cpp", b);
        let p = Progress::default();
        let pair = TextDiff::open(Some(&left), Some(&right), TextOptions::default(), &p).unwrap();
        let inline =
            InlineDiff::between(&mut pair.left.reader(), Some(0), &mut pair.right.reader(), Some(0), TextRules::default(), &p).unwrap();
        assert!(!inline.coarse);
        assert_eq!(inline.spans, [expected.clone(), expected]);
    }
    let mut seed = 137u64;
    let mut random = || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        (seed >> 32) as usize
    };
    for _ in 0..80 {
        let a: String = (0..random() % 24).map(|_| (b'a' + (random() % 4) as u8) as char).collect();
        let b: String = (0..random() % 24).map(|_| (b'a' + (random() % 4) as u8) as char).collect();
        let left = f.write("left.cpp", &a);
        let right = f.write("right.cpp", &b);
        let p = Progress::default();
        let pair = TextDiff::open(Some(&left), Some(&right), TextOptions::default(), &p).unwrap();
        let inline = InlineDiff::between(
            &mut pair.left.reader(),
            (!a.is_empty()).then_some(0),
            &mut pair.right.reader(),
            (!b.is_empty()).then_some(0),
            TextRules::default(),
            &p,
        )
        .unwrap();
        assert!(!inline.coarse);
        let kept = |s: &str, side: usize| {
            s.chars()
                .enumerate()
                .filter_map(|(i, c)| (!inline.spans[side].iter().any(|r| r.contains(&(i as u64)))).then_some(c))
                .collect::<String>()
        };
        let common = kept(&a, 0);
        assert_eq!(common, kept(&b, 1), "a={a:?}, b={b:?}");
        let mut oracle = vec![vec![0; b.len() + 1]; a.len() + 1];
        for i in 0..a.len() {
            for j in 0..b.len() {
                oracle[i + 1][j + 1] =
                    if a.as_bytes()[i] == b.as_bytes()[j] { oracle[i][j] + 1 } else { oracle[i][j + 1].max(oracle[i + 1][j]) };
            }
        }
        assert_eq!(common.len(), oracle[a.len()][b.len()]);
    }
    let left = f.write("left.cpp", "HELLO a=1; b=2");
    let right = f.write("right.cpp", "hello a=3; b=4");
    {
        let p = Progress::default();
        let rules = TextRules { ignore_case: true, ..Default::default() };
        let pair = TextDiff::open(Some(&left), Some(&right), TextOptions { rules, ..Default::default() }, &p).unwrap();
        let inline = InlineDiff::between(&mut pair.left.reader(), Some(0), &mut pair.right.reader(), Some(0), rules, &p).unwrap();
        assert_eq!(inline.spans, [vec![8..9, 13..14], vec![8..9, 13..14]]);
    }
    let left = f.write("left.cpp", "a".repeat(20_000));
    let right = f.write("right.cpp", "b".repeat(20_000));
    let p = Progress::default();
    let pair = TextDiff::open(Some(&left), Some(&right), TextOptions::default(), &p).unwrap();
    assert!(
        InlineDiff::between(&mut pair.left.reader(), Some(0), &mut pair.right.reader(), Some(0), TextRules::default(), &p).unwrap().coarse
    );
}

#[test]
fn text_rules_keep_original_lines_and_do_not_hide_significant_characters() {
    let f = Fixture::new();
    let spaces = TextRules { ignore_spaces: true, ..Default::default() };
    let tabs = TextRules { ignore_tabs: true, ..Default::default() };
    let blanks = TextRules { ignore_blank_lines: true, ..Default::default() };
    let case = TextRules { ignore_case: true, ..Default::default() };
    let endings = TextRules { ignore_line_endings: true, ..Default::default() };
    let all = TextRules { ignore_spaces: true, ignore_tabs: true, ignore_blank_lines: true, ignore_case: true, ignore_line_endings: true };
    for (a, b, rules, equal) in [
        ("int x = 1;\n", "intx=1;\n", spaces, true),
        ("a\tb\n", "ab\n", tabs, true),
        ("a\tb\n", "ab\n", spaces, false),
        ("a b\n", "ab\n", tabs, false),
        ("a\u{a0}b\n", "ab\n", spaces, false),
        ("ПрИвЕт Σ İ\n", "привет σ i\u{307}\n", case, true),
        ("Straße\n", "STRASSE\n", case, false),
        ("é\n", "e\u{301}\n", case, false),
        ("a\n\n \t\nb\n", "a\nb\n", blanks, true),
        ("a\n \nb\n\nc\n", "a\nx\n\t\nc\n", blanks, false),
        ("a\n \nb\n \nc\n", "a\n\t\nc\n", blanks, false),
        (" \t\n\n", "", blanks, true),
        ("a\n\r", "a\n", blanks, true),
        ("a\n\n", "a", blanks, false),
        ("a\rb\r", "a\nb\n", endings, true),
        ("a\r", "a", endings, false),
        (" HELLO \r\n \t\r\nWORLD\t\r\n", "hello\nworld\n", all, true),
        ("int x = 200;\n", "int x = 255;\n", all, false),
    ] {
        let left = f.write("left.cpp", a);
        let right = f.write("right.cpp", b);
        let p = Progress::default();
        let pair = TextDiff::open(Some(&left), Some(&right), TextOptions { rules, ..Default::default() }, &p).unwrap();
        assert_eq!(pair.matches_with_settings(), equal, "a={a:?}, b={b:?}, rules={rules:?}");
        for (side, doc) in [(false, &pair.left), (true, &pair.right)] {
            let mut seen = Vec::new();
            for row in 0..pair.rows {
                let (a, b, _) = pair.row(row).unwrap();
                if let Some(line) = if side { b } else { a } {
                    seen.push(line);
                    assert_eq!(pair.row_of(side, line), Some(row));
                }
            }
            assert_eq!(seen, (0..doc.line_count()).collect::<Vec<_>>());
        }
        assert_eq!(fs::read(&left).unwrap(), a.as_bytes());
        assert_eq!(fs::read(&right).unwrap(), b.as_bytes());
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn alignment_preserves_lines_and_matches_a_small_lcs_oracle() {
    let f = Fixture::new();
    let mut seed = 97u64;
    let mut random = || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        (seed >> 32) as usize
    };
    for _ in 0..160 {
        let a: Vec<_> = (0..random() % 32).map(|_| format!("{}", random() % 6)).collect();
        let b: Vec<_> = (0..random() % 32).map(|_| format!("{}", random() % 6)).collect();
        let left = f.write("left.txt", if a.is_empty() { String::new() } else { format!("{}\n", a.join("\n")) });
        let right = f.write("right.txt", if b.is_empty() { String::new() } else { format!("{}\n", b.join("\n")) });
        let pair = TextDiff::open(Some(&left), Some(&right), TextOptions::default(), &Progress::default()).unwrap();
        let (mut ai, mut bi, mut equal) = (0, 0, 0);
        for block in &pair.blocks {
            assert_eq!((block.left.start, block.right.start), (ai, bi));
            if block.equal {
                assert_eq!(a[block.left.clone()], b[block.right.clone()]);
                equal += block.left.len();
            }
            ai = block.left.end;
            bi = block.right.end;
        }
        assert_eq!((ai, bi), (a.len(), b.len()));
        let mut oracle = vec![vec![0; b.len() + 1]; a.len() + 1];
        for i in 0..a.len() {
            for j in 0..b.len() {
                oracle[i + 1][j + 1] = if a[i] == b[j] { oracle[i][j] + 1 } else { oracle[i][j + 1].max(oracle[i + 1][j]) };
            }
        }
        assert!(!pair.coarse);
        assert_eq!(equal, oracle[a.len()][b.len()], "a={a:?}, b={b:?}, blocks={:?}", pair.blocks);
    }
}

#[test]
fn encodings_long_lines_limits_and_locked_sources() {
    let f = Fixture::new();
    let text = "fn main() {\r\n\tprintln!(\"Привет 🦀\");\r\n}\r\n";
    let a = f.write("a.rs", text);
    let b = f.write("b.rs", [vec![0xff, 0xfe], text.replace("\r\n", "\n").encode_utf16().flat_map(u16::to_le_bytes).collect()].concat());
    let p = Progress::default();
    let pair = TextDiff::open(Some(&a), Some(&b), TextOptions::default(), &p).unwrap();
    assert_eq!(pair.changes(), 1);
    assert!(pair.blocks.iter().all(|b| b.equal));
    assert!(pair.format_differs());
    assert!(pair.result(true).format_differs);
    assert!(!pair.result(true).matches_with_settings);
    assert_eq!(pair.left.reader().window(1, 0, 100, &p).unwrap(), "    println!(\"Привет 🦀\");");
    assert!(std::fs::OpenOptions::new().write(true).open(&a).is_err());
    drop(pair);
    let line = format!("{}END🦀", "a".repeat(2 * 1024 * 1024));
    f.write("a.rs", line.as_bytes());
    f.write("b.rs", line.as_bytes());
    let pair = TextDiff::open(Some(&a), Some(&b), TextOptions::default(), &p).unwrap();
    assert_eq!(pair.changes(), 0);
    assert_eq!(pair.left.reader().window(0, 2 * 1024 * 1024, 4, &p).unwrap(), "END🦀");
    assert_eq!(pair.left.find("END🦀", (0, 0), &p, || false).unwrap(), Some((0, 2 * 1024 * 1024)));
    assert_eq!(pair.left.find("END🦀", (0, 2 * 1024 * 1024 + 1), &p, || false).unwrap(), None);
    drop(pair);
    f.write("a.rs", b"a\r\nb\n");
    f.write("b.rs", b"a\nb\r\n");
    let pair = TextDiff::open(Some(&a), Some(&b), TextOptions::default(), &p).unwrap();
    assert!(pair.format_differs());
    assert_eq!(pair.result(false).change_rows, &[0]);
    drop(pair);
    let comment = format!("/*{}\nbody */ let value = 42;\n", "x".repeat(9000));
    f.write("a.rs", comment.as_bytes());
    f.write("b.rs", comment.as_bytes());
    let pair = TextDiff::open(Some(&a), Some(&b), TextOptions::default(), &p).unwrap();
    let (slice, state) = pair.left.reader().window_styled(0, 6000, 40, &p).unwrap();
    let tokens = fc_core::text::syntax::tokens_from(&slice, pair.left.language, state);
    assert!(tokens.iter().all(|t| t.kind == fc_core::text::syntax::Kind::Comment));
    let (slice, state) = pair.left.reader().window_styled(1, 0, 100, &p).unwrap();
    let tokens = fc_core::text::syntax::tokens_from(&slice, pair.left.language, state);
    assert!(tokens.iter().any(|t| t.kind == fc_core::text::syntax::Kind::Keyword && &slice[t.range.clone()] == "let"));
    drop(pair);
    f.write("a.rs", b"a\nb\nc\n");
    f.write("b.rs", b"x\ny\nz\n");
    let limited = TextOptions { work: 1, ..Default::default() };
    let pair = TextDiff::open(Some(&a), Some(&b), limited, &p).unwrap();
    assert!(pair.coarse);
    assert_eq!(pair.changes(), 1);
    assert_eq!(pair.rows, 3);
    drop(pair);
    assert!(TextDiff::open(Some(&a), Some(&b), TextOptions { index_bytes: 1, ..Default::default() }, &p).is_err());
    f.write("a.rs", [0xff, 0, 0]);
    assert!(TextDiff::open(Some(&a), Some(&b), TextOptions::default(), &p).is_err());
    p.cancel.store(true, Relaxed);
    assert_eq!(TextDiff::open(Some(&a), Some(&b), TextOptions::default(), &p).err().unwrap().kind(), std::io::ErrorKind::Interrupted);
}

#[test]
fn crlf_rules_preserve_text_cr_only_and_final_newline_differences() {
    let f = Fixture::new();
    for (a, b, strict, filtered, format) in [
        ("a\r\nb\r\n", "a\nb\n", 1, 0, false),
        ("a\r\nb\n", "a\nb\r\n", 1, 0, false),
        ("a\r\nb\r\n", "a\nchanged\n", 2, 1, false),
        ("a\r", "a\n", 1, 1, true),
        ("a\r\n", "a", 1, 1, true),
        ("a\r\nb\r\n", "a\nb", 1, 1, true),
        ("a\r\nb\r\nc\r\n", "a\nb\nc\r", 1, 1, true),
        ("a\r\n\r\n", "a\n", 2, 1, false),
        ("a\n", "a\n", 0, 0, false),
        ("", "", 0, 0, false),
    ] {
        let left = f.write("left.txt", a);
        let right = f.write("right.txt", b);
        let progress = Progress::default();
        let pair = TextDiff::open(Some(&left), Some(&right), TextOptions::default(), &progress).unwrap();
        let bytes = progress.bytes.load(Relaxed);
        assert_eq!(pair.changes(), strict, "a={a:?}, b={b:?}");
        let result = pair.result(true);
        assert_eq!(result.change_rows.len(), filtered, "a={a:?}, b={b:?}");
        assert_eq!(result.format_differs, format, "a={a:?}, b={b:?}");
        assert_eq!(result.matches_with_settings, filtered == 0 && !format);
        assert_eq!(progress.bytes.load(Relaxed), bytes);
    }
    let left = f.write("left.txt", b"a\r\nb\r\n");
    let right = f.write("right.txt", b"a\nb\n");
    let pair =
        TextDiff::open(Some(&left), Some(&right), TextOptions { ignore_crlf: true, ..Default::default() }, &Progress::default()).unwrap();
    assert_eq!(pair.changes(), 0);
    assert!(!pair.format_differs());
    assert!(pair.matches_with_settings());
    let mut output = Vec::new();
    pair.write_unified(&mut output, 3, &Progress::default()).unwrap();
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("# Result: matches with comparison settings."));
    assert!(!output.contains("# Format differs:"));
    assert!(!output.contains("@@"));
}
