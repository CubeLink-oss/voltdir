//! 串流彙總。即時的 `overview`／`report` 不保留逐筆項目，但數字必須和「先 `scan` 成快照再問快照」
//! 完全相同；記憶體上限測試需要 100 萬檔案的 B5 樹，預設不跑。

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
    assert!(r.code == 0 || r.code == 2, "{}", r.stderr);
    let l = r.lines();
    assert_eq!(l.len(), 1);
    l.into_iter().next().unwrap()
}

/// 夾具：跨資料夾的硬連結（主要名稱取決於標準順序，檔案與子目錄依名稱混合排序）、
/// 空檔案、空資料夾、各種副檔名、雙根。
fn fixture_tree(name: &str) -> (PathBuf, PathBuf) {
    let base = fixture(name);
    let (r1, r2) = (base.join("one"), base.join("two"));
    // a\x.bin 與 z\x2.bin：a 在 z 之前，所以 a\x.bin 是主要名稱
    write(&r1.join("a").join("x.bin"), 40_960);
    std::fs::create_dir_all(r1.join("z")).unwrap();
    std::fs::hard_link(r1.join("a").join("x.bin"), r1.join("z").join("x2.bin")).unwrap();
    // 檔案與子目錄混排：d\j（子目錄，名稱較小）裡的連結先於 d\k.txt，所以 d\j\k2.txt 是主要名稱
    write(&r1.join("d").join("k.txt"), 20_480);
    std::fs::create_dir_all(r1.join("d").join("j")).unwrap();
    std::fs::hard_link(
        r1.join("d").join("k.txt"),
        r1.join("d").join("j").join("k2.txt"),
    )
    .unwrap();
    // 同一個資料夾裡的兩個名稱
    write(&r1.join("same").join("p.dat"), 8_192);
    std::fs::hard_link(r1.join("same").join("p.dat"), r1.join("same").join("q.dat")).unwrap();
    // 跨根的硬連結：第二個根裡的名稱是次要的
    write(&r1.join("shared").join("s.iso"), 65_536);
    std::fs::create_dir_all(&r2).unwrap();
    std::fs::hard_link(r1.join("shared").join("s.iso"), r2.join("s_link.iso")).unwrap();
    write(&r2.join("own").join("o.log"), 12_288);
    // 空檔案、空資料夾、混合大小寫副檔名、大小相同的檔案（排名要靠路徑決定）
    write(&r1.join("e").join("empty.txt"), 0);
    std::fs::create_dir_all(r1.join("e").join("void").join("deeper")).unwrap();
    write(&r1.join("t").join("A.TXT"), 4_096);
    write(&r1.join("t").join("b.txt"), 4_096);
    write(&r1.join("t").join("c.Txt"), 4_096);
    write(&r1.join("noext"), 4_096);
    write(&r1.join("日本語").join("檔案.dat"), 16_384);
    (r1, r2)
}

/// 兩條路徑的結果要完全相同：即時（串流）與「掃成快照再問快照」。
fn assert_live_equals_snapshot(roots: &[&Path], tag: &str) {
    assert_live_equals_snapshot_with(roots, tag, &[]);
}

/// `scan_opts`（例如 `--exclude`、`--follow-links`）同時給快照掃描與即時掃描。
fn assert_live_equals_snapshot_with(roots: &[&Path], tag: &str, scan_opts: &[&str]) {
    let base = fixture(&format!("m12_{tag}_snap"));
    let snap = base.join("s.ndjson");
    let mut scan_args: Vec<&OsStr> = vec!["scan".as_ref()];
    scan_args.extend(roots.iter().map(|r| r.as_os_str()));
    scan_args.extend(scan_opts.iter().map(OsStr::new));
    scan_args.extend(["-q".as_ref(), "--out".as_ref(), snap.as_os_str()]);
    let s = voltdir(&scan_args);
    assert!(s.code == 0 || s.code == 2, "{}", s.stderr);
    let root_args: Vec<&str> = roots.iter().map(|r| r.to_str().unwrap()).collect();
    let snap_s = snap.to_str().unwrap();

    for extra in [
        vec![],
        vec!["--rank", "logical"],
        vec!["--top", "3", "--ext-top", "2"],
        vec!["--dir-rank", "direct"],
        vec!["--detail", "full"],
    ] {
        // 明確給 --top：不受「輸出超過預算就自動減少筆數」影響（live 的 next 比快照的多一行）
        let pin: Vec<&str> = if extra.contains(&"--top") {
            vec![]
        } else {
            vec!["--top", "10"]
        };
        let mut live = vec!["overview"];
        live.extend(&root_args);
        live.extend(["-q"]);
        live.extend(scan_opts);
        live.extend(&extra);
        live.extend(&pin);
        let mut fromsnap = vec!["overview", "--snapshot", snap_s, "-q"];
        fromsnap.extend(&extra);
        fromsnap.extend(&pin);
        let (a, b) = (one(&run(&live)), one(&run(&fromsnap)));
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
        ] {
            assert_eq!(a.get(k), b.get(k), "{tag} overview {extra:?}: {k}");
        }
        assert_eq!(
            a.get("roots"),
            b.get("roots"),
            "{tag}: roots differ between live and snapshot"
        );
    }

    let analyses = [
        "--top-files",
        "50",
        "--top-dirs",
        "50",
        "--by-ext",
        "50",
        "--many-files",
        "50",
    ];
    let mut live = vec!["report"];
    live.extend(&root_args);
    live.extend(["-q"]);
    live.extend(scan_opts);
    live.extend(analyses);
    let mut fromsnap = vec!["report", "--snapshot", snap_s, "-q"];
    fromsnap.extend(analyses);
    let (a, b) = (one(&run(&live)), one(&run(&fromsnap)));
    for k in ["totals", "coverage", "reports"] {
        assert_eq!(a.get(k), b.get(k), "{tag} report: {k}");
    }
}

