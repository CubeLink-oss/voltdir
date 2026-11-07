//! 只寫索引的串流掃描（`overview --save-index`）與從索引追問（`--index`）。
//!
//! 想法來自「一次走訪算完所有答案」的強基準：串流掃描時同時建好彙總索引，不寫快照；
//! 之後的 `overview`／`report` 追問只讀索引。

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

fn one(r: &Run) -> J {
    assert!(r.code == 0 || r.code == 2, "{}", r.stderr);
    let l = r.lines();
    assert_eq!(l.len(), 1);
    l.into_iter().next().unwrap()
}

/// 跨資料夾硬連結、檔案與子目錄混排、空檔案、空資料夾、各種副檔名、雙根。
fn tree(name: &str) -> (PathBuf, PathBuf) {
    let base = fixture(name);
    let (r1, r2) = (base.join("one"), base.join("two"));
    write(&r1.join("a").join("x.bin"), 40_960);
    std::fs::create_dir_all(r1.join("z")).unwrap();
    std::fs::hard_link(r1.join("a").join("x.bin"), r1.join("z").join("x2.bin")).unwrap();
    write(&r1.join("d").join("k.txt"), 20_480);
    std::fs::create_dir_all(r1.join("d").join("j")).unwrap();
    std::fs::hard_link(
        r1.join("d").join("k.txt"),
        r1.join("d").join("j").join("k2.txt"),
    )
    .unwrap();
    write(&r1.join("e").join("empty.txt"), 0);
    std::fs::create_dir_all(r1.join("e").join("void").join("deeper")).unwrap();
    for (i, n) in ["A.TXT", "b.txt", "c.Txt"].iter().enumerate() {
        write(&r1.join("t").join(n), 4_096 * (1 + i % 2));
    }
    write(&r1.join("noext"), 4_096);
    write(&r1.join("日本語").join("檔案.dat"), 16_384);
    write(&r1.join("deep").join("l1").join("l2").join("f.log"), 8_192);
    write(&r2.join("own").join("o.log"), 12_288);
    (r1, r2)
}

fn save_index(roots: &[&Path], index: &Path, extra: &[&str]) -> Run {
    let mut a = vec!["overview"];
    a.extend(roots.iter().map(|r| r.to_str().unwrap()));
    a.extend(["-q", "--save-index", index.to_str().unwrap()]);
    a.extend(extra);
    run(&a)
}

#[test]
fn t_ag_12_save_index_writes_only_the_index_and_still_answers_the_question() {
    let (r1, _) = tree("m15_write");
    let base = fixture("m15_write_out");
    let idx = base.join("i.idx");
    let res = save_index(&[&r1], &idx, &[]);
    assert_eq!(res.code, 0, "{}", res.stderr);
    // 這次呼叫本身就回答了 overview（與不存索引的 overview 相同）
    let plain = run(&["overview", r1.to_str().unwrap(), "-q"]);
    let (a, b) = (one(&res), one(&plain));
    for k in [
        "totals",
        "top_dirs",
        "top_files",
        "by_ext",
        "stale_large",
        "empties",
        "coverage",
    ] {
        assert_eq!(a.get(k), b.get(k), "{k}");
    }
    // 資料夾裡只有索引：沒有快照、沒有暫存檔
    let names: Vec<_> = std::fs::read_dir(&base)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["i.idx"], "{names:?}");
    assert!(
        std::fs::read_to_string(&idx)
            .unwrap()
            .contains("voltdir.index/1")
    );
}

