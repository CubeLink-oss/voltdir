//! 快照旁的彙總索引（`<FILE>.idx`，`voltdir.index/1`）。
//!
//! 核心要求：有索引與沒有索引，輸出逐位元組相同；索引無效就忽略並印 `index: not used (...)`；
//! 索引寫入失敗不影響快照；`--no-index` 不產生索引。

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

fn idx_of(snap: &Path) -> PathBuf {
    let mut s = snap.as_os_str().to_owned();
    s.push(".idx");
    PathBuf::from(s)
}

/// 跨資料夾硬連結、空檔案、空資料夾、不同副檔名、重複大小（靠路徑排名）、一個超過 100 MiB 的稀疏檔。
fn tree(name: &str) -> PathBuf {
    let r = fixture(name);
    write(&r.join("a").join("x.bin"), 40_960);
    std::fs::create_dir_all(r.join("z")).unwrap();
    std::fs::hard_link(r.join("a").join("x.bin"), r.join("z").join("x2.bin")).unwrap();
    write(&r.join("d").join("k.txt"), 20_480);
    std::fs::create_dir_all(r.join("d").join("j")).unwrap();
    std::fs::hard_link(
        r.join("d").join("k.txt"),
        r.join("d").join("j").join("k2.txt"),
    )
    .unwrap();
    write(&r.join("e").join("empty.txt"), 0);
    std::fs::create_dir_all(r.join("e").join("void").join("deeper")).unwrap();
    for (i, n) in ["A.TXT", "b.txt", "c.Txt"].iter().enumerate() {
        write(&r.join("t").join(n), 4_096 * (1 + i % 2));
    }
    write(&r.join("noext"), 4_096);
    write(&r.join("日本語").join("檔案.dat"), 16_384);
    write(
        &r.join("deep")
            .join("l1")
            .join("l2")
            .join("l3")
            .join("f.log"),
        8_192,
    );
    r
}

fn snap_with_index(root: &Path, name: &str, extra: &[&str]) -> PathBuf {
    let base = fixture(name);
    let snap = base.join("s.ndjson");
    let mut a = vec![
        "scan",
        root.to_str().unwrap(),
        "-q",
        "--out",
        snap.to_str().unwrap(),
    ];
    a.extend(extra);
    let r = run(&a);
    assert!(r.code == 0 || r.code == 2, "{}", r.stderr);
    snap
}

/// 同一份快照：先用索引、再把索引改名藏起來（逐位元組比較兩次輸出）。
fn both(snap: &Path, args: &[&str]) -> (Run, Run) {
    let idx = idx_of(snap);
    assert!(idx.exists(), "no index written next to {}", snap.display());
    let with = run(args);
    let hidden = idx.with_extension("idx.hidden");
    std::fs::rename(&idx, &hidden).unwrap();
    let without = run(args);
    std::fs::rename(&hidden, &idx).unwrap();
    (with, without)
}

#[test]
fn t_ag_10_scan_writes_an_index_next_to_the_snapshot_and_no_index_turns_it_off() {
    let r = tree("m13_write");
    let snap = snap_with_index(&r, "m13_write_out", &[]);
    let idx = idx_of(&snap);
    assert!(idx.exists(), "scan --out must write <FILE>.idx");
    let head = std::fs::read_to_string(&idx).unwrap();
    assert!(
        head.contains("voltdir.index/1"),
        "{}",
        &head[..head.len().min(200)]
    );
    // 資料夾裡只有快照與索引，沒有別的檔案（不建快取資料夾）
    let names: Vec<_> = std::fs::read_dir(snap.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), 2, "{names:?}");
    // --no-index
    let base = fixture("m13_noindex_out");
    let s2 = base.join("s.ndjson");
    let res = run(&[
        "scan",
        r.to_str().unwrap(),
        "-q",
        "--no-index",
        "--out",
        s2.to_str().unwrap(),
    ]);
    assert_eq!(res.code, 0, "{}", res.stderr);
    assert!(!idx_of(&s2).exists());
    // overview --save-snapshot 也寫索引，--no-index 也能關
    let s3 = base.join("o.ndjson");
    let res = run(&[
        "overview",
        r.to_str().unwrap(),
        "-q",
        "--save-snapshot",
        s3.to_str().unwrap(),
    ]);
    assert_eq!(res.code, 0, "{}", res.stderr);
    assert!(idx_of(&s3).exists());
    let s4 = base.join("o2.ndjson");
    let res = run(&[
        "overview",
        r.to_str().unwrap(),
        "-q",
        "--no-index",
        "--save-snapshot",
        s4.to_str().unwrap(),
    ]);
    assert_eq!(res.code, 0, "{}", res.stderr);
    assert!(!idx_of(&s4).exists());
    // scan 到標準輸出沒有地方放索引
    let res = run(&["scan", r.to_str().unwrap(), "-q", "--out", "-"]);
    assert_eq!(res.code, 0);
}

