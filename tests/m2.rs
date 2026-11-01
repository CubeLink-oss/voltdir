//! M2 整合測試：scan 選項、report、快照輸入。

mod common;

use common::*;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

fn ds(args: &[&str]) -> Run {
    let v: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    voltdir(&v)
}

fn p(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn write(path: &Path, bytes: usize) {
    std::fs::write(path, vec![b'x'; bytes]).unwrap();
}

/// 建立一棵有不同大小、深度與副檔名的樹。
fn sample_tree(name: &str) -> PathBuf {
    let r = fixture(name);
    for d in ["a", "a/b", "a/b/c", "empty", "many"] {
        std::fs::create_dir_all(r.join(d)).unwrap();
    }
    write(&r.join("big.iso"), 3_000_000);
    write(&r.join("mid.zip"), 300_000);
    write(&r.join("small.txt"), 500);
    write(&r.join("a").join("x.iso"), 1_200_000);
    write(&r.join("a/b").join("y.bin"), 40_000);
    write(&r.join("a/b/c").join("deep.log"), 70_000);
    write(&r.join("a/b/c").join("tiny.log"), 3);
    for i in 0..12 {
        write(&r.join("many").join(format!("f{i:02}.dat")), 100 + i);
    }
    r
}

fn parent_of(path: &str) -> String {
    path.rsplit_once('\\')
        .map(|x| x.0.to_string())
        .unwrap_or_default()
}

fn scan_to(r: &Path, extra: &[&str], out: &Path) -> Run {
    let mut a = vec!["scan"];
    let rs = p(r);
    a.push(&rs);
    a.extend_from_slice(extra);
    let os = p(out);
    a.extend_from_slice(&["-q", "--out", &os]);
    ds(&a)
}

fn read_lines(path: &Path) -> Vec<J> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(parse_json)
        .collect()
}

fn dir_map(lines: &[J]) -> HashMap<String, &J> {
    lines
        .iter()
        .filter(|l| l.get("type").str() == "dir")
        .map(|l| (l.get("path").str().to_lowercase(), l))
        .collect()
}

#[test]
fn ec13_min_size_keeps_rollups_and_accounts_for_omitted() {
    let r = sample_tree("m2_minsize");
    let out = r.parent().unwrap().join("m2_minsize_full.ndjson");
    let out2 = r.parent().unwrap().join("m2_minsize_cut.ndjson");
    assert_eq!(scan_to(&r, &[], &out).code, 0);
    assert_eq!(scan_to(&r, &["--min-size", "1KiB"], &out2).code, 0);
    let (full, cut) = (read_lines(&out), read_lines(&out2));
    let (fd, cd) = (dir_map(&full), dir_map(&cut));
    assert_eq!(fd.len(), cd.len(), "all directories are still listed");
    // 已輸出的直接子檔案合計（依父目錄分組）
    let mut emitted: HashMap<String, (i128, i128, i128)> = HashMap::new();
    for f in cut.iter().filter(|l| l.get("type").str() == "file") {
        assert!(
            f.get("size_physical").num() >= 1024,
            "small files must be omitted"
        );
        let e = emitted
            .entry(parent_of(f.get("path").str()).to_lowercase())
            .or_default();
        e.0 += 1;
        e.1 += f.get("size_logical").num();
        e.2 += if f.get("hardlink_secondary").bool() {
            0
        } else {
            f.get("size_physical").num()
        };
    }
    let mut omitted_total = 0;
    for (k, full_dir) in &fd {
        let cut_dir = cd[k];
        for field in ["files", "dirs", "logical", "physical"] {
            assert_eq!(
                full_dir.get("rollup").get(field).num(),
                cut_dir.get("rollup").get(field).num(),
                "{k} rollup.{field}"
            );
        }
        let e = emitted.get(k).copied().unwrap_or_default();
        let om = cut_dir.get("omitted");
        assert_eq!(
            om.get("files").num() + e.0,
            cut_dir.get("direct").get("files").num(),
            "{k} files"
        );
        assert_eq!(
            om.get("logical").num() + e.1,
            cut_dir.get("direct").get("logical").num(),
            "{k} logical"
        );
        assert_eq!(
            om.get("physical").num() + e.2,
            cut_dir.get("direct").get("physical").num(),
            "{k} physical"
        );
        assert_eq!(
            full_dir.get("omitted").get("files").num(),
            0,
            "full scan omits nothing"
        );
        omitted_total += om.get("files").num();
    }
    assert!(
        omitted_total >= 14,
        "tiny.log, small.txt-sized and many/* files are omitted: {omitted_total}"
    );
    assert_eq!(cut[0].get("options").get("min_size").num(), 1024);
}

