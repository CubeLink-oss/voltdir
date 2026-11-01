//! M6：`find`與 `diff`。

mod common;

use common::*;
use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

fn write(p: &Path, bytes: usize) {
    std::fs::write(p, vec![b'x'; bytes]).unwrap();
}

fn ds(args: &[&OsStr]) -> Run {
    voltdir(args)
}

fn run(args: &[&str]) -> Run {
    let v: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    ds(&v)
}

fn snap(root: &Path, out: &Path) {
    let r = ds(&[
        "scan".as_ref(),
        root.as_os_str(),
        "-q".as_ref(),
        "--out".as_ref(),
        out.as_os_str(),
    ]);
    assert_eq!(r.code, 0, "{}", r.stderr);
}

fn find_in(l: &[J]) -> &J {
    assert_eq!(l.len(), 1, "find prints a single JSON document");
    assert_eq!(l[0].get("schema").str(), "voltdir.find/1");
    &l[0]
}

fn items(doc: &J) -> Vec<String> {
    match doc.get("items") {
        J::Arr(a) => a.iter().map(|x| x.get("path").str().to_string()).collect(),
        other => panic!("items is not an array: {other:?}"),
    }
}

fn names(doc: &J) -> Vec<String> {
    match doc.get("items") {
        J::Arr(a) => a.iter().map(|x| x.get("name").str().to_string()).collect(),
        other => panic!("items is not an array: {other:?}"),
    }
}

fn set_age_days(p: &Path, days: u64) {
    let f = std::fs::OpenOptions::new().write(true).open(p).unwrap();
    f.set_modified(SystemTime::now() - Duration::from_secs(days * 86_400))
        .unwrap();
}

/// 建立 find 的共用夾具。
fn find_fixture(name: &str) -> std::path::PathBuf {
    let r = fixture(name);
    std::fs::create_dir(r.join("sub")).unwrap();
    std::fs::create_dir(r.join("emptydir")).unwrap();
    write(&r.join("a.txt"), 10);
    write(&r.join("b.LOG"), 5_000);
    write(&r.join("c.log"), 20_000);
    write(&r.join("zero.bin"), 0);
    write(&r.join("sub").join("deep.txt"), 100);
    write(&r.join("a[1] b.txt"), 7);
    write(&r.join("資料 檔案.txt"), 7);
    write(&r.join("x.txt"), 1);
    write(&r.join("img_001.PNG"), 3);
    write(&r.join("img_x.png"), 3);
    set_age_days(&r.join("c.log"), 400);
    set_age_days(&r.join("b.LOG"), 10);
    r
}

#[test]
fn ec21_names_with_brackets_spaces_and_unicode_and_limit_zero() {
    let r = find_fixture("m6_ec21");
    let q = |extra: &[&str]| {
        let mut a = vec!["find", r.to_str().unwrap(), "-q"];
        a.extend_from_slice(extra);
        run(&a)
    };
    // `[` 與空白是字面字元
    let x = q(&["--name", "a[1] b.txt"]);
    assert_eq!(x.code, 0, "{}", x.stderr);
    assert_eq!(names(find_in(&x.lines())), ["a[1] b.txt"]);
    assert!(
        q(&["--name", "a1 b.txt"]).lines()[0]
            .get("matched_total")
            .num()
            == 0
    );
    // Unicode 與萬用字元
    let u = q(&["--name", "資料*"]);
    assert_eq!(names(find_in(&u.lines())), ["資料 檔案.txt"]);
    // `?` 只比對一個單位；不分大小寫
    let one = q(&["--name", "?.TXT"]);
    assert_eq!(names(find_in(&one.lines())), ["a.txt", "x.txt"]);
    // --limit 0：只有總數
    let z = q(&["--name", "*.txt", "--limit", "0"]);
    let l = z.lines();
    let d = find_in(&l);
    assert!(items(d).is_empty());
    assert_eq!(d.get("matched_total").num(), 5);
    assert!(d.get("truncated").bool());
}

