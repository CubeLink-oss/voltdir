//! `overview`。

mod common;

use common::*;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

fn write(p: &Path, bytes: usize) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, vec![b'x'; bytes]).unwrap();
}

fn run(args: &[&str]) -> Run {
    let v: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    voltdir(&v)
}

fn one(r: &Run) -> J {
    assert_eq!(r.code, 0, "{}", r.stderr);
    let l = r.lines();
    assert_eq!(l.len(), 1, "one JSON object, one line");
    l.into_iter().next().unwrap()
}

fn items(j: &J, list: &str) -> Vec<J> {
    match j.get(list).get("items") {
        J::Arr(a) => a.clone(),
        other => panic!("{list}: {other:?}"),
    }
}

/// 一棵小樹：大小不一的檔案、空檔案、空資料夾、副檔名各異、一個舊的大檔。
fn tree(name: &str) -> PathBuf {
    let r = fixture(name);
    write(&r.join("media").join("a.mp4"), 65_536);
    write(&r.join("media").join("b.mp4"), 32_768);
    write(&r.join("media").join("raw").join("c.mp4"), 16_384);
    write(&r.join("src").join("x.rs"), 4_096);
    write(&r.join("src").join("y.RS"), 8_192);
    write(&r.join("src").join("Makefile"), 4_096);
    write(&r.join("logs").join("a.log"), 40_960);
    write(&r.join("empty.txt"), 0);
    write(&r.join("src").join("zero.rs"), 0);
    std::fs::create_dir_all(r.join("void")).unwrap();
    std::fs::create_dir_all(r.join("src").join("also_void")).unwrap();
    r
}

#[test]
fn t_ag_08_numbers_match_report_and_find() {
    let r = tree("m11_cross");
    let ov = one(&run(&[
        "overview",
        r.to_str().unwrap(),
        "-q",
        "--detail",
        "full",
    ]));
    let rp = one(&run(&[
        "report",
        r.to_str().unwrap(),
        "-q",
        "--top-files",
        "10",
        "--top-dirs",
        "10",
        "--by-ext",
        "8",
    ]));
    assert_eq!(rp.get("totals"), ov.get("totals"));
    // top_files（absolute 路徑，--detail full）
    let (of, rf) = (
        items(&ov, "top_files"),
        items(rp.get("reports"), "top_files"),
    );
    assert_eq!(of.len(), rf.len());
    for (a, b) in of.iter().zip(&rf) {
        assert_eq!(a.get("path"), b.get("path"));
        assert_eq!(a.get("physical"), b.get("size_physical"));
        assert_eq!(a.get("logical"), b.get("size_logical"));
        // overview 的時間只到秒
        let want = b.get("mtime").str();
        let want = want.split('.').next().unwrap();
        assert!(a.get("modified").str().starts_with(want), "{want}");
    }
    // top_dirs：report 的第一項是掃描根本身，overview 不含
    let od = items(&ov, "top_dirs");
    let rd: Vec<J> = items(rp.get("reports"), "top_dirs")
        .into_iter()
        .filter(|d| d.get("depth").num() > 0)
        .collect();
    assert!(!od.is_empty());
    for (a, b) in od.iter().zip(&rd) {
        assert_eq!(a.get("path"), b.get("path"));
        assert_eq!(a.get("physical"), b.get("rollup").get("physical"));
        assert_eq!(a.get("logical"), b.get("rollup").get("logical"));
        assert_eq!(a.get("files"), b.get("rollup").get("files"));
    }
    // by_ext
    let (oe, re) = (items(&ov, "by_ext"), items(rp.get("reports"), "by_ext"));
    assert_eq!(oe, re);
    // empties 與 find --empty
    let count = |ty: &str| {
        let f = one(&run(&[
            "find",
            r.to_str().unwrap(),
            "-q",
            "--empty",
            "--type",
            ty,
            "--limit",
            "0",
        ]));
        f.get("matched_total").num()
    };
    assert_eq!(ov.get("empties").get("files").num(), count("file"));
    assert_eq!(ov.get("empties").get("dirs").num(), count("dir"));
    assert_eq!(ov.get("empties").get("files").num(), 2);
    assert_eq!(ov.get("empties").get("dirs").num(), 2);
}