#[test]
fn max_depth_trims_output_but_not_rollups() {
    let r = sample_tree("m2_depth");
    let out = r.parent().unwrap().join("m2_depth_full.ndjson");
    let out2 = r.parent().unwrap().join("m2_depth_cut.ndjson");
    assert_eq!(scan_to(&r, &[], &out).code, 0);
    assert_eq!(scan_to(&r, &["--max-depth", "1"], &out2).code, 0);
    let (full, cut) = (read_lines(&out), read_lines(&out2));
    for l in cut
        .iter()
        .filter(|l| matches!(l.get("type").str(), "dir" | "file"))
    {
        assert!(l.get("depth").num() <= 1, "{}", l.get("path").str());
    }
    let (fd, cd) = (dir_map(&full), dir_map(&cut));
    assert!(cd.len() < fd.len());
    for (k, d) in &cd {
        assert_eq!(d.get("rollup"), fd[k].get("rollup"), "{k}");
    }
    // 摘要中的總數不受剪裁影響
    assert_eq!(
        summary(&cut).get("entries").get("dirs"),
        summary(&full).get("entries").get("dirs")
    );
    assert_eq!(
        summary(&cut).get("entries").get("files"),
        summary(&full).get("entries").get("files")
    );
    assert!(
        summary(&cut).get("entries").get("emitted").num()
            < summary(&full).get("entries").get("emitted").num()
    );
    // 深度 0：只剩根目錄
    assert_eq!(scan_to(&r, &["--max-depth", "0"], &out2).code, 0);
    let only_root = read_lines(&out2);
    assert_eq!(
        only_root
            .iter()
            .filter(|l| l.get("type").str() != "meta" && l.get("type").str() != "summary")
            .count(),
        1
    );
}