#[test]
fn find_filters_combine_and_sorting_is_deterministic() {
    let r = find_fixture("m6_filters");
    let q = |extra: &[&str]| {
        let mut a = vec!["find", r.to_str().unwrap(), "-q"];
        a.extend_from_slice(extra);
        run(&a)
    };
    // 副檔名（不分大小寫）＋大小下限
    let a = q(&["--ext", ".log", "--min-size", "10000"]);
    assert_eq!(names(find_in(&a.lines())), ["c.log"]);
    // 依大小遞減
    let s = q(&["--ext", ".log", "--sort", "size"]);
    assert_eq!(names(find_in(&s.lines())), ["c.log", "b.LOG"]);
    // 日期：超過 100 天沒修改
    let o = q(&["--ext", ".log", "--older-than", "100"]);
    assert_eq!(names(find_in(&o.lines())), ["c.log"]);
    let n = q(&["--ext", ".log", "--newer-than", "100"]);
    assert_eq!(names(find_in(&n.lines())), ["b.LOG"]);
    // 空檔案與空目錄
    let e = q(&["--empty", "--type", "any"]);
    assert_eq!(names(find_in(&e.lines())), ["emptydir", "zero.bin"]);
    // 目錄以 rollup 比較；--type dir
    let d = q(&["--type", "dir", "--min-size", "1"]);
    let dl = d.lines();
    let dn = names(find_in(&dl));
    assert!(dn.contains(&"sub".to_string()), "{dn:?}");
    assert!(!dn.contains(&"emptydir".to_string()), "{dn:?}");
    // --path 比對完整路徑
    let p = q(&["--path", "*\\sub\\*", "--type", "any"]);
    assert_eq!(names(find_in(&p.lines())), ["deep.txt"]);
    // 限制筆數與總數；截斷旗標
    let lim = q(&["--name", "*.txt", "--limit", "2", "--sort", "size"]);
    let ll = lim.lines();
    let ld = find_in(&ll);
    assert_eq!(ld.get("matched_total").num(), 5);
    assert_eq!(items(ld).len(), 2);
    assert!(ld.get("truncated").bool());
    assert_eq!(names(ld), ["deep.txt", "a.txt"]);
}

#[test]
fn find_regex_is_supported_and_invalid_patterns_are_usage_errors() {
    let r = find_fixture("m6_regex");
    let x = run(&[
        "find",
        r.to_str().unwrap(),
        "-q",
        "--name-regex",
        r"^img_\d+\.(png|jpg)$",
    ]);
    assert_eq!(x.code, 0, "{}", x.stderr);
    assert_eq!(names(find_in(&x.lines())), ["img_001.PNG"]);
    let bad = run(&["find", r.to_str().unwrap(), "-q", "--name-regex", "(a"]);
    assert_eq!(bad.code, 1);
    assert!(bad.stdout.is_empty());
    // --name 與 --name-regex 擇一
    let both = run(&[
        "find",
        r.to_str().unwrap(),
        "--name",
        "a",
        "--name-regex",
        "b",
    ]);
    assert_eq!(both.code, 1);
    // 沒有指數級回溯：這個模式在回溯式引擎上會卡住
    let t = std::time::Instant::now();
    let slow = run(&["find", r.to_str().unwrap(), "-q", "--name-regex", "^(a+)+$"]);
    assert_eq!(slow.code, 0);
    assert!(t.elapsed().as_secs() < 10);
}

#[test]
fn find_from_a_snapshot_gives_the_same_items_as_a_live_scan() {
    let r = find_fixture("m6_snapshot");
    // 暖機：剛建立的目錄，時間戳記要等被開啟過一次才會更新
    snap(&r, &r.join("..").join("m6_warmup.ndjson"));
    let s = r.join("..").join("m6_snapshot.ndjson");
    snap(&r, &s);
    let live = run(&[
        "find",
        r.to_str().unwrap(),
        "-q",
        "--type",
        "any",
        "--sort",
        "size",
        "--limit",
        "1000",
    ]);
    let from = run(&[
        "find",
        "--snapshot",
        s.to_str().unwrap(),
        "-q",
        "--type",
        "any",
        "--sort",
        "size",
        "--limit",
        "1000",
    ]);
    assert_eq!(live.code, 0, "{}", live.stderr);
    assert_eq!(from.code, 0, "{}", from.stderr);
    let (ll, fl) = (live.lines(), from.lines());
    let (a, b) = (find_in(&ll), find_in(&fl));
    assert_eq!(items(a), items(b));
    assert_eq!(a.get("matched_total").num(), b.get("matched_total").num());
    // 項目欄位與快照的 file／dir 行相同
    assert_eq!(a.get("items"), b.get("items"));
    assert!(!b.get("limited_by_snapshot").bool());
}