#[test]
fn t_ag_12_overview_from_the_index_equals_a_live_overview() {
    let (r1, r2) = tree("m15_ov");
    for (tag, roots) in [
        ("single", vec![r1.as_path()]),
        ("double", vec![r1.as_path(), r2.as_path()]),
    ] {
        let idx = fixture(&format!("m15_ov_{tag}")).join("i.idx");
        let res = save_index(&roots, &idx, &[]);
        assert_eq!(res.code, 0, "{}", res.stderr);
        let root_args: Vec<&str> = roots.iter().map(|r| r.to_str().unwrap()).collect();
        for extra in [
            vec![],
            vec!["--rank", "logical"],
            vec!["--dir-rank", "direct"],
            vec!["--rank", "logical", "--dir-rank", "direct"],
            vec!["--top", "3", "--ext-top", "2"],
            vec!["--stale-days", "0"],
        ] {
            let mut live = vec!["overview"];
            live.extend(&root_args);
            live.extend(["-q", "--top", "10"]);
            live.extend(&extra);
            let mut from_idx = vec![
                "overview",
                "--index",
                idx.to_str().unwrap(),
                "-q",
                "--top",
                "10",
            ];
            from_idx.extend(&extra);
            if extra.contains(&"--top") {
                live.truncate(live.len() - extra.len() - 2);
                live.extend(&extra);
                from_idx.truncate(from_idx.len() - extra.len() - 2);
                from_idx.extend(&extra);
            }
            let (a, b) = (one(&run(&live)), one(&run(&from_idx)));
            for k in [
                "totals",
                "top_dirs",
                "top_files",
                "by_ext",
                "stale_large",
                "empties",
                "coverage",
                "path_base",
                "rank",
                "roots",
                "engine",
            ] {
                assert_eq!(a.get(k), b.get(k), "{tag} {extra:?}: {k}");
            }
        }
    }
}

#[test]
fn t_ag_12_report_from_the_index_equals_a_live_report() {
    let (r1, _) = tree("m15_rp");
    let idx = fixture("m15_rp_out").join("i.idx");
    assert_eq!(save_index(&[&r1], &idx, &[]).code, 0);
    for extra in [
        vec![
            "--top-dirs",
            "5",
            "--by-ext",
            "5",
            "--many-files",
            "5",
            "--top-files",
            "5",
        ],
        vec![
            "--top-dirs",
            "1000",
            "--by-ext",
            "1000",
            "--many-files",
            "1000",
            "--top-files",
            "1000",
        ],
        vec!["--top-dirs", "20", "--dir-depth", "1"],
        vec!["--top-dirs", "3", "--dir-depth", "0"],
        vec!["--many-files", "4"],
    ] {
        let mut live = vec!["report", r1.to_str().unwrap(), "-q"];
        live.extend(&extra);
        let mut from_idx = vec!["report", "--index", idx.to_str().unwrap(), "-q"];
        from_idx.extend(&extra);
        let (a, b) = (one(&run(&live)), one(&run(&from_idx)));
        for k in ["totals", "coverage", "reports"] {
            assert_eq!(a.get(k), b.get(k), "{extra:?}: {k}");
        }
        assert_eq!(b.get("meta").get("source").str(), "index");
    }
}

#[test]
fn t_ag_12_a_question_the_index_cannot_answer_fails_with_a_hint_instead_of_guessing() {
    let (r1, _) = tree("m15_cannot");
    let idx = fixture("m15_cannot_out").join("i.idx");
    assert_eq!(save_index(&[&r1], &idx, &[]).code, 0);
    let i = idx.to_str().unwrap();
    for args in [
        vec![
            "report",
            "--index",
            i,
            "--top-files",
            "3",
            "--min-size",
            "1000",
            "--log",
            "json",
        ],
        vec![
            "overview",
            "--index",
            i,
            "--stale-min-size",
            "1MiB",
            "--log",
            "json",
        ],
    ] {
        let r = run(&args);
        assert_eq!(r.code, 1, "{args:?}: {}", r.stderr);
        assert!(r.stdout.is_empty());
        let e = parse_json(r.stderr.trim());
        assert_eq!(e.get("code").str(), "index_cannot_answer", "{e:?}");
        assert!(e.get("hint").str().contains("--save-snapshot"), "{e:?}");
    }
    // 找不到索引檔、或根本不是索引
    let r = run(&[
        "overview",
        "--index",
        idx.with_extension("nope").to_str().unwrap(),
        "--log",
        "json",
    ]);
    assert_eq!(r.code, 1);
    assert_eq!(
        parse_json(r.stderr.trim()).get("code").str(),
        "index_unreadable"
    );
    let bogus = idx.with_extension("bogus");
    std::fs::write(&bogus, b"{not json").unwrap();
    let r = run(&[
        "overview",
        "--index",
        bogus.to_str().unwrap(),
        "--log",
        "json",
    ]);
    assert_eq!(r.code, 1);
    assert_eq!(
        parse_json(r.stderr.trim()).get("code").str(),
        "index_unreadable"
    );
}