#[test]
fn exclude_lists_directories_without_entering_and_drops_files() {
    let r = sample_tree("m2_exclude");
    write(&r.join("junk.tmp"), 777);
    write(&r.join("a").join("more.tmp"), 888);
    let run = ds(&[
        "scan",
        &p(&r),
        "-q",
        "--exclude",
        r"*\a\b",
        "--exclude",
        "*.TMP",
    ]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    let l = run.lines();
    let b = find(&l, "dir", &r.join("a").join("b"));
    assert_eq!(b.get("scan").str(), "excluded");
    assert_eq!(b.get("rollup").get("files").num(), 0);
    assert!(l.iter().filter(|x| x.get("type").str() == "file").all(|x| {
        let path = x.get("path").str().to_lowercase();
        !path.contains(r"\a\b\") && !path.ends_with(".tmp")
    }));
    let full = ds(&["scan", &p(&r), "-q"]);
    let root_full = find(&full.lines(), "dir", &r)
        .get("rollup")
        .get("logical")
        .num();
    let root = find(&l, "dir", &r).get("rollup").get("logical").num();
    assert_eq!(
        root_full - root,
        40_000 + 70_000 + 3 + 777 + 888,
        "excluded subtree and .tmp files are not counted"
    );
    assert_eq!(
        l[0].get("options").get("exclude").clone(),
        J::Arr(vec![J::Str(r"*\a\b".into()), J::Str("*.TMP".into())])
    );
    // excluded 不算「沒讀到」
    assert_eq!(summary(&l).get("coverage").get("incomplete_dirs").num(), 0);
}

fn mklink_j(link: &Path, target: &Path) {
    let st = Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .unwrap();
    assert!(st.status.success(), "mklink /J failed: {st:?}");
}

#[test]
fn ec06_junction_cycle_terminates_and_is_reported() {
    let r = fixture("m2_cycle");
    std::fs::create_dir_all(r.join("a")).unwrap();
    std::fs::create_dir_all(r.join("other")).unwrap();
    write(&r.join("a").join("f.txt"), 10);
    write(&r.join("other").join("o.txt"), 20);
    mklink_j(&r.join("a").join("loop"), &r.join("a")); // 指回自己的父目錄
    mklink_j(&r.join("a").join("to_other"), &r.join("other")); // 一般連結
    // 預設：不進入任何連結
    let d = ds(&["scan", &p(&r), "-q"]);
    assert_eq!(d.code, 0, "{}", d.stderr);
    let l = d.lines();
    assert_eq!(
        find(&l, "dir", &r.join("a").join("loop")).get("scan").str(),
        "not_followed"
    );
    assert_eq!(find(&l, "dir", &r).get("rollup").get("files").num(), 2);
    // 跟隨 junction：必須結束（以逾時當保險），迴圈目錄標示原因，其餘連結目標被計入
    let f = ds(&[
        "scan",
        &p(&r),
        "-q",
        "--follow-links",
        "junction",
        "--timeout-sec",
        "30",
    ]);
    assert_eq!(f.code, 0, "must finish without timing out: {}", f.stderr);
    let l = f.lines();
    let lp = find(&l, "dir", &r.join("a").join("loop"));
    assert_eq!(lp.get("scan").str(), "not_followed");
    assert_eq!(lp.get("error").get("code").num(), 1921);
    let to = find(&l, "dir", &r.join("a").join("to_other"));
    assert_eq!(to.get("scan").str(), "complete");
    assert_eq!(to.get("rollup").get("logical").num(), 20);
    assert_eq!(
        find(&l, "dir", &r).get("rollup").get("files").num(),
        3,
        "f.txt, o.txt, and o.txt again via the link"
    );
    assert_eq!(
        l[0].get("options").get("follow_links").clone(),
        J::Arr(vec![J::Str("junction".into())])
    );
    assert_eq!(summary(&l).get("coverage").get("incomplete_dirs").num(), 0);
    let _ = std::fs::remove_dir(r.join("a").join("loop"));
    let _ = std::fs::remove_dir(r.join("a").join("to_other"));
}

#[test]
fn hidden_system_reparse_dirs_are_never_followed() {
    let r = fixture("m2_hs");
    std::fs::create_dir_all(r.join("target")).unwrap();
    write(&r.join("target").join("t.txt"), 5);
    let link = r.join("hs_link");
    mklink_j(&link, &r.join("target"));
    let st = Command::new("attrib")
        .args(["+h", "+s", "/L"])
        .arg(&link)
        .output()
        .unwrap();
    assert!(st.status.success(), "{st:?}");
    let f = ds(&[
        "scan",
        &p(&r),
        "-q",
        "--follow-links",
        "junction,symlink,mount",
    ]);
    assert_eq!(f.code, 0, "{}", f.stderr);
    let l = f.lines();
    let e = find(&l, "dir", &link);
    assert_eq!(e.get("scan").str(), "not_followed");
    assert_eq!(e.get("rollup").get("files").num(), 0);
    assert_eq!(find(&l, "dir", &r).get("rollup").get("files").num(), 1);
    let _ = Command::new("attrib")
        .args(["-h", "-s", "/L"])
        .arg(&link)
        .output();
    let _ = std::fs::remove_dir(&link);
}

#[test]
fn symlinks_listed_not_entered_by_default_and_entered_when_asked() {
    let r = fixture("m2_symlink");
    std::fs::create_dir_all(r.join("target")).unwrap();
    write(&r.join("target").join("t.txt"), 5);
    let link = r.join("dirlink");
    let st = Command::new("cmd")
        .args(["/c", "mklink", "/D"])
        .arg(&link)
        .arg(r.join("target"))
        .output()
        .unwrap();
    if !st.status.success() {
        eprintln!("UNVERIFIED: cannot create directory symlinks here: {st:?}");
        return;
    }
    write(&r.join("real.txt"), 7);
    let fl = Command::new("cmd")
        .args(["/c", "mklink"])
        .arg(r.join("filelink.txt"))
        .arg(r.join("real.txt"))
        .output()
        .unwrap();
    let have_file_link = fl.status.success();
    let d = ds(&["scan", &p(&r), "-q"]);
    let l = d.lines();
    let e = find(&l, "dir", &link);
    assert_eq!(
        (e.get("reparse").str(), e.get("scan").str()),
        ("symlink", "not_followed")
    );
    if have_file_link {
        assert!(
            l.iter().all(|x| x.get("type").str() == "meta"
                || x.get("type").str() == "summary"
                || !x.get("path").str().ends_with("filelink.txt")),
            "file symlinks are omitted"
        );
    }
    let f = ds(&["scan", &p(&r), "-q", "--follow-links", "symlink"]);
    let l = f.lines();
    let e = find(&l, "dir", &link);
    assert_eq!(e.get("scan").str(), "complete");
    assert_eq!(e.get("rollup").get("logical").num(), 5);
    let _ = std::fs::remove_dir(&link);
}

#[test]
fn ec15_timeout_gives_partial_output_cancelled_dirs_and_exit_5() {
    // 單執行緒掃描 C:\Windows（唯讀），1 秒後必然尚未完成
    let run = ds(&[
        "scan",
        r"C:\Windows",
        "-q",
        "--threads",
        "1",
        "--timeout-sec",
        "1",
    ]);
    assert_eq!(run.code, 5, "stderr: {}", run.stderr);
    let l = run.lines(); // 部分結果仍是完整、合法的 NDJSON，且以 summary 結尾
    let cancelled = l
        .iter()
        .filter(|x| x.get("type").str() == "dir" && x.get("scan").str() == "cancelled")
        .count();
    assert!(cancelled > 0, "some directories must be marked cancelled");
    let cov = summary(&l).get("coverage");
    assert!(cov.get("incomplete_dirs").num() >= cancelled as i128);
    assert_eq!(l[0].get("options").get("timeout_sec").num(), 1);
}

#[test]
fn ec18_overlapping_roots_are_counted_once() {
    let r = sample_tree("m2_roots");
    let single = ds(&["scan", &p(&r), "-q"]).lines();
    let sub = r.join("a");
    let multi = ds(&["scan", &p(&r), &p(&sub), &p(&r), "-q"]);
    assert_eq!(multi.code, 0, "{}", multi.stderr);
    let l = multi.lines();
    assert_eq!(l.len(), single.len(), "same entries, nothing duplicated");
    match l[0].get("roots") {
        J::Arr(a) => assert_eq!(a.len(), 1),
        other => panic!("roots: {other:?}"),
    }
    assert_eq!(
        summary(&l).get("totals").get("files"),
        summary(&single).get("totals").get("files")
    );
    // 兩個不相關的根仍然都掃
    let other = fixture("m2_roots_other");
    write(&other.join("z.txt"), 1);
    let two = ds(&["scan", &p(&r), &p(&other), "-q"]).lines();
    assert_eq!(
        summary(&two).get("totals").get("files").num(),
        summary(&single).get("totals").get("files").num() + 1
    );
}

#[test]
fn hardlinks_none_counts_every_name() {
    let r = fixture("m2_hl_none");
    write(&r.join("a.bin"), 10_000);
    std::fs::hard_link(r.join("a.bin"), r.join("b.bin")).unwrap();
    let dedupe = ds(&["scan", &p(&r), "-q"]).lines();
    let none = ds(&["scan", &p(&r), "-q", "--hardlinks", "none"]).lines();
    assert_eq!(
        find(&dedupe, "dir", &r).get("rollup").get("physical").num(),
        12_288
    );
    assert_eq!(
        find(&none, "dir", &r).get("rollup").get("physical").num(),
        24_576
    );
    assert!(
        none.iter()
            .filter(|x| x.get("type").str() == "file")
            .all(|x| !x.get("hardlink_secondary").bool())
    );
    assert_eq!(none[0].get("options").get("hardlinks").str(), "none");
}

// ---------------- report ----------------

fn report_json(args: &[&str]) -> (i32, J, String) {
    let run = ds(args);
    let text = run.text();
    let j = if text.trim().is_empty() {
        J::Null
    } else {
        parse_json(text.trim())
    };
    (run.code, j, run.stderr)
}

fn items(j: &J, which: &str) -> Vec<J> {
    match j.get("reports").get(which).get("items") {
        J::Arr(a) => a.clone(),
        other => panic!("{which}: {other:?}"),
    }
}

#[test]
fn report_from_snapshot_equals_live_report() {
    let r = sample_tree("m2_report");
    let snap = r.parent().unwrap().join("m2_report.ndjson");
    assert_eq!(scan_to(&r, &[], &snap).code, 0);
    let flags = [
        "--top-files",
        "5",
        "--top-dirs",
        "5",
        "--by-ext",
        "5",
        "--many-files",
        "3",
        "-q",
    ];
    let mut live_args = vec!["report", &*Box::leak(p(&r).into_boxed_str())];
    live_args.extend_from_slice(&flags);
    let (c1, live, e1) = report_json(&live_args);
    let sp = p(&snap);
    let mut snap_args = vec!["report", "--snapshot", sp.as_str()];
    snap_args.extend_from_slice(&flags);
    let (c2, from_snap, e2) = report_json(&snap_args);
    assert_eq!((c1, c2), (0, 0), "{e1} {e2}");
    assert_eq!(live.get("reports"), from_snap.get("reports"));
    assert_eq!(live.get("totals"), from_snap.get("totals"));
    assert_eq!(live.get("coverage"), from_snap.get("coverage"));
    assert_eq!(live.get("schema").str(), "voltdir.report/1");
    assert!(!live.get("limited_by_snapshot").bool());
    assert!(!from_snap.get("limited_by_snapshot").bool());
    assert_eq!(live.get("meta").get("source").str(), "scan");
    assert_eq!(from_snap.get("meta").get("source").str(), "snapshot");
    // 內容抽查
    let tf = items(&live, "top_files");
    assert_eq!(
        tf[0].get("path").str().rsplit('\\').next().unwrap(),
        "big.iso"
    );
    assert_eq!(tf[0].get("size_logical").num(), 3_000_000);
    assert!(live.get("reports").get("top_files").get("truncated").bool());
    let td = items(&live, "top_dirs");
    assert_eq!(td[0].get("depth").num(), 0);
    let mf = items(&live, "many_files");
    assert!(mf[0].get("path").str().ends_with("\\many"));
    assert_eq!(mf[0].get("direct_files").num(), 12);
    let be = items(&live, "by_ext");
    assert_eq!(be[0].get("ext").str(), ".iso");
    assert_eq!(be[0].get("files").num(), 2);
}

#[test]
fn report_filters_ext_min_size_and_age() {
    let r = sample_tree("m2_filters");
    // 讓 a\x.iso 變成 400 天前
    let old = SystemTime::now() - Duration::from_secs(400 * 86_400);
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(r.join("a").join("x.iso"))
        .unwrap();
    f.set_modified(old).unwrap();
    drop(f);
    let rs = p(&r);
    let (c, j, e) = report_json(&[
        "report",
        &rs,
        "-q",
        "--top-files",
        "10",
        "--ext",
        ".ISO",
        "--older-than",
        "365",
    ]);
    assert_eq!(c, 0, "{e}");
    let it = items(&j, "top_files");
    assert_eq!(it.len(), 1, "{it:?}");
    assert!(it[0].get("path").str().ends_with("x.iso"));
    assert!(!j.get("reports").get("top_files").get("truncated").bool());
    let (_, j, _) = report_json(&["report", &rs, "-q", "--top-files", "10", "--min-size", "1M"]);
    let names: Vec<String> = items(&j, "top_files")
        .iter()
        .map(|x| x.get("path").str().rsplit('\\').next().unwrap().to_string())
        .collect();
    assert_eq!(names, ["big.iso", "x.iso"]);
    let (_, j, _) = report_json(&["report", &rs, "-q", "--top-dirs", "10", "--dir-depth", "1"]);
    assert!(
        items(&j, "top_dirs")
            .iter()
            .all(|x| x.get("depth").num() <= 1)
    );
}

#[test]
fn ec19_trimmed_snapshots_flag_limits_only_where_results_are_affected() {
    let r = sample_tree("m2_limited");
    let dir = r.parent().unwrap().to_path_buf();
    let (full, cut, shallow) = (
        dir.join("m2_l_full.ndjson"),
        dir.join("m2_l_min.ndjson"),
        dir.join("m2_l_depth.ndjson"),
    );
    assert_eq!(scan_to(&r, &[], &full).code, 0);
    assert_eq!(scan_to(&r, &["--min-size", "1M"], &cut).code, 0);
    assert_eq!(scan_to(&r, &["--max-depth", "1"], &shallow).code, 0);
    let (fp, cp, sp) = (p(&full), p(&cut), p(&shallow));
    let dirs_only = |s: &str| {
        report_json(&[
            "report",
            "--snapshot",
            s,
            "-q",
            "--top-dirs",
            "20",
            "--many-files",
            "20",
        ])
    };
    let (c0, j_full, _) = dirs_only(&fp);
    let (c1, j_cut, _) = dirs_only(&cp);
    assert_eq!((c0, c1), (0, 0));
    // min-size 不影響目錄層級的結果，也不該被標為受限
    assert_eq!(j_full.get("reports"), j_cut.get("reports"));
    assert!(!j_cut.get("limited_by_snapshot").bool());
    // 檔案層級：受限且說明原因
    let (_, j, _) = report_json(&[
        "report",
        "--snapshot",
        &cp,
        "-q",
        "--top-files",
        "10",
        "--by-ext",
        "10",
    ]);
    assert!(j.get("limited_by_snapshot").bool());
    match j.get("limited_reasons") {
        J::Arr(a) => assert!(a[0].str().contains("--min-size")),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        items(&j, "top_files").len(),
        2,
        "only the files >= 1MB are visible"
    );
    // max-depth：目錄層級也受限
    let (_, j, _) = report_json(&["report", "--snapshot", &sp, "-q", "--top-dirs", "10"]);
    assert!(j.get("limited_by_snapshot").bool());
    // 總計與涵蓋範圍不受剪裁影響
    let (_, jf, _) = report_json(&["report", "--snapshot", &fp, "-q", "--top-dirs", "10"]);
    assert_eq!(j.get("totals"), jf.get("totals"));
}

#[test]
fn ec20_bad_snapshots_exit_1_with_no_output() {
    let r = sample_tree("m2_badsnap");
    let dir = r.parent().unwrap().to_path_buf();
    let good = dir.join("m2_bad_good.ndjson");
    assert_eq!(scan_to(&r, &[], &good).code, 0);
    let text = std::fs::read_to_string(&good).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    let cases: Vec<(&str, String)> = vec![
        ("missing summary", lines[..lines.len() - 1].join("\n")),
        (
            "cut in the middle of a line",
            text[..text.len() / 2].to_string(),
        ),
        (
            "garbage line",
            format!("{}\nnot json\n{}", lines[0], lines[lines.len() - 1]),
        ),
        (
            "wrong schema",
            text.replace("voltdir.snapshot/1", "voltdir.snapshot/7"),
        ),
        ("empty file", String::new()),
        ("no meta first", lines[1..].join("\n")),
        ("data after summary", format!("{text}{}\n", lines[1])),
    ];
    for (name, body) in cases {
        let f = dir.join("m2_bad_case.ndjson");
        std::fs::write(&f, body).unwrap();
        let run = ds(&["report", "--snapshot", &p(&f), "--top-files", "3", "-q"]);
        assert_eq!(run.code, 1, "{name}: {}", run.stderr);
        assert!(run.stdout.is_empty(), "{name}: no partial output allowed");
        assert!(
            run.stderr.contains("invalid snapshot"),
            "{name}: {}",
            run.stderr
        );
    }
    let missing = ds(&[
        "report",
        "--snapshot",
        &p(&dir.join("does_not_exist.ndjson")),
        "--top-files",
        "3",
    ]);
    assert_eq!(missing.code, 1);
}

#[test]
fn report_usage_errors() {
    let r = sample_tree("m2_usage");
    let rs = p(&r);
    assert_eq!(ds(&["report", &rs]).code, 1, "no analysis");
    assert_eq!(ds(&["report", &rs, "--top-files", "0"]).code, 1);
    assert_eq!(ds(&["report", &rs, "--top-files", "1001"]).code, 1);
    assert_eq!(ds(&["report", "--top-files", "5"]).code, 1, "no source");
    assert_eq!(
        ds(&["report", "--snapshot", "x.ndjson", &rs, "--top-files", "5"]).code,
        1
    );
    assert_eq!(
        ds(&[
            "report",
            "--snapshot",
            "x.ndjson",
            "--top-files",
            "5",
            "--threads",
            "2"
        ])
        .code,
        1
    );
    assert_eq!(
        ds(&["report", &rs, "--top-files", "5", "--engine", "mft"]).code,
        3
    );
    assert_eq!(ds(&["dupes", &rs]).code, 1);
}

#[test]
fn report_out_file_is_written_atomically_and_stdout_stays_empty() {
    let r = sample_tree("m2_reportout");
    let out = r.parent().unwrap().join("m2_reportout.json");
    let run = ds(&[
        "report",
        &p(&r),
        "--top-files",
        "2",
        "-q",
        "--out",
        &p(&out),
    ]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(run.stdout.is_empty());
    let j = parse_json(std::fs::read_to_string(&out).unwrap().trim());
    assert_eq!(j.get("schema").str(), "voltdir.report/1");
    let bad = r.join("nope").join("r.json");
    assert_eq!(
        ds(&[
            "report",
            &p(&r),
            "--top-files",
            "2",
            "-q",
            "--out",
            &p(&bad)
        ])
        .code,
        4
    );
}

#[test]
fn report_propagates_incomplete_coverage_as_exit_2() {
    let r = fixture("m2_incomplete");
    let locked = r.join("locked");
    std::fs::create_dir(&locked).unwrap();
    write(&locked.join("s.txt"), 10);
    write(&r.join("v.txt"), 5);
    let user = std::env::var("USERNAME").unwrap();
    struct Guard(PathBuf, String);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = Command::new("icacls")
                .arg(&self.0)
                .args(["/remove:d", &self.1])
                .output();
            let _ = Command::new("icacls")
                .arg(&self.0)
                .args(["/grant", &format!("{}:(OI)(CI)F", self.1)])
                .output();
        }
    }
    let _g = Guard(locked.clone(), user.clone());
    let o = Command::new("icacls")
        .arg(&locked)
        .args(["/deny", &format!("{user}:(RD)")])
        .output()
        .unwrap();
    assert!(o.status.success());
    let snap = r.parent().unwrap().join("m2_incomplete.ndjson");
    assert_eq!(scan_to(&r, &[], &snap).code, 2);
    let (c, j, _) = report_json(&["report", &p(&r), "-q", "--top-dirs", "5"]);
    assert_eq!(c, 2);
    assert_eq!(j.get("coverage").get("incomplete_dirs").num(), 1);
    let (c, j2, _) = report_json(&["report", "--snapshot", &p(&snap), "-q", "--top-dirs", "5"]);
    assert_eq!(c, 2, "a snapshot with unread directories must also warn");
    assert_eq!(j.get("coverage"), j2.get("coverage"));
}

#[test]
fn scan_meta_records_all_effective_options() {
    let r = sample_tree("m2_meta");
    let run = ds(&[
        "scan",
        &p(&r),
        "-q",
        "--threads",
        "3",
        "--min-size",
        "2K",
        "--max-depth",
        "4",
        "--hardlinks",
        "dedupe",
        "--timeout-sec",
        "600",
    ]);
    let l = run.lines();
    let o = l[0].get("options");
    assert_eq!(o.get("threads").num(), 3);
    assert_eq!(o.get("min_size").num(), 2000);
    assert_eq!(o.get("max_depth").num(), 4);
    assert_eq!(o.get("hardlinks").str(), "dedupe");
    assert_eq!(o.get("timeout_sec").num(), 600);
}