#[test]
fn t_ag_10_overview_output_is_byte_identical_with_and_without_the_index() {
    let r = tree("m13_ov");
    let snap = snap_with_index(&r, "m13_ov_out", &[]);
    let s = snap.to_str().unwrap();
    for extra in [
        vec![],
        vec!["--rank", "logical"],
        vec!["--dir-rank", "direct"],
        vec!["--rank", "logical", "--dir-rank", "direct"],
        vec!["--top", "3", "--ext-top", "2"],
        vec!["--top", "100", "--ext-top", "100"],
        vec!["--stale-days", "0", "--stale-min-size", "100MiB"],
    ] {
        let mut a = vec!["overview", "--snapshot", s];
        a.extend(&extra);
        let (with, without) = both(&snap, &a);
        assert_eq!(with.code, without.code, "{extra:?}");
        assert!(!with.stdout.is_empty());
        assert_eq!(with.stdout, without.stdout, "overview {extra:?}");
        assert!(
            !with.stderr.contains("index: not used"),
            "{extra:?}: {}",
            with.stderr
        );
    }
}

#[test]
fn t_ag_10_report_output_is_byte_identical_with_and_without_the_index() {
    let r = tree("m13_rp");
    let snap = snap_with_index(&r, "m13_rp_out", &[]);
    let s = snap.to_str().unwrap();
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
        vec!["--top-dirs", "20", "--dir-depth", "2"],
        vec!["--top-dirs", "3", "--dir-depth", "0"],
        vec!["--by-ext", "3"],
        vec!["--many-files", "4"],
    ] {
        let mut a = vec!["report", "--snapshot", s, "-q"];
        a.extend(&extra);
        let (with, without) = both(&snap, &a);
        assert_eq!(with.code, without.code, "{extra:?}");
        assert!(!with.stdout.is_empty());
        assert_eq!(with.stdout, without.stdout, "report {extra:?}");
    }
    // 沒有 -q 時，用到索引不印訊息；索引沒法回答的請求（有額外篩選）回退並說明原因
    let ok = run(&["report", "--snapshot", s, "--top-dirs", "3"]);
    assert!(!ok.stderr.contains("index: not used"), "{}", ok.stderr);
    let filtered = run(&[
        "report",
        "--snapshot",
        s,
        "--top-files",
        "3",
        "--min-size",
        "1000",
    ]);
    assert!(
        filtered.stderr.contains("index: not used ("),
        "{}",
        filtered.stderr
    );
}

#[test]
fn t_ag_10_limited_snapshots_also_match() {
    let r = tree("m13_lim");
    for (tag, extra) in [
        ("min", vec!["--min-size", "10000"]),
        ("depth", vec!["--max-depth", "1"]),
    ] {
        let snap = snap_with_index(&r, &format!("m13_lim_{tag}"), &extra);
        let s = snap.to_str().unwrap();
        let (a, b) = both(&snap, &["overview", "--snapshot", s]);
        assert_eq!(a.stdout, b.stdout, "{tag} overview");
        let (a, b) = both(
            &snap,
            &[
                "report",
                "--snapshot",
                s,
                "-q",
                "--top-dirs",
                "10",
                "--by-ext",
                "10",
                "--top-files",
                "10",
                "--many-files",
                "10",
            ],
        );
        assert_eq!(a.stdout, b.stdout, "{tag} report");
    }
}