#[test]
fn t_ag_08_rank_orders_by_the_requested_size_and_lists_both() {
    // 稀疏檔：邏輯大小大、實際佔用小（NTFS 會省下未寫入的叢集）
    let r = fixture("m11_rank");
    write(&r.join("dense.bin"), 65_536);
    let sparse = r.join("sparse.bin");
    write(&sparse, 4_096);
    let ok = Command::new("fsutil")
        .args(["sparse", "setflag"])
        .arg(&sparse)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if ok {
        let _ = Command::new("fsutil")
            .args(["file", "setzerodata", "offset=0", "length=4096"])
            .arg(&sparse)
            .output();
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&sparse)
            .unwrap();
        f.set_len(10_000_000).unwrap();
    }
    let path = |rank: &str| {
        let j = one(&run(&[
            "overview",
            r.to_str().unwrap(),
            "-q",
            "--rank",
            rank,
        ]));
        assert_eq!(j.get("rank").str(), rank);
        let first = items(&j, "top_files")[0].clone();
        // 兩種大小都在
        assert!(first.get("logical").num() > 0);
        let _ = first.get("physical").num();
        first.get("path").str().to_string()
    };
    assert_eq!(path("physical"), "dense.bin");
    if ok {
        assert_eq!(path("logical"), "sparse.bin");
    }
}

#[test]
fn t_ag_08_path_base_detail_and_meta() {
    let r = tree("m11_base");
    let other = fixture("m11_base2");
    write(&other.join("o.bin"), 4_096);
    let s = one(&run(&["overview", r.to_str().unwrap(), "-q"]));
    assert_eq!(s.get("path_base").str(), "relative");
    assert!(s.get("meta").is_null(), "concise has no meta");
    assert!(
        items(&s, "top_files")
            .iter()
            .all(|f| !f.get("path").str().contains(':'))
    );
    let two = one(&run(&[
        "overview",
        r.to_str().unwrap(),
        other.to_str().unwrap(),
        "-q",
    ]));
    assert_eq!(two.get("path_base").str(), "absolute");
    assert!(items(&two, "top_files")[0].get("path").str().contains(':'));
    let full = one(&run(&[
        "overview",
        r.to_str().unwrap(),
        "-q",
        "--detail",
        "full",
    ]));
    assert_eq!(full.get("path_base").str(), "absolute");
    assert!(!full.get("meta").is_null());
}

#[test]
fn t_ag_08_save_snapshot_round_trips_through_report_and_conflicts_with_snapshot() {
    let r = tree("m11_save");
    let base = fixture("m11_save_out");
    let file = base.join("s.ndjson");
    let live = one(&run(&[
        "overview",
        r.to_str().unwrap(),
        "-q",
        "--save-snapshot",
        file.to_str().unwrap(),
    ]));
    let rp = one(&run(&[
        "report",
        "--snapshot",
        file.to_str().unwrap(),
        "-q",
        "--top-files",
        "10",
    ]));
    assert_eq!(rp.get("totals"), live.get("totals"));
    // 從快照再問一次 overview：數字與即時掃描相同
    let again = one(&run(&[
        "overview",
        "--snapshot",
        file.to_str().unwrap(),
        "-q",
    ]));
    for k in [
        "totals",
        "top_dirs",
        "top_files",
        "by_ext",
        "stale_large",
        "empties",
        "coverage",
        "path_base",
    ] {
        assert_eq!(live.get(k), again.get(k), "{k}");
    }
    // 有快照時 next 用該快照
    let J::Arr(next) = live.get("next") else {
        panic!()
    };
    assert!(
        next.iter().all(|n| n.str().contains("--snapshot")),
        "{next:?}"
    );
    // 互斥
    let bad = run(&[
        "overview",
        "--snapshot",
        file.to_str().unwrap(),
        "--save-snapshot",
        base.join("t.ndjson").to_str().unwrap(),
        "--log",
        "json",
    ]);
    assert_eq!(bad.code, 1);
    assert!(bad.stdout.is_empty());
    assert!(parse_json(bad.stderr.trim()).get("hint").str().len() > 3);
    assert!(!base.join("t.ndjson").exists());
    // 父資料夾不存在：掃描前就失敗，不留檔案
    let missing = base.join("no_such").join("s.ndjson");
    let r2 = run(&[
        "overview",
        r.to_str().unwrap(),
        "-q",
        "--save-snapshot",
        missing.to_str().unwrap(),
    ]);
    assert_eq!(r2.code, 4);
    assert!(r2.stdout.is_empty());
}