#[test]
fn find_marks_results_limited_by_a_trimmed_snapshot_and_rejects_bad_ones() {
    let r = find_fixture("m6_trim");
    let s = r.join("..").join("m6_trim.ndjson");
    let sc = ds(&[
        "scan".as_ref(),
        r.as_os_str(),
        "-q".as_ref(),
        "--min-size".as_ref(),
        "1000".as_ref(),
        "--out".as_ref(),
        s.as_os_str(),
    ]);
    assert_eq!(sc.code, 0);
    let x = run(&["find", "--snapshot", s.to_str().unwrap(), "-q"]);
    let l = x.lines();
    assert!(find_in(&l).get("limited_by_snapshot").bool());
    // 只找目錄時不受 --min-size 影響
    let d = run(&[
        "find",
        "--snapshot",
        s.to_str().unwrap(),
        "-q",
        "--type",
        "dir",
    ]);
    assert!(!find_in(&d.lines()).get("limited_by_snapshot").bool());
    // 截斷的快照
    let text = std::fs::read_to_string(&s).unwrap();
    let cut = r.join("..").join("m6_cut.ndjson");
    std::fs::write(&cut, &text[..text.len() / 2]).unwrap();
    let bad = run(&["find", "--snapshot", cut.to_str().unwrap(), "-q"]);
    assert_eq!(bad.code, 1);
    assert!(bad.stdout.is_empty());
    // 掃描選項不可與 --snapshot 併用
    let mix = run(&[
        "find",
        "--snapshot",
        s.to_str().unwrap(),
        "--engine",
        "walk",
    ]);
    assert_eq!(mix.code, 1);
}

#[test]
fn find_and_diff_do_not_fail_on_files_whose_content_cannot_be_read() {
    // 沒有讀取權限的檔案仍會被列出（find 只用中繼資料），結束代碼為 0
    let r = fixture("m6_noread");
    let f = r.join("secret.bin");
    write(&f, 5_000);
    let user = std::env::var("USERNAME").unwrap();
    let deny = |on: bool| {
        let mut c = Command::new("icacls");
        c.arg(&f);
        if on {
            c.arg("/deny").arg(format!("{user}:(RD)"));
        } else {
            c.arg("/remove:d").arg(&user);
        }
        c.output().unwrap();
    };
    deny(true);
    struct Restore<'a>(&'a dyn Fn(bool));
    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            (self.0)(false);
        }
    }
    let _g = Restore(&deny);
    assert!(
        std::fs::File::open(&f).is_err(),
        "the content really is unreadable"
    );
    let x = run(&["find", r.to_str().unwrap(), "-q", "--name", "secret.bin"]);
    assert_eq!(x.code, 0, "{}", x.stderr);
    let l = x.lines();
    let d = find_in(&l);
    assert_eq!(names(d), ["secret.bin"]);
    assert_eq!(d.get("items").get_idx(0).get("size_logical").num(), 5_000);
}

// ---- diff ----

fn num(j: &J) -> i128 {
    j.num()
}

fn paths_of(doc: &J, group: &str, kind: &str) -> Vec<(String, i128)> {
    match doc.get(group).get(kind).get("items") {
        J::Arr(a) => a
            .iter()
            .map(|x| {
                let key = if !x.get("delta").is_null() {
                    x.get("delta")
                } else if !x.get("physical").is_null() {
                    x.get("physical")
                } else {
                    x.get("delta_direct")
                };
                (x.get("path").str().to_string(), num(key))
            })
            .collect(),
        other => panic!("not a list: {other:?}"),
    }
}

fn diff_doc(a: &Path, b: &Path, extra: &[&str]) -> (Run, Vec<J>) {
    let mut args = vec!["diff", a.to_str().unwrap(), b.to_str().unwrap(), "-q"];
    args.extend_from_slice(extra);
    let r = run(&args);
    let l = if r.stdout.is_empty() {
        Vec::new()
    } else {
        r.lines()
    };
    (r, l)
}

