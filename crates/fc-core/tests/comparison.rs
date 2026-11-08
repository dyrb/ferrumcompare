use fc_core::{Backend, NONE, Options, Progress, State};
use std::fs::{self, File, FileTimes, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{Duration, SystemTime};

fn write(path: &Path, data: &[u8], second: u64) {
    fs::write(path, data).unwrap();
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + second)))
        .unwrap();
}

fn state(pair: &fc_core::Comparison, name: &str) -> State {
    let id = pair
        .items
        .iter()
        .position(|row| {
            row.left != NONE && pair.left.relative(row.left) == Path::new(name)
                || row.right != NONE && pair.right.relative(row.right) == Path::new(name)
        })
        .unwrap();
    pair.items[id].own
}

#[test]
fn same_metadata_does_not_hide_content_differences_or_subminute_timestamps() {
    let root = std::env::temp_dir().join(format!("fc-compare-{}", std::process::id()));
    let (left, right) = (root.join("left"), root.join("right"));
    fs::create_dir_all(left.join("nested")).unwrap();
    fs::create_dir_all(right.join("nested")).unwrap();
    write(&left.join("same.bin"), b"identical", 0);
    write(&right.join("same.bin"), b"identical", 0);
    let mut payload = vec![0x55; (2 << 20) + 123];
    write(&left.join("different.bin"), &payload, 0);
    payload[(1 << 20) + 17] ^= 1;
    write(&right.join("different.bin"), &payload, 0);
    write(&left.join("time.txt"), b"same", 1);
    write(&right.join("time.txt"), b"same", 2);
    write(&left.join("size.hpp"), b"int menualpha = 200;\n", 0);
    write(&right.join("size.hpp"), b"int menualpha = 255;\nint width = 1280;\n", 0);
    write(&left.join("nested/left.txt"), b"left", 0);
    write(&right.join("nested/right.txt"), b"right", 0);
    write(&left.join("type"), b"file", 0);
    fs::create_dir(right.join("type")).unwrap();
    let options = Options { backend: Backend::Walk, ..Default::default() };
    let quick = fc_core::compare(&left, &right, &options, &Progress::default()).unwrap();
    assert_eq!(state(&quick, "different.bin"), State::Metadata);
    assert_eq!(state(&quick, "time.txt"), State::Time);
    assert_eq!(state(&quick, "nested/left.txt"), State::LeftOnly);
    assert_eq!(state(&quick, "nested/right.txt"), State::RightOnly);
    assert_eq!(state(&quick, "type"), State::Type);
    assert_eq!(state(&quick, "size.hpp"), State::Size);
    assert!(state(&quick, "size.hpp").content_differs());
    assert!(!state(&quick, "different.bin").content_differs());
    let full = fc_core::compare(&left, &right, &Options { contents: true, ..options }, &Progress::default()).unwrap();
    assert_eq!(state(&full, "different.bin"), State::Different);
    assert_eq!(state(&full, "same.bin"), State::Equal);
    assert_eq!(state(&full, "time.txt"), State::Equal);
    assert_eq!(state(&full, "size.hpp"), State::Size);
    assert!(state(&full, "different.bin").content_differs());
    assert!(!state(&full, "same.bin").content_differs());
    assert_eq!(full.summary().uncertain, 0);
    let mut report = Vec::new();
    fc_core::export::json(&full, &mut report).unwrap();
    assert!(
        String::from_utf8(report)
            .unwrap()
            .contains("\"state\":\"size_differs\",\"aggregate\":\"size_differs\",\"error\":0,\"content_equal\":false")
    );
    let held = OpenOptions::new().write(true).share_mode(7).open(left.join("same.bin")).unwrap();
    let busy =
        fc_core::compare(&left, &right, &Options { contents: true, backend: Backend::Walk, ..Default::default() }, &Progress::default())
            .unwrap();
    assert_eq!(state(&busy, "same.bin"), State::Error);
    drop(held);
    assert!(fc_core::export::save(&full, &left.join("report.json"), true).is_err());
    assert!(!left.join("report.json").exists());
    fc_core::export::save(&full, &root.join("report.json"), true).unwrap();
    assert!(fc_core::export::save(&full, &root.join("report.json"), true).is_err());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn cancellation_and_hard_links_preserve_results_without_redundant_content_reads() {
    let root = std::env::temp_dir().join(format!("fc-links-{}", std::process::id()));
    let (left, right) = (root.join("left"), root.join("right"));
    fs::create_dir_all(&left).unwrap();
    fs::create_dir_all(&right).unwrap();
    write(&left.join("linked.bin"), &[0x5a; 65536], 0);
    fs::hard_link(left.join("linked.bin"), right.join("linked.bin")).unwrap();
    let progress = Progress::default();
    let options = Options { contents: true, backend: Backend::Walk, ..Default::default() };
    let pair = fc_core::compare(&left, &right, &options, &progress).unwrap();
    assert_eq!(state(&pair, "linked.bin"), State::Equal);
    assert_eq!(progress.bytes.load(std::sync::atomic::Ordering::Relaxed), 0);
    progress.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(fc_core::compare(&left, &right, &options, &progress).is_err());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn analysis_masks_prune_excluded_subtrees_and_preserve_the_reported_scope() {
    let root = std::env::temp_dir().join(format!("fc-masks-{}", std::process::id()));
    let (left, right) = (root.join("left"), root.join("right"));
    for folder in [&left, &right] {
        fs::create_dir_all(folder.join("src")).unwrap();
        fs::create_dir_all(folder.join("build")).unwrap();
        write(&folder.join("src/main.CPP"), b"int x = 1;", 0);
        write(&folder.join("src/header.hpp"), b"#pragma once", 0);
        write(&folder.join("build/generated.cpp"), b"ignored", 0);
        write(&folder.join("notes.log"), b"ignored", 0);
    }
    let scope = fc_core::filter::Scope::new("*.cpp;*.hpp", "build;.git;target").unwrap();
    let pair = fc_core::compare(
        &left,
        &right,
        &Options { contents: true, backend: Backend::Walk, scope, ..Default::default() },
        &Progress::default(),
    )
    .unwrap();
    assert_eq!(pair.summary().files, 2);
    assert_eq!(pair.summary().verified, 2);
    assert!(pair.left.nodes.iter().enumerate().all(|(i, _)| !pair.left.relative(i as u32).to_string_lossy().contains("build")));
    let mut report = Vec::new();
    fc_core::export::json(&pair, &mut report).unwrap();
    assert!(String::from_utf8(report).unwrap().contains("\"filters\":{\"include\":\"*.cpp;*.hpp\",\"exclude\":\"build;.git;target\"}"));
    assert!(fc_core::filter::Masks::parse("*.*").unwrap().matches(&"README".encode_utf16().collect::<Vec<_>>()));
    assert!(fc_core::filter::Masks::parse("*.ТХТ").unwrap().matches(&"файл.тхт".encode_utf16().collect::<Vec<_>>()));
    let display = fc_core::filter::Scope::from_display("*.cpp;!generated*").unwrap();
    assert!(display.allows(&"main.cpp".encode_utf16().collect::<Vec<_>>(), false));
    assert!(!display.allows(&"generated.cpp".encode_utf16().collect::<Vec<_>>(), false));
    fs::remove_dir_all(root).unwrap();
}