#[test]
fn t_ag_08_next_commands_run_as_written() {
    let r = tree("m11_next");
    let base = fixture("m11_next_out");
    let file = base.join("s.ndjson");
    let ov = one(&run(&[
        "overview",
        r.to_str().unwrap(),
        "-q",
        "--save-snapshot",
        file.to_str().unwrap(),
    ]));
    let J::Arr(next) = ov.get("next") else {
        panic!()
    };
    let kinds: Vec<&str> = next.iter().map(|n| n.str()).collect();
    assert!(kinds.iter().any(|n| n.starts_with("voltdir find ")));
    assert!(kinds.iter().any(|n| n.starts_with("voltdir report ")));
    for cmd in &kinds {
        let args = split_args(cmd);
        assert_eq!(args[0], "voltdir");
        let refs: Vec<&str> = args[1..].iter().map(String::as_str).collect();
        let res = run(&refs);
        assert_eq!(res.code, 0, "{cmd}: {}", res.stderr);
        assert!(!res.stdout.is_empty(), "{cmd}");
    }
    // 沒有快照時：命令用掃描根，且有一條 --save-snapshot
    let live = one(&run(&["overview", r.to_str().unwrap(), "-q"]));
    let J::Arr(n2) = live.get("next") else {
        panic!()
    };
    assert!(n2.iter().any(|n| n.str().contains("--save-snapshot")));
    let find_cmd = n2
        .iter()
        .find(|n| n.str().starts_with("voltdir find"))
        .unwrap();
    let args = split_args(find_cmd.str());
    let refs: Vec<&str> = args[1..].iter().map(String::as_str).collect();
    assert_eq!(run(&refs).code, 0);
}