#[test]
fn diff_matches_two_real_scans_of_a_changing_directory() {
    let r = fixture("m6_diff");
    std::fs::create_dir_all(r.join("keep").join("inner")).unwrap();
    std::fs::create_dir(r.join("gone")).unwrap();
    write(&r.join("keep").join("grow.bin"), 4_096);
    write(&r.join("keep").join("shrink.bin"), 40_960);
    write(&r.join("keep").join("same.bin"), 8_192);
    write(&r.join("keep").join("inner").join("deep.bin"), 4_096);
    write(&r.join("gone").join("old.bin"), 12_288);
    write(&r.join("removed.txt"), 4_096);
    let sa = r.join("..").join("m6_diff_a.ndjson");
    let sb = r.join("..").join("m6_diff_b.ndjson");
    snap(&r, &sa);

    // 變更：成長、縮小、新增（含新目錄）、刪除（含整個目錄）
    write(&r.join("keep").join("grow.bin"), 20_480); // +16,384
    write(&r.join("keep").join("shrink.bin"), 4_096); // −36,864
    std::fs::create_dir(r.join("fresh")).unwrap();
    write(&r.join("fresh").join("new1.bin"), 81_920);
    write(&r.join("fresh").join("new2.bin"), 4_096);
    write(&r.join("keep").join("inner").join("added.bin"), 8_192);
    std::fs::remove_dir_all(r.join("gone")).unwrap();
    std::fs::remove_file(r.join("removed.txt")).unwrap();
    snap(&r, &sb);

    let (run1, l) = diff_doc(&sa, &sb, &["--top", "50"]);
    assert_eq!(run1.code, 0, "{}", run1.stderr);
    assert_eq!(l.len(), 1);
    let d = &l[0];
    assert_eq!(d.get("schema").str(), "voltdir.diff/1");

    let root = r.to_string_lossy().to_string();
    let p = |rel: &str| format!("{root}\\{rel}");
    // 檔案
    assert_eq!(
        paths_of(d, "growth", "files"),
        [(p("keep\\grow.bin"), 16_384),]
    );
    assert_eq!(
        paths_of(d, "shrink", "files"),
        [(p("keep\\shrink.bin"), -36_864)]
    );
    assert_eq!(
        paths_of(d, "added", "files"),
        [
            (p("fresh\\new1.bin"), 81_920),
            (p("keep\\inner\\added.bin"), 8_192),
            (p("fresh\\new2.bin"), 4_096),
        ]
    );
    assert_eq!(
        paths_of(d, "removed", "files"),
        [(p("gone\\old.bin"), 12_288), (p("removed.txt"), 4_096)]
    );
    // 目錄：新增的 fresh、消失的 gone
    let added_dirs = paths_of(d, "added", "dirs");
    assert_eq!(added_dirs, [(p("fresh"), 86_016)]);
    assert_eq!(paths_of(d, "removed", "dirs"), [(p("gone"), 12_288)]);
    // 預設以 direct 排序：keep（grow +16,384、shrink −36,864 → 直接變化 −20,480）在 shrink；
    // keep\inner（+8,192）在 growth；只是子孫變大或變小的上層目錄不會被列入
    let gd = paths_of(d, "growth", "dirs");
    assert_eq!(gd.len(), 1, "{gd:?}");
    assert_eq!(gd[0].0, p("keep\\inner"));
    // 根目錄的直接子檔案 removed.txt 消失（−4,096），所以根在 shrink 的第二位
    let sd = paths_of(d, "shrink", "dirs");
    assert_eq!(sd, [(p("keep"), -20_480), (root.clone(), -4_096)], "{sd:?}");
    // 與兩次掃描的根合計一致
    let ta = find_total(&sa);
    let tb = find_total(&sb);
    let t = d.get("totals");
    assert_eq!(num(t.get("a_physical")), ta);
    assert_eq!(num(t.get("b_physical")), tb);
    assert_eq!(num(t.get("delta_physical")), tb - ta);
    // 同一份快照比自己：沒有任何變化
    let (_, same) = diff_doc(&sa, &sa, &[]);
    let s = &same[0];
    for g in ["growth", "shrink", "added", "removed"] {
        for k in ["dirs", "files"] {
            assert!(paths_of(s, g, k).is_empty(), "{g}.{k}");
        }
    }
    assert_eq!(num(s.get("totals").get("delta_physical")), 0);
}

/// 快照中根目錄那一行的 `rollup.physical`。
fn find_total(snapshot: &Path) -> i128 {
    let text = std::fs::read_to_string(snapshot).unwrap();
    let first_dir = text
        .lines()
        .map(parse_json)
        .find(|l| l.get("type").str() == "dir")
        .unwrap();
    first_dir.get("rollup").get("physical").num()
}

