//! 篩選樣式的整合測試：樣式裡的 `/` 與 `\` 相同、沒命中的 `--exclude` 會警告、
//! agent 很自然會寫的 `--json` 與 `--follow-links none` 不會被擋下來。
//!
//! 這幾條都來自實際使用時踩到的坑：`--exclude "*/node_modules/*"` 以前什麼都不排除，
//! 命令照樣成功、結束代碼 0、沒有任何警告。

mod common;

use common::*;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

fn write(p: &Path) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, b"x").unwrap();
}

/// 4 個 package.json：projA 自己 1 個、它的 node_modules 裡 2 個、projB 1 個。
fn workspace(name: &str) -> PathBuf {
    let r = fixture(name);
    for f in [
        "projA\\package.json",
        "projA\\node_modules\\dep1\\package.json",
        "projA\\node_modules\\dep2\\package.json",
        "projB\\package.json",
        "projB\\Cargo.toml",
        "projB\\target\\x.txt",
    ] {
        write(&r.join(f));
    }
    r
}

fn run(args: &[&str]) -> Run {
    let v: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    voltdir(&v)
}

fn matched(r: &Run) -> i128 {
    assert_eq!(r.code, 0, "{}", r.stderr);
    parse_json(&r.text()).get("matched_total").num()
}

fn find_packages(root: &Path, extra: &[&str]) -> Run {
    let mut a = vec![
        "find",
        root.to_str().unwrap(),
        "--name",
        "package.json",
        "-q",
    ];
    a.extend_from_slice(extra);
    run(&a)
}

#[test]
fn forward_slash_patterns_exclude_just_like_backslash_ones() {
    let r = workspace("filters_exclude");
    assert_eq!(matched(&find_packages(&r, &[])), 4);
    for pattern in [
        "*/node_modules/*",
        "*\\node_modules\\*",
        "*node_modules*",
        "*/node_modules",
        "*\\node_modules",
    ] {
        assert_eq!(
            matched(&find_packages(&r, &["--exclude", pattern])),
            2,
            "--exclude {pattern}"
        );
    }
}

#[test]
fn forward_slash_in_the_path_filter_works() {
    let r = workspace("filters_path");
    for pattern in ["*/projA/*", "*\\projA\\*"] {
        assert_eq!(
            matched(&find_packages(&r, &["--path", pattern])),
            3,
            "--path {pattern}"
        );
    }
}

#[test]
fn an_exclude_that_matches_nothing_warns_and_still_succeeds() {
    let r = workspace("filters_warn");
    let root = r.to_str().unwrap();

    // find（走 scan_resolved 的路徑）
    let none = run(&[
        "find",
        root,
        "--name",
        "package.json",
        "--exclude",
        "*\\no-such-folder*",
    ]);
    assert_eq!(matched(&none), 4, "nothing is excluded");
    assert!(none.stderr.contains("matched no path"), "{}", none.stderr);
    assert!(none.stderr.contains("no-such-folder"), "{}", none.stderr);

    // overview（走串流掃描的路徑）
    let ov = run(&["overview", root, "--exclude", "*/no-such-folder/*"]);
    assert_eq!(ov.code, 0, "{}", ov.stderr);
    assert!(ov.stderr.contains("matched no path"), "{}", ov.stderr);

    // 有命中的樣式不警告；只列沒命中的那個
    let mixed = run(&[
        "find",
        root,
        "--name",
        "package.json",
        "--exclude",
        "*/node_modules/*",
        "--exclude",
        "*/typo-folder/*",
    ]);
    assert_eq!(matched(&mixed), 2);
    assert!(mixed.stderr.contains("typo-folder"), "{}", mixed.stderr);
    assert!(
        !mixed.stderr.contains("'*/node_modules/*'"),
        "the pattern that matched must not be reported: {}",
        mixed.stderr
    );

    // -q 安靜
    let quiet = find_packages(&r, &["--exclude", "*\\no-such-folder*"]);
    assert_eq!(matched(&quiet), 4);
    assert!(
        !quiet.stderr.contains("matched no path"),
        "{}",
        quiet.stderr
    );
}

#[test]
fn json_and_follow_links_none_do_not_fail_real_runs() {
    let r = workspace("filters_flags");
    let root = r.to_str().unwrap();
    let ov = run(&["overview", root, "-q", "--json", "--follow-links", "none"]);
    assert_eq!(ov.code, 0, "{}", ov.stderr);
    assert_eq!(parse_json(&ov.text()).get("totals").get("files").num(), 6);
    let sc = run(&["scan", root, "-q", "--json", "--out", "-"]);
    assert_eq!(sc.code, 0, "{}", sc.stderr);
}