#[test]
fn t_ag_09_live_numbers_equal_the_snapshot_numbers_with_hardlinks_and_empties() {
    let (r1, r2) = fixture_tree("hl");
    assert_live_equals_snapshot(&[&r1], "single");
    assert_live_equals_snapshot(&[&r1, &r2], "double");
    // 硬連結主要名稱的歸屬：a\x.bin 持有大小，z\x2.bin 是次要名稱
    let ov = one(&run(&[
        "overview",
        r1.to_str().unwrap(),
        "-q",
        "--dir-rank",
        "direct",
        "--top",
        "100",
    ]));
    let direct = |dir: &str| -> i128 {
        let J::Arr(items) = ov.get("top_dirs").get("items") else {
            panic!()
        };
        items
            .iter()
            .find(|d| d.get("path").str() == dir)
            .map(|d| d.get("direct_physical").num())
            .unwrap_or(0)
    };
    assert_eq!(direct("a"), 40_960, "a comes before z");
    assert_eq!(direct("z"), 0, "z holds only a secondary name");
    assert_eq!(
        direct("d\\j"),
        20_480,
        "the subfolder j sorts before k.txt, so k2.txt is primary"
    );
    assert_eq!(direct("d"), 0, "k.txt is the secondary name");
}

#[test]
fn t_ag_09_live_numbers_equal_the_snapshot_numbers_with_an_unreadable_folder() {
    let r = fixture("m12_denied");
    write(&r.join("locked").join("secret.txt"), 100);
    write(&r.join("visible").join("v.txt"), 50);
    write(&r.join("top.bin"), 8_192);
    let user = std::env::var("USERNAME").expect("USERNAME");
    let locked = r.join("locked");
    let out = Command::new("icacls")
        .arg(&locked)
        .args(["/deny", &format!("{user}:(RD)")])
        .output()
        .unwrap();
    assert!(out.status.success(), "icacls: {out:?}");
    let result = std::panic::catch_unwind(|| assert_live_equals_snapshot(&[&r], "denied"));
    let _ = Command::new("icacls")
        .arg(&locked)
        .args(["/remove:d", &user])
        .output();
    let _ = Command::new("icacls")
        .arg(&locked)
        .args(["/grant", &format!("{user}:(OI)(CI)F")])
        .output();
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

#[test]
fn t_ag_09_exclude_and_follow_links_behave_like_the_snapshot_scan() {
    let base = fixture("m12_follow");
    let (main, other) = (base.join("main"), base.join("other"));
    write(&main.join("keep").join("k.bin"), 12_288);
    write(&main.join("skipme").join("s.bin"), 8_192);
    write(&main.join("a.tmp"), 4_096);
    write(&other.join("o.bin"), 20_480);
    let link = main.join("link_to_other");
    let st = Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(&link)
        .arg(&other)
        .output()
        .unwrap();
    assert!(st.status.success(), "mklink /J failed: {st:?}");
    // 預設不跟隨連結、跟隨 junction、排除資料夾與副檔名
    assert_live_equals_snapshot_with(&[&main], "follow_none", &[]);
    assert_live_equals_snapshot_with(&[&main], "follow_junction", &["--follow-links", "junction"]);
    assert_live_equals_snapshot_with(
        &[&main],
        "exclude",
        &["--exclude", "*\\skipme", "--exclude", "*.tmp"],
    );
    let _ = std::fs::remove_dir(&link);
}

#[test]
fn t_ag_09_single_thread_and_many_threads_give_the_same_numbers() {
    let (r1, _) = fixture_tree("threads");
    let run_with = |t: &str| {
        let j = one(&run(&[
            "overview",
            r1.to_str().unwrap(),
            "-q",
            "--threads",
            t,
            "--top",
            "10",
        ]));
        (
            j.get("totals").clone(),
            j.get("top_dirs").clone(),
            j.get("top_files").clone(),
            j.get("by_ext").clone(),
        )
    };
    let one_thread = run_with("1");
    for t in ["2", "8", "24"] {
        assert_eq!(one_thread, run_with(t), "--threads {t}");
    }
}

// ---- 記憶體上限（B5 樹）----

#[repr(C)]
#[allow(non_snake_case)]
struct ProcessMemoryCounters {
    cb: u32,
    PageFaultCount: u32,
    PeakWorkingSetSize: usize,
    WorkingSetSize: usize,
    QuotaPeakPagedPoolUsage: usize,
    QuotaPagedPoolUsage: usize,
    QuotaPeakNonPagedPoolUsage: usize,
    QuotaNonPagedPoolUsage: usize,
    PagefileUsage: usize,
    PeakPagefileUsage: usize,
}

unsafe extern "system" {
    fn K32GetProcessMemoryInfo(
        process: *mut std::ffi::c_void,
        counters: *mut ProcessMemoryCounters,
        cb: u32,
    ) -> i32;
}

/// 執行 voltdir 並回傳子程序的峰值 working set（MB）。
fn peak_working_set_mb(args: &[&str]) -> f64 {
    use std::os::windows::io::AsRawHandle;
    let mut child = Command::new(env!("CARGO_BIN_EXE_voltdir"))
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn voltdir");
    let status = child.wait().expect("wait");
    assert!(
        status.code() == Some(0) || status.code() == Some(2),
        "{status:?}"
    );
    let mut c = ProcessMemoryCounters {
        cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
        PageFaultCount: 0,
        PeakWorkingSetSize: 0,
        WorkingSetSize: 0,
        QuotaPeakPagedPoolUsage: 0,
        QuotaPagedPoolUsage: 0,
        QuotaPeakNonPagedPoolUsage: 0,
        QuotaNonPagedPoolUsage: 0,
        PagefileUsage: 0,
        PeakPagefileUsage: 0,
    };
    // SAFETY: 句柄在 `child` 存活期間有效；`c` 是大小正確、可寫入的結構。
    let ok = unsafe { K32GetProcessMemoryInfo(child.as_raw_handle(), &mut c, c.cb) };
    assert_ne!(ok, 0, "GetProcessMemoryInfo failed");
    c.PeakWorkingSetSize as f64 / (1024.0 * 1024.0)
}

fn b5_tree() -> String {
    std::env::var("VOLTDIR_B5_TREE")
        .expect("set VOLTDIR_B5_TREE to the tree made by benchmarks\\New-BenchTree.ps1")
}

/// B5 樹（100 萬檔案）上，即時 `report` 與 `overview` 的峰值 working set ≤ 40 MB。
/// 需要 `benchmarks\New-BenchTree.ps1` 產生的樹；以 `cargo test --release --test m12 -- --ignored` 執行。
#[test]
#[ignore = "needs the 1,000,000-file tree from benchmarks\\New-BenchTree.ps1 (set VOLTDIR_B5_TREE); run with --ignored"]
fn t_ag_09_report_peak_working_set_is_at_most_40_mb_on_the_b5_tree() {
    let t = b5_tree();
    let mb = peak_working_set_mb(&[
        "report",
        &t,
        "-q",
        "--top-files",
        "20",
        "--top-dirs",
        "20",
        "--by-ext",
        "10",
        "--engine",
        "walk",
    ]);
    eprintln!("report peak working set: {mb:.1} MB");
    assert!(mb <= 40.0, "{mb:.1} MB");
}

#[test]
#[ignore = "needs the 1,000,000-file tree from benchmarks\\New-BenchTree.ps1 (set VOLTDIR_B5_TREE); run with --ignored"]
fn t_ag_09_overview_peak_working_set_is_at_most_40_mb_on_the_b5_tree() {
    let t = b5_tree();
    let mb = peak_working_set_mb(&["overview", &t, "-q", "--engine", "walk"]);
    eprintln!("overview peak working set: {mb:.1} MB");
    assert!(mb <= 40.0, "{mb:.1} MB");
}