#[test]
fn diff_filters_ranking_and_limits() {
    let r = fixture("m6_diff2");
    std::fs::create_dir_all(r.join("a").join("b")).unwrap();
    write(&r.join("a").join("b").join("f.bin"), 4_096);
    write(&r.join("a").join("g.bin"), 4_096);
    let sa = r.join("..").join("m6_d2_a.ndjson");
    let sb = r.join("..").join("m6_d2_b.ndjson");
    snap(&r, &sa);
    write(&r.join("a").join("b").join("f.bin"), 40_960); // +36,864
    write(&r.join("a").join("g.bin"), 8_192); // +4,096
    snap(&r, &sb);
    let root = r.to_string_lossy().to_string();

    // rollup 排序：祖先目錄也會出現；direct 排序：只有真正造成成長的目錄
    let (_, dr) = diff_doc(&sa, &sb, &["--rank", "rollup"]);
    let rolled: Vec<String> = paths_of(&dr[0], "growth", "dirs")
        .into_iter()
        .map(|x| x.0)
        .collect();
    assert!(rolled.contains(&root), "{rolled:?}");
    let (_, dd) = diff_doc(&sa, &sb, &["--rank", "direct"]);
    let direct: Vec<String> = paths_of(&dd[0], "growth", "dirs")
        .into_iter()
        .map(|x| x.0)
        .collect();
    assert!(!direct.contains(&root), "{direct:?}");
    // --top 1 → 截斷
    let (_, t1) = diff_doc(&sa, &sb, &["--top", "1"]);
    let files = t1[0].get("growth").get("files");
    assert_eq!(paths_of(&t1[0], "growth", "files").len(), 1);
    assert!(files.get("truncated").bool());
    // --min-change 過濾掉小變化
    let (_, mc) = diff_doc(&sa, &sb, &["--min-change", "10000"]);
    assert_eq!(
        paths_of(&mc[0], "growth", "files"),
        [(format!("{root}\\a\\b\\f.bin"), 36_864)]
    );
    // --under
    let (_, un) = diff_doc(&sa, &sb, &["--under", &format!("{root}\\a\\b*")]);
    let u: Vec<String> = paths_of(&un[0], "growth", "files")
        .into_iter()
        .map(|x| x.0)
        .collect();
    assert_eq!(u, [format!("{root}\\a\\b\\f.bin")]);
}

#[test]
fn ec22_snapshots_with_disjoint_roots_exit_1_and_partial_overlap_is_reported() {
    let base = fixture("m6_ec22");
    std::fs::create_dir_all(base.join("x")).unwrap();
    std::fs::create_dir_all(base.join("y")).unwrap();
    write(&base.join("x").join("f.bin"), 4_096);
    write(&base.join("y").join("g.bin"), 4_096);
    let (sx, sy, sall) = (
        base.join("..").join("m6_x.ndjson"),
        base.join("..").join("m6_y.ndjson"),
        base.join("..").join("m6_all.ndjson"),
    );
    snap(&base.join("x"), &sx);
    snap(&base.join("y"), &sy);
    snap(&base, &sall);
    let (bad, out) = diff_doc(&sx, &sy, &[]);
    assert_eq!(bad.code, 1, "{}", bad.stderr);
    assert!(out.is_empty());
    assert!(bad.stderr.contains("no root in common"), "{}", bad.stderr);
    // 一個根包含另一個：只比較重疊的範圍，並標明
    let (ok, l) = diff_doc(&sall, &sx, &[]);
    assert_eq!(ok.code, 0, "{}", ok.stderr);
    let ov = match l[0].get("overlap") {
        J::Arr(a) => a.iter().map(|x| x.str().to_string()).collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    assert_eq!(ov, [base.join("x").to_string_lossy().to_string()]);
    // 截斷的快照 → 1
    let text = std::fs::read_to_string(&sall).unwrap();
    let cut = base.join("..").join("m6_cut2.ndjson");
    std::fs::write(&cut, &text[..text.len() / 2]).unwrap();
    let (c, o2) = diff_doc(&sall, &cut, &[]);
    assert_eq!(c.code, 1);
    assert!(o2.is_empty());
    // 用法：少一個參數
    assert_eq!(run(&["diff", sall.to_str().unwrap()]).code, 1);
}

#[test]
fn diff_marks_trimmed_snapshots_as_limited() {
    let r = fixture("m6_diff_trim");
    write(&r.join("small.bin"), 10);
    write(&r.join("big.bin"), 40_000);
    let (sa, sb) = (
        r.join("..").join("m6_t_a.ndjson"),
        r.join("..").join("m6_t_b.ndjson"),
    );
    snap(&r, &sa);
    let sc = ds(&[
        "scan".as_ref(),
        r.as_os_str(),
        "-q".as_ref(),
        "--min-size".as_ref(),
        "1000".as_ref(),
        "--out".as_ref(),
        sb.as_os_str(),
    ]);
    assert_eq!(sc.code, 0);
    let (x, l) = diff_doc(&sa, &sb, &[]);
    assert_eq!(x.code, 0, "{}", x.stderr);
    assert!(l[0].get("limited_by_snapshot").bool());
    // 目錄層級的合計不受影響：兩份根的 rollup 相同
    assert_eq!(num(l[0].get("totals").get("delta_physical")), 0);
}
