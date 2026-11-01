//! 跨路徑 `diff`。

mod common;

use common::*;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

fn write(p: &Path, bytes: usize) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, vec![b'x'; bytes]).unwrap();
}

fn run(args: &[&str]) -> Run {
    let v: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    voltdir(&v)
}

fn snap(roots: &[&Path], out: &Path) {
    let mut args: Vec<&OsStr> = vec!["scan".as_ref()];
    args.extend(roots.iter().map(|r| r.as_os_str()));
    args.extend(["-q".as_ref(), "--out".as_ref(), out.as_os_str()]);
    let r = voltdir(&args);
    assert_eq!(r.code, 0, "{}", r.stderr);
}

/// monday / friday：兩個不同路徑的資料夾，形狀仿 `New-DemoWorkspace.ps1`（大小縮小）。
fn monday_friday(name: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let base = fixture(name);
    let (m, f) = (base.join("monday"), base.join("friday"));
    for r in [&m, &f] {
        write(&r.join("app").join("keep.bin"), 8_192);
    }
    write(&m.join("logs").join("server.log"), 65_536);
    write(&f.join("logs").join("server.log"), 98_304); // +32768
    write(&m.join("media").join("raw").join("day10.bin"), 40_960); // 消失
    for i in 0..5 {
        write(
            &f.join("app").join(".cache").join(format!("b{i}.bin")),
            8_192,
        ); // 新增 40960
    }
    let (sm, sf) = (
        base.join("..").join(format!("{name}_mon.ndjson")),
        base.join("..").join(format!("{name}_fri.ndjson")),
    );
    snap(&[&m], &sm);
    snap(&[&f], &sf);
    (m, f, sm, sf)
}

fn first(l: &J, a: &str, b: &str) -> J {
    l.get(a).get(b).get("items").get_idx(0).clone()
}

fn err_json(args: &[&str]) -> J {
    let mut v = args.to_vec();
    v.extend(["--log", "json"]);
    let r = run(&v);
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(r.stdout.is_empty(), "stdout must stay empty");
    parse_json(r.stderr.trim())
}