#[test]
fn t_ag_10_a_changed_snapshot_makes_the_index_unusable_and_the_answer_stays_correct() {
    let r = tree("m13_bad");
    let snap = snap_with_index(&r, "m13_bad_out", &[]);
    let s = snap.to_str().unwrap();
    let good = run(&["overview", "--snapshot", s]);
    assert!(!good.stderr.contains("index: not used"));
    // 附加一個位元組（長度改變）
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&snap)
        .unwrap();
    f.write_all(b"\n").unwrap();
    drop(f);
    let r1 = run(&["overview", "--snapshot", s]);
    assert!(r1.stderr.contains("index: not used ("), "{}", r1.stderr);
    assert_eq!(r1.stdout, good.stdout, "answer must still be correct");
    // 長度回復但修改時間改變
    let mut data = std::fs::read(&snap).unwrap();
    data.pop();
    std::fs::write(&snap, &data).unwrap();
    let r2 = run(&["overview", "--snapshot", s]);
    assert!(
        r2.stderr.contains("index: not used ("),
        "mtime changed: {}",
        r2.stderr
    );
    assert_eq!(r2.stdout, good.stdout);
    // -q 不印訊息
    let r3 = run(&["overview", "--snapshot", s, "-q"]);
    assert!(!r3.stderr.contains("index: not used"));
    // 索引本身壞掉：忽略並回退
    let snap2 = snap_with_index(&r, "m13_bad_out2", &[]);
    std::fs::write(idx_of(&snap2), b"{not json").unwrap();
    let r4 = run(&["overview", "--snapshot", snap2.to_str().unwrap()]);
    assert!(r4.stderr.contains("index: not used ("), "{}", r4.stderr);
    assert_eq!(r4.code, 0);
    // 沒有索引（舊快照）：回退並說明
    let snap3 = snap_with_index(&r, "m13_bad_out3", &["--no-index"]);
    let r5 = run(&["overview", "--snapshot", snap3.to_str().unwrap()]);
    assert!(r5.stderr.contains("index: not used ("), "{}", r5.stderr);
    assert_eq!(r5.code, 0);
}

#[test]
fn t_ag_10_index_write_failure_does_not_fail_the_snapshot() {
    let r = tree("m13_fail");
    let base = fixture("m13_fail_out");
    let snap = base.join("s.ndjson");
    // 索引路徑被同名資料夾佔住
    std::fs::create_dir(idx_of(&snap)).unwrap();
    let res = run(&["scan", r.to_str().unwrap(), "--out", snap.to_str().unwrap()]);
    assert_eq!(res.code, 0, "{}", res.stderr);
    assert!(
        res.stderr.contains("index"),
        "a warning about the index is expected: {}",
        res.stderr
    );
    assert!(snap.exists());
    let again = run(&["overview", "--snapshot", snap.to_str().unwrap()]);
    assert_eq!(again.code, 0, "{}", again.stderr);
    // overview --save-snapshot 同樣
    let snap2 = base.join("o.ndjson");
    std::fs::create_dir(idx_of(&snap2)).unwrap();
    let res = run(&[
        "overview",
        r.to_str().unwrap(),
        "--save-snapshot",
        snap2.to_str().unwrap(),
    ]);
    assert_eq!(res.code, 0, "{}", res.stderr);
    assert!(snap2.exists());
}

#[test]
fn t_ag_10_find_ignores_the_index() {
    let r = tree("m13_find");
    let snap = snap_with_index(&r, "m13_find_out", &[]);
    let res = run(&[
        "find",
        "--snapshot",
        snap.to_str().unwrap(),
        "--name",
        "*.txt",
    ]);
    assert_eq!(res.code, 0, "{}", res.stderr);
    assert!(!res.stderr.contains("index"), "{}", res.stderr);
}