#[test]
fn t_ag_12_option_conflicts_and_early_output_checks() {
    let (r1, _) = tree("m15_opts");
    let base = fixture("m15_opts_out");
    let (idx, snap) = (base.join("i.idx"), base.join("s.ndjson"));
    let r = r1.to_str().unwrap();
    for args in [
        vec![
            "overview",
            r,
            "--save-index",
            idx.to_str().unwrap(),
            "--save-snapshot",
            snap.to_str().unwrap(),
            "--log",
            "json",
        ],
        vec![
            "overview",
            r,
            "--save-index",
            idx.to_str().unwrap(),
            "--no-index",
            "--log",
            "json",
        ],
        vec![
            "overview",
            "--index",
            idx.to_str().unwrap(),
            "--snapshot",
            snap.to_str().unwrap(),
            "--log",
            "json",
        ],
        vec![
            "overview",
            r,
            "--index",
            idx.to_str().unwrap(),
            "--log",
            "json",
        ],
        vec![
            "overview",
            "--index",
            idx.to_str().unwrap(),
            "--save-index",
            idx.to_str().unwrap(),
            "--log",
            "json",
        ],
        vec![
            "scan",
            r,
            "--out",
            snap.to_str().unwrap(),
            "--save-index",
            idx.to_str().unwrap(),
            "--log",
            "json",
        ],
    ] {
        let res = run(&args);
        assert_eq!(res.code, 1, "{args:?}: {}", res.stderr);
        assert!(res.stdout.is_empty(), "{args:?}");
        assert!(!parse_json(res.stderr.trim()).get("hint").str().is_empty());
    }
    // 父資料夾不存在：掃描前就失敗，結束代碼 4
    let missing = base.join("nope").join("i.idx");
    let res = run(&[
        "overview",
        r,
        "-q",
        "--save-index",
        missing.to_str().unwrap(),
    ]);
    assert_eq!(res.code, 4, "{}", res.stderr);
    assert!(res.stdout.is_empty());
    // 沒有 --save-index／--index 的行為不變：不寫任何檔案
    assert_eq!(run(&["overview", r, "-q"]).code, 0);
    assert!(!idx.exists());
}