#[test]
fn t_ag_07_match_roots_pairs_by_relative_path_and_says_so() {
    let (m, f, sm, sf) = monday_friday("m10_basic");
    let r = run(&[
        "diff",
        sm.to_str().unwrap(),
        sf.to_str().unwrap(),
        "--match-roots",
        "-q",
    ]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let l = r.lines();
    assert_eq!(l.len(), 1);
    let d = &l[0];
    assert!(d.get("rebased").bool());
    assert!(d.get("overlap").is_null(), "no overlap key in rebased mode");
    assert_eq!(
        d.get("a").get("compare_root").str().to_lowercase(),
        m.to_string_lossy().to_lowercase()
    );
    assert_eq!(
        d.get("b").get("compare_root").str().to_lowercase(),
        f.to_string_lossy().to_lowercase()
    );
    assert_eq!(
        first(d, "growth", "files").get("path").str(),
        "logs\\server.log"
    );
    assert_eq!(first(d, "growth", "files").get("delta").num(), 32_768);
    assert_eq!(first(d, "growth", "dirs").get("path").str(), "logs");
    assert_eq!(first(d, "added", "dirs").get("path").str(), "app\\.cache");
    assert_eq!(first(d, "added", "dirs").get("physical").num(), 40_960);
    assert_eq!(
        first(d, "removed", "files").get("path").str(),
        "media\\raw\\day10.bin"
    );
    assert_eq!(first(d, "removed", "files").get("physical").num(), 40_960);
    // 所有清單的路徑都是相對路徑，且比較根本身不列為項目
    for (a, b) in [
        ("growth", "dirs"),
        ("growth", "files"),
        ("shrink", "dirs"),
        ("shrink", "files"),
        ("added", "dirs"),
        ("added", "files"),
        ("removed", "dirs"),
        ("removed", "files"),
    ] {
        let J::Arr(items) = d.get(a).get(b).get("items") else {
            panic!("items")
        };
        for it in items {
            let p = it.get("path").str();
            assert!(
                !p.is_empty() && !p.starts_with('\\') && !p.contains(':'),
                "not relative: {p}"
            );
        }
    }
    let t = d.get("totals");
    assert_eq!(
        t.get("delta_physical").num(),
        t.get("b_physical").num() - t.get("a_physical").num()
    );
}

#[test]
fn t_ag_07_without_options_the_hint_names_match_roots() {
    let (_, _, sm, sf) = monday_friday("m10_nohint");
    let e = err_json(&["diff", sm.to_str().unwrap(), sf.to_str().unwrap()]);
    assert_eq!(e.get("code").str(), "diff_roots_disjoint");
    assert!(e.get("hint").str().contains("--match-roots"), "{e:?}");
    assert!(e.get("hint").str().contains("--a-root"), "{e:?}");
}

#[test]
fn t_ag_07_four_misuse_cases_each_exit_1_with_a_hint() {
    let (m, f, sm, sf) = monday_friday("m10_errors");
    let (sm, sf) = (sm.to_str().unwrap(), sf.to_str().unwrap());
    let (ms, fs) = (m.to_str().unwrap(), f.to_str().unwrap());
    // 1. 只給其中一個 root
    let e = err_json(&["diff", sm, sf, "--a-root", ms]);
    assert!(
        e.get("message").str().contains("must be given together"),
        "{e:?}"
    );
    assert!(e.get("hint").str().contains("--match-roots"), "{e:?}");
    // 2. --match-roots 與 --a-root 併用
    let e = err_json(&[
        "diff",
        sm,
        sf,
        "--match-roots",
        "--a-root",
        ms,
        "--b-root",
        fs,
    ]);
    assert!(
        e.get("message").str().contains("cannot be combined"),
        "{e:?}"
    );
    assert!(!e.get("hint").str().is_empty());
    // 3. --match-roots 但某份快照有多個根
    let base = fixture("m10_many");
    let (p, q) = (base.join("p"), base.join("q"));
    write(&p.join("a.bin"), 4_096);
    write(&q.join("b.bin"), 4_096);
    let multi = base.join("..").join("m10_multi.ndjson");
    snap(&[&p, &q], &multi);
    let multi = multi.to_str().unwrap();
    let e = err_json(&["diff", multi, sf, "--match-roots"]);
    assert!(e.get("message").str().contains("exactly one root"), "{e:?}");
    assert!(e.get("hint").str().contains("--a-root"), "{e:?}");
    // 3b. 多個根的快照，明確指定 --a-root 就可以比
    let ok = run(&[
        "diff",
        multi,
        sf,
        "--a-root",
        p.to_str().unwrap(),
        "--b-root",
        fs,
        "-q",
    ]);
    assert_eq!(ok.code, 0, "{}", ok.stderr);
    // 4. --a-root 不在該快照的任何根之下
    let e = err_json(&["diff", sm, sf, "--a-root", fs, "--b-root", fs]);
    assert!(
        e.get("message")
            .str()
            .contains("is not under any root of snapshot A"),
        "{e:?}"
    );
    assert!(!e.get("hint").str().is_empty());
}

#[test]
fn t_ag_07_under_glob_applies_to_the_relative_path() {
    let (_, _, sm, sf) = monday_friday("m10_under");
    let r = run(&[
        "diff",
        sm.to_str().unwrap(),
        sf.to_str().unwrap(),
        "--match-roots",
        "--under",
        "logs\\*",
        "-q",
    ]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let d = &r.lines()[0];
    assert_eq!(
        first(d, "growth", "files").get("path").str(),
        "logs\\server.log"
    );
    assert!(
        first(d, "added", "dirs").is_null(),
        "app\\.cache is outside logs\\*"
    );
    assert!(first(d, "removed", "files").is_null());
}

#[test]
fn t_ag_07_same_path_without_options_keeps_the_old_shape() {
    let base = fixture("m10_regress");
    let root = base.join("w");
    write(&root.join("a.bin"), 4_096);
    let (s1, s2) = (
        base.join("..").join("m10_r1.ndjson"),
        base.join("..").join("m10_r2.ndjson"),
    );
    snap(&[&root], &s1);
    write(&root.join("a.bin"), 8_192);
    snap(&[&root], &s2);
    let r = run(&["diff", s1.to_str().unwrap(), s2.to_str().unwrap(), "-q"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let text = r.text();
    assert!(!text.contains("rebased") && !text.contains("compare_root"));
    // 頂層欄位順序與舊版相同
    let keys = [
        "\"schema\"",
        "\"a\"",
        "\"b\"",
        "\"overlap\"",
        "\"notes\"",
        "\"totals\"",
        "\"growth\"",
        "\"shrink\"",
        "\"added\"",
        "\"removed\"",
        "\"limited_by_snapshot\"",
    ];
    let mut at = 0;
    for k in keys {
        let i = text[at..]
            .find(k)
            .unwrap_or_else(|| panic!("missing or out of order: {k}"));
        at += i;
    }
}

/// 以真正的示範資料（`scripts\New-DemoWorkspace.ps1` 產生的 monday／friday）驗證。
/// 需要示範資料（環境變數 `VOLTDIR_DEMO`），所以預設不跑；用 `cargo test -- --ignored` 執行。
#[test]
#[ignore = "needs the demo workspace made by scripts\\New-DemoWorkspace.ps1 (set VOLTDIR_DEMO)"]
fn t_ag_07_demo_workspace_monday_vs_friday() {
    let demo = PathBuf::from(
        std::env::var("VOLTDIR_DEMO")
            .expect("set VOLTDIR_DEMO to the folder made by scripts\\New-DemoWorkspace.ps1"),
    );
    let base = fixture("m10_demo");
    let (sm, sf) = (
        base.join("..").join("m10_demo_mon.ndjson"),
        base.join("..").join("m10_demo_fri.ndjson"),
    );
    snap(&[&demo.join("monday")], &sm);
    snap(&[&demo.join("friday")], &sf);
    let r = run(&[
        "diff",
        sm.to_str().unwrap(),
        sf.to_str().unwrap(),
        "--match-roots",
        "-q",
    ]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let d = &r.lines()[0];
    assert!(d.get("rebased").bool());
    // app-api\.cache 在 monday 不存在，所以它在 added.dirs（不是 growth.dirs）
    assert_eq!(
        first(d, "added", "dirs").get("path").str(),
        "app-api\\.cache"
    );
    assert_eq!(first(d, "added", "dirs").get("physical").num(), 209_715_200);
    assert_eq!(
        first(d, "growth", "files").get("path").str(),
        "logs\\server.log"
    );
    assert_eq!(first(d, "growth", "files").get("delta").num(), 31_457_280);
    assert_eq!(
        first(d, "removed", "files").get("path").str(),
        "media\\training\\raw\\day10.mp4"
    );
    assert_eq!(
        first(d, "removed", "files").get("physical").num(),
        25_165_824
    );
}