/// B5 樹（100 萬檔案）：寫快照後的追問都在 0.5 秒內，索引 ≤ 5 MiB，輸出與沒有索引時相同。
#[test]
#[ignore = "needs the 1,000,000-file tree from benchmarks\\New-BenchTree.ps1 (set VOLTDIR_B5_TREE); run with --ignored"]
fn t_ag_10_b5_tree_follow_ups_are_fast_and_the_index_is_small() {
    let t = std::env::var("VOLTDIR_B5_TREE")
        .expect("set VOLTDIR_B5_TREE to the tree made by benchmarks\\New-BenchTree.ps1");
    let base = fixture("m13_b5");
    let snap = base.join("s.ndjson");
    let res = run(&[
        "overview",
        &t,
        "-q",
        "--save-snapshot",
        snap.to_str().unwrap(),
    ]);
    assert_eq!(res.code, 0, "{}", res.stderr);
    let size = std::fs::metadata(idx_of(&snap)).unwrap().len();
    eprintln!("index size: {} bytes", size);
    assert!(size <= 5 * 1024 * 1024, "{size}");
    let s = snap.to_str().unwrap();
    for args in [
        vec!["overview", "--snapshot", s],
        vec![
            "report",
            "--snapshot",
            s,
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
        let t0 = std::time::Instant::now();
        let with = run(&args);
        let secs = t0.elapsed().as_secs_f64();
        eprintln!("{args:?}: {secs:.2} s");
        assert!(secs <= 0.5, "{secs:.2} s for {args:?}");
        let idx = idx_of(&snap);
        let hidden = idx.with_extension("idx.hidden");
        std::fs::rename(&idx, &hidden).unwrap();
        let without = run(&args);
        std::fs::rename(&hidden, &idx).unwrap();
        assert_eq!(with.stdout, without.stdout, "{args:?}");
    }
}

#[test]
fn t_ag_10_many_folders_and_ties_stay_identical_including_truncation() {
    // 1,300 個同深度的資料夾（超過每個深度保留的 1000 名），大小有很多相同值，檔案名稱也有大量重複大小
    let r = fixture("m13_many");
    for d in 0..1300usize {
        let dir = r.join(format!("dir_{d:04}"));
        let n = 1 + d % 3;
        for f in 0..n {
            write(&dir.join(format!("f{f}.dat")), 4_096 * (1 + d % 7));
        }
        if d % 50 == 0 {
            write(&dir.join("sub").join("deep.bin"), 4_096 * (1 + d % 5));
        }
    }
    let snap = snap_with_index(&r, "m13_many_out", &[]);
    let s = snap.to_str().unwrap();
    for extra in [
        vec![
            "--top-dirs",
            "1000",
            "--many-files",
            "1000",
            "--top-files",
            "1000",
            "--by-ext",
            "10",
        ],
        vec!["--top-dirs", "1000", "--dir-depth", "1"],
        vec!["--top-dirs", "7", "--dir-depth", "2", "--many-files", "7"],
    ] {
        let mut a = vec!["report", "--snapshot", s, "-q"];
        a.extend(&extra);
        let (with, without) = both(&snap, &a);
        assert_eq!(with.stdout, without.stdout, "report {extra:?}");
    }
    for extra in [
        vec![],
        vec!["--top", "100"],
        vec!["--rank", "logical", "--top", "100"],
        vec!["--dir-rank", "direct", "--top", "100"],
        vec!["--dir-rank", "direct", "--rank", "logical", "--top", "100"],
    ] {
        let mut a = vec!["overview", "--snapshot", s];
        a.extend(&extra);
        let (with, without) = both(&snap, &a);
        assert_eq!(with.stdout, without.stdout, "overview {extra:?}");
        assert!(!with.stderr.contains("index: not used"), "{}", with.stderr);
    }
}