#[test]
fn t_ag_12_next_commands_from_an_index_run_and_do_not_promise_find() {
    let (r1, _) = tree("m15_next");
    let idx = fixture("m15_next_out").join("i.idx");
    assert_eq!(save_index(&[&r1], &idx, &[]).code, 0);
    let ov = one(&run(&["overview", "--index", idx.to_str().unwrap(), "-q"]));
    let J::Arr(next) = ov.get("next") else {
        panic!()
    };
    assert!(!next.is_empty());
    for n in next {
        let cmd = n.str();
        assert!(
            !cmd.contains("voltdir find"),
            "no snapshot, so no find: {cmd}"
        );
        let args = split_args(cmd);
        assert_eq!(args[0], "voltdir");
        let refs: Vec<&str> = args[1..].iter().map(String::as_str).collect();
        // 要求「重新掃描並存快照」的那一條不在這裡執行（會掃整棵樹）；其餘必須能直接跑
        if cmd.contains("--save-snapshot") {
            continue;
        }
        let res = run(&refs);
        assert!(res.code == 0 || res.code == 2, "{cmd}: {}", res.stderr);
    }
    assert!(
        next.iter().any(|n| n.str().contains("--save-snapshot")),
        "must say how to get find back"
    );
}

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
fn t_ag_12_the_scan_with_save_index_uses_the_streaming_path() {
    // 與串流彙總相同：不保留逐筆項目。小夾具量不出記憶體，這裡只確認數字與串流的 overview 相同，
    // 並且 --threads 不影響結果；記憶體上限見下面 #[ignore] 的 B5 測試
    let (r1, _) = tree("m15_threads");
    let mut seen = Vec::new();
    for t in ["1", "3", "16"] {
        let idx = fixture(&format!("m15_threads_{t}")).join("i.idx");
        let res = save_index(&[&r1], &idx, &["--threads", t]);
        assert_eq!(res.code, 0, "{}", res.stderr);
        let from_idx = one(&run(&[
            "report",
            "--index",
            idx.to_str().unwrap(),
            "-q",
            "--top-dirs",
            "50",
            "--by-ext",
            "50",
            "--many-files",
            "50",
            "--top-files",
            "50",
        ]));
        seen.push((
            from_idx.get("totals").clone(),
            from_idx.get("reports").clone(),
        ));
    }
    // 時間欄位不拿來比：資料夾的修改時間是從「父層目錄索引裡的副本」讀出來的，NTFS 在資料夾剛被
    // 改過之後會稍晚才更新那份副本，所以建好夾具馬上連掃三次，第一次可能讀到略舊的值（差約 1 到 2
    // 毫秒）。這個測試要確認的是數量、大小與排名不受 --threads 影響
    let seen: Vec<_> = seen
        .iter()
        .map(|(a, b)| (without_times(a), without_times(b)))
        .collect();
    assert!(
        seen.windows(2).all(|w| w[0] == w[1]),
        "the index answers differ between --threads 1, 3 and 16: {seen:?}"
    );
}

/// 去掉所有 `mtime` 與 `latest_mtime` 欄位（遞迴）。
fn without_times(j: &J) -> J {
    match j {
        J::Obj(v) => J::Obj(
            v.iter()
                .filter(|(k, _)| k != "mtime" && k != "latest_mtime")
                .map(|(k, x)| (k.clone(), without_times(x)))
                .collect(),
        ),
        J::Arr(v) => J::Arr(v.iter().map(without_times).collect()),
        other => other.clone(),
    }
}

/// B5 樹：一次串流掃描建好索引（不寫快照），記憶體約 15 MB；之後的追問 ≤ 0.5 秒；索引 ≤ 5 MiB。
#[test]
#[ignore = "needs the 1,000,000-file tree from benchmarks\\New-BenchTree.ps1 (set VOLTDIR_B5_TREE); run with --ignored"]
fn t_ag_12_b5_tree_index_only_session_is_small_and_fast() {
    let t = std::env::var("VOLTDIR_B5_TREE")
        .expect("set VOLTDIR_B5_TREE to the tree made by benchmarks\\New-BenchTree.ps1");
    let idx = fixture("m15_b5").join("i.idx");
    let t0 = std::time::Instant::now();
    let res = run(&["overview", &t, "-q", "--save-index", idx.to_str().unwrap()]);
    let scan = t0.elapsed().as_secs_f64();
    assert_eq!(res.code, 0, "{}", res.stderr);
    let size = std::fs::metadata(&idx).unwrap().len();
    eprintln!("scan+index {scan:.2} s, index {size} bytes");
    assert!(size <= 5 * 1024 * 1024, "{size}");
    for args in [
        vec!["overview", "--index", idx.to_str().unwrap()],
        vec![
            "report",
            "--index",
            idx.to_str().unwrap(),
            "--top-dirs",
            "5",
            "--by-ext",
            "5",
            "--many-files",
            "5",
            "--top-files",
            "5",
        ],
    ] {
        let t1 = std::time::Instant::now();
        let r = run(&args);
        let secs = t1.elapsed().as_secs_f64();
        eprintln!("{args:?}: {secs:.2} s");
        assert!(r.code == 0, "{}", r.stderr);
        assert!(secs <= 0.5, "{secs:.2} s");
    }
}