/// 把 `voltdir a "b c" d` 拆成參數（只處理雙引號；Windows 路徑沒有引號字元）。
fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in s.chars() {
        match c {
            '"' => quoted = !quoted,
            ' ' if !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[test]
fn t_ag_08_concise_output_stays_under_4_kib_with_defaults() {
    let r = fixture("m11_size");
    for d in 0..30 {
        for f in 0..5 {
            write(
                &r.join(format!("folder_{d:02}"))
                    .join(format!("file_{f}.dat")),
                4_096 * (d + 1),
            );
        }
    }
    let res = run(&["overview", r.to_str().unwrap(), "-q"]);
    assert_eq!(res.code, 0);
    assert!(res.stdout.len() <= 4096, "{} bytes", res.stdout.len());
}

#[test]
fn t_ag_08_edge_cases() {
    // 空資料夾
    let e = fixture("m11_empty");
    let j = one(&run(&["overview", e.to_str().unwrap(), "-q"]));
    assert_eq!(j.get("totals").get("files").num(), 0);
    assert!(items(&j, "top_files").is_empty());
    assert!(items(&j, "top_dirs").is_empty());
    // 不存在的路徑
    let nf = run(&[
        "overview",
        e.join("nope").to_str().unwrap(),
        "-q",
        "--log",
        "json",
    ]);
    assert_eq!(nf.code, 1);
    assert_eq!(
        parse_json(nf.stderr.trim()).get("code").str(),
        "path_not_found"
    );
    // --top 的範圍
    for bad in ["0", "101"] {
        let b = run(&[
            "overview",
            e.to_str().unwrap(),
            "-q",
            "--top",
            bad,
            "--log",
            "json",
        ]);
        assert_eq!(b.code, 1, "--top {bad}");
        assert!(b.stdout.is_empty());
        let j = parse_json(b.stderr.trim());
        assert_eq!(j.get("code").str(), "value_out_of_range");
        assert!(j.get("message").str().contains("1 and 100"));
    }
    // 少了輸入與 report 的選項
    assert_eq!(run(&["overview", "-q"]).code, 1);
    assert_eq!(
        run(&["overview", e.to_str().unwrap(), "--top-files", "5"]).code,
        1
    );
    // 無權限的目錄：結果仍完整，coverage 非空，結束代碼 2
    let r = fixture("m11_denied");
    let locked = r.join("locked");
    write(&locked.join("secret.txt"), 100);
    write(&r.join("visible.txt"), 50);
    let user = std::env::var("USERNAME").expect("USERNAME");
    let out = Command::new("icacls")
        .arg(&locked)
        .args(["/deny", &format!("{user}:(RD)")])
        .output()
        .unwrap();
    assert!(out.status.success(), "icacls: {out:?}");
    let res = run(&["overview", r.to_str().unwrap(), "-q"]);
    let _ = Command::new("icacls")
        .arg(&locked)
        .args(["/remove:d", &user])
        .output();
    let _ = Command::new("icacls")
        .arg(&locked)
        .args(["/grant", &format!("{user}:(OI)(CI)F")])
        .output();
    assert_eq!(res.code, 2, "{}", res.stderr);
    let j = res.lines().remove(0);
    assert_eq!(j.get("coverage").get("incomplete_dirs").num(), 1);
    assert_eq!(j.get("totals").get("logical").num(), 50);
}

#[test]
fn t_ag_08_help_lists_every_overview_option() {
    let h = run(&["overview", "--help"]);
    assert_eq!(h.code, 0);
    let t = h.text();
    for o in [
        "--top",
        "--ext-top",
        "--stale-days",
        "--stale-min-size",
        "--rank",
        "--detail",
        "--save-snapshot",
        "--snapshot",
    ] {
        assert!(t.contains(o), "{o}");
    }
}

fn demo() -> PathBuf {
    PathBuf::from(
        std::env::var("VOLTDIR_DEMO")
            .expect("set VOLTDIR_DEMO to the folder made by scripts\\New-DemoWorkspace.ps1"),
    )
}

/// 示範資料（monday）。需要環境變數 `VOLTDIR_DEMO`，預設不跑：`cargo test --test m11 -- --ignored`。
#[test]
#[ignore = "needs the demo workspace (set VOLTDIR_DEMO)"]
fn t_ag_08_demo_monday() {
    let m = demo().join("monday");
    let res = run(&["overview", m.to_str().unwrap(), "-q"]);
    assert!(res.stdout.len() <= 4096, "{} bytes", res.stdout.len());
    let j = one(&res);
    assert_eq!(j.get("totals").get("files").num(), 28_675);
    assert_eq!(j.get("totals").get("logical").num(), 873_021_440);
    let rp = one(&run(&[
        "report",
        m.to_str().unwrap(),
        "-q",
        "--top-files",
        "10",
        "--top-dirs",
        "10",
        "--by-ext",
        "8",
        "--many-files",
        "5",
    ]));
    assert_eq!(rp.get("totals"), j.get("totals"));
    assert_eq!(items(&j, "by_ext"), items(rp.get("reports"), "by_ext"));
}

/// 100 萬檔案的樹與標準答案。需要環境變數 `VOLTDIR_B5_TREE`。
#[test]
#[ignore = "needs the 1,000,000-file tree from benchmarks\\New-BenchTree.ps1 (set VOLTDIR_B5_TREE)"]
fn t_ag_08_b5_tree_matches_the_answer_key() {
    let t = std::env::var("VOLTDIR_B5_TREE")
        .expect("set VOLTDIR_B5_TREE to the tree made by benchmarks\\New-BenchTree.ps1");
    let t = t.as_str();
    let res = run(&["overview", t, "-q", "--rank", "logical"]);
    assert!(res.stdout.len() <= 4096, "{} bytes", res.stdout.len());
    let j = one(&res);
    assert_eq!(j.get("totals").get("files").num(), 1_000_000);
    assert_eq!(j.get("totals").get("logical").num(), 8_863_186_944);
    let dirs: Vec<String> = items(&j, "top_dirs")
        .iter()
        .take(5)
        .map(|d| d.get("path").str().to_string())
        .collect();
    assert_eq!(dirs, ["pkgs", "build", "media", "logs", "media\\big"]);
    let files: Vec<String> = items(&j, "top_files")
        .iter()
        .take(5)
        .map(|d| d.get("path").str().to_string())
        .collect();
    assert_eq!(
        files,
        [
            "media\\big\\big_11.bin",
            "media\\big\\big_10.bin",
            "media\\big\\big_09.bin",
            "media\\big\\big_08.bin",
            "media\\big\\big_07.bin"
        ]
    );
    assert_eq!(items(&j, "by_ext")[0].get("ext").str(), ".js");
    assert_eq!(j.get("empties").get("files").num(), 1000);
    assert_eq!(j.get("empties").get("dirs").num(), 50);
}

#[test]
fn t_ag_08_skill_md_stays_within_6000_characters_and_starts_with_overview() {
    let skill = include_str!("../skills/voltdir/SKILL.md");
    assert!(
        skill.chars().count() <= 6000,
        "{} chars",
        skill.chars().count()
    );
    let first_flow = skill
        .find("voltdir overview")
        .expect("overview in SKILL.md");
    let first_find = skill.find("voltdir find").expect("find in SKILL.md");
    assert!(first_flow < first_find, "overview must be the first flow");
    assert!(skill.contains("reference.md"));
}

#[test]
fn t_ag_08_dir_rank_direct_lists_the_folders_that_own_the_files() {
    let r = tree("m11_dirrank");
    let rollup = one(&run(&["overview", r.to_str().unwrap(), "-q"]));
    let direct = one(&run(&[
        "overview",
        r.to_str().unwrap(),
        "-q",
        "--dir-rank",
        "direct",
    ]));
    assert!(
        rollup.get("dir_rank").is_null(),
        "default output has no dir_rank"
    );
    assert_eq!(direct.get("dir_rank").str(), "direct");
    let d = items(&direct, "top_dirs");
    // media 自己有 a.mp4＋b.mp4（98304），子資料夾 raw 有 c.mp4（16384）；
    // 滾算時 media 含 raw 共 114688。direct 排序下的順序與大小要用直接子檔案
    assert_eq!(d[0].get("path").str(), "media");
    assert_eq!(d[0].get("direct_physical").num(), 98_304);
    assert_eq!(
        d[0].get("physical").num(),
        114_688,
        "rollup is still listed"
    );
    // 沒有自己的檔案的資料夾（void、also_void）不會出現
    assert!(d.iter().all(|x| x.get("direct_physical").num() > 0));
    // 滾算排名最後一名之前，祖先與子孫並列；direct 不會把同一批檔案算兩次
    let total: i128 = d.iter().map(|x| x.get("direct_physical").num()).sum();
    assert!(total <= direct.get("totals").get("physical").num());
    // 其餘欄位與預設相同
    assert_eq!(rollup.get("totals"), direct.get("totals"));
    assert_eq!(rollup.get("top_files"), direct.get("top_files"));
}

#[test]
fn t_ag_08_output_over_budget_is_trimmed_unless_top_is_explicit() {
    // 很長的資料夾與檔案名稱：預設 10 筆會超過 4096 位元組
    let r = fixture("m11_trim");
    for d in 0..14 {
        let long = format!(
            "{}_{d:02}",
            "very_long_folder_name_for_the_size_budget_".repeat(2)
        );
        for f in 0..2 {
            write(
                &r.join(&long)
                    .join(format!("{}_{f}.dat", "long_file_name_".repeat(3))),
                4_096 * (d + 1),
            );
        }
    }
    let auto = run(&["overview", r.to_str().unwrap(), "-q"]);
    assert!(auto.stdout.len() <= 4096, "{} bytes", auto.stdout.len());
    let j = one(&auto);
    let top = j.get("trimmed").get("top").num();
    assert!(top == 5 || top == 3, "{top}");
    assert!(items(&j, "top_dirs").len() as i128 <= top);
    assert!(!items(&j, "top_dirs").is_empty());
    assert!(
        j.get("top_dirs").get("truncated").bool(),
        "trimmed lists are truncated"
    );
    // 數字仍與 report 一致
    let rp = one(&run(&[
        "report",
        r.to_str().unwrap(),
        "-q",
        "--top-files",
        "1",
    ]));
    assert_eq!(rp.get("totals"), j.get("totals"));
    assert_eq!(
        items(rp.get("reports"), "top_files")[0].get("size_physical"),
        items(&j, "top_files")[0].get("physical")
    );
    // 明確給 --top：尊重使用者，不收斂
    let explicit = one(&run(&[
        "overview",
        r.to_str().unwrap(),
        "-q",
        "--top",
        "10",
    ]));
    assert!(explicit.get("trimmed").is_null());
    assert_eq!(items(&explicit, "top_dirs").len(), 10);
    // 沒超過預算時沒有 trimmed
    let small = tree("m11_notrim");
    assert!(
        one(&run(&["overview", small.to_str().unwrap(), "-q"]))
            .get("trimmed")
            .is_null()
    );
}
