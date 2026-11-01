//! M1 夾具整合測試。夾具建在 target\fixtures\。

mod common;

use common::*;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn write(p: &Path, bytes: usize) {
    std::fs::write(p, vec![b'x'; bytes]).unwrap();
}

#[test]
fn ec08_empty_files_dirs_and_small_files() {
    let r = fixture("ec08");
    std::fs::create_dir(r.join("emptydir")).unwrap();
    write(&r.join("zero.txt"), 0);
    write(&r.join("tiny.txt"), 1);
    write(&r.join("small.txt"), 5000);
    let run = scan(&r);
    assert_eq!(run.code, 0, "{}", run.stderr);
    let l = run.lines();
    let root = find(&l, "dir", &r);
    assert_eq!(root.get("rollup").get("files").num(), 3);
    assert_eq!(root.get("rollup").get("dirs").num(), 1);
    assert_eq!(root.get("rollup").get("logical").num(), 5001);
    let empty = find(&l, "dir", &r.join("emptydir"));
    for k in ["files", "dirs", "logical", "physical"] {
        assert_eq!(empty.get("rollup").get(k).num(), 0, "emptydir rollup.{k}");
    }
    assert_eq!(
        find(&l, "file", &r.join("zero.txt"))
            .get("size_logical")
            .num(),
        0
    );
    assert_eq!(
        find(&l, "file", &r.join("tiny.txt"))
            .get("size_logical")
            .num(),
        1
    );
    assert_eq!(
        find(&l, "file", &r.join("small.txt"))
            .get("size_logical")
            .num(),
        5000
    );
    // 前序、同層依名稱排序：emptydir < small.txt < tiny.txt < zero.txt
    let paths: Vec<_> = l
        .iter()
        .skip(1)
        .take(5)
        .map(|x| x.get("name").str().to_string())
        .collect();
    assert_eq!(
        paths[1..],
        ["emptydir", "small.txt", "tiny.txt", "zero.txt"]
    );
}

#[test]
fn ec01_unpaired_surrogate_is_escaped_and_output_is_valid_utf8() {
    let r = fixture("ec01");
    let name: Vec<u16> = vec![b'a' as u16, 0xD800, b'b' as u16, b'.' as u16, b't' as u16];
    let p = r.join(OsString::from_wide(&name));
    write(&p, 3);
    let run = scan(&r);
    assert_eq!(run.code, 0, "{}", run.stderr);
    let text = run.text(); // 必須是合法 UTF-8
    assert!(text.contains("a\\ud800b.t"), "escape missing: {text}");
    assert!(!text.contains('\u{FFFD}'), "must not substitute U+FFFD");
    let l = run.lines();
    let f = l.iter().find(|x| x.get("type").str() == "file").unwrap();
    assert_eq!(f.get("size_logical").num(), 3);
    assert_eq!(f.get("ext").str(), ".t");
}

#[test]
fn ec02_path_longer_than_260() {
    let r = fixture("ec02");
    let mut deep = r.clone();
    for i in 0..14 {
        deep = deep.join(format!("segment_number_{i:02}_padding"));
    }
    // 以 \\?\ 前綴建立長路徑
    let ext = PathBuf::from(format!(r"\\?\{}", deep.display()));
    std::fs::create_dir_all(&ext).unwrap();
    write(&ext.join("deep.txt"), 7);
    assert!(deep.join("deep.txt").to_string_lossy().len() > 260);
    let run = scan(&r);
    assert_eq!(run.code, 0, "{}", run.stderr);
    let l = run.lines();
    let f = find(&l, "file", &deep.join("deep.txt"));
    assert_eq!(f.get("size_logical").num(), 7);
    assert!(f.get("path").str().len() > 260);
    assert!(!f.get("path").str().starts_with(r"\\?\"));
    assert_eq!(summary(&l).get("coverage").get("incomplete_dirs").num(), 0);
}

#[test]
fn ec12_alternate_data_streams_are_not_counted() {
    let r = fixture("ec12");
    let f = r.join("f.txt");
    write(&f, 10);
    std::fs::write(format!("{}:hidden", f.display()), vec![b'y'; 1_000_000]).unwrap();
    let run = scan(&r);
    assert_eq!(run.code, 0, "{}", run.stderr);
    let l = run.lines();
    assert_eq!(find(&l, "file", &f).get("size_logical").num(), 10);
    assert_eq!(find(&l, "dir", &r).get("rollup").get("logical").num(), 10);
    assert_eq!(summary(&l).get("entries").get("files").num(), 1);
}

#[test]
fn ec09_file_larger_than_4_gib_does_not_overflow() {
    let r = fixture("ec09");
    let f = r.join("big.bin");
    std::fs::File::create(&f).unwrap();
    // 先標記為稀疏檔，避免真的配置 5 GiB
    let st = Command::new("fsutil")
        .args(["sparse", "setflag"])
        .arg(&f)
        .output()
        .unwrap();
    assert!(st.status.success(), "fsutil sparse setflag failed: {st:?}");
    let want: u64 = 5 * (1 << 30) + 123;
    std::fs::OpenOptions::new()
        .write(true)
        .open(&f)
        .unwrap()
        .set_len(want)
        .unwrap();
    let run = scan(&r);
    assert_eq!(run.code, 0, "{}", run.stderr);
    let l = run.lines();
    let e = find(&l, "file", &f);
    assert_eq!(e.get("size_logical").num(), i128::from(want));
    assert!(
        e.get("size_physical").num() < i128::from(want) / 2,
        "sparse file should have small physical size"
    );
    assert!(e.get("attrs").clone() != J::Null);
    assert_eq!(
        find(&l, "dir", &r).get("rollup").get("logical").num(),
        i128::from(want)
    );
}

#[test]
fn ec05_files_created_and_deleted_during_scan_do_not_crash() {
    let r = fixture("ec05");
    for i in 0..200 {
        write(&r.join(format!("stable{i}.txt")), 10);
    }
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let churn = {
        let (stop, r) = (stop.clone(), r.clone());
        std::thread::spawn(move || {
            let mut n = 0u64;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let d = r.join(format!("tmpdir{}", n % 50));
                let _ = std::fs::create_dir_all(&d);
                for k in 0..20 {
                    let _ = std::fs::write(d.join(format!("t{k}")), b"abc");
                }
                let _ = std::fs::remove_dir_all(&d);
                n += 1;
            }
        })
    };
    for _ in 0..30 {
        let run = scan(&r);
        assert!(
            run.code == 0 || run.code == 2,
            "unexpected exit {}: {}",
            run.code,
            run.stderr
        );
        let l = run.lines(); // 每一行都是合法 JSON
        assert_eq!(summary(&l).get("type").str(), "summary");
        assert!(summary(&l).get("entries").get("files").num() >= 200);
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    churn.join().unwrap();
}

/// 退出時還原 ACL 的守衛，確保測完一定移除拒絕規則。
struct AclGuard(PathBuf);

impl Drop for AclGuard {
    fn drop(&mut self) {
        let user = std::env::var("USERNAME").unwrap_or_default();
        let _ = Command::new("icacls")
            .arg(&self.0)
            .args(["/remove:d", &user])
            .output();
        let _ = Command::new("icacls")
            .arg(&self.0)
            .args(["/grant", &format!("{user}:(OI)(CI)F")])
            .output();
    }
}

#[test]
fn ec04_access_denied_directory_is_reported_and_scan_continues() {
    let r = fixture("ec04");
    let locked = r.join("locked");
    std::fs::create_dir(&locked).unwrap();
    write(&locked.join("secret.txt"), 100);
    write(&r.join("visible.txt"), 50);
    let user = std::env::var("USERNAME").expect("USERNAME");
    let _guard = AclGuard(locked.clone());
    let out = Command::new("icacls")
        .arg(&locked)
        .args(["/deny", &format!("{user}:(RD)")])
        .output()
        .unwrap();
    assert!(out.status.success(), "icacls deny failed: {out:?}");

    let run = scan(&r);
    assert_eq!(run.code, 2, "stderr: {}", run.stderr);
    let l = run.lines();
    let d = find(&l, "dir", &locked);
    assert_eq!(d.get("scan").str(), "unreadable");
    assert_eq!(d.get("error").get("code").num(), 5);
    assert!(!d.get("error").get("message").str().is_empty());
    assert_eq!(find(&l, "dir", &r).get("rollup").get("logical").num(), 50);
    assert_eq!(
        find(&l, "file", &r.join("visible.txt"))
            .get("size_logical")
            .num(),
        50
    );
    let cov = summary(&l).get("coverage");
    assert_eq!(cov.get("incomplete_dirs").num(), 1);
    assert_eq!(cov.get("errors").get("5").num(), 1);
}

#[test]
fn ec16_folder_with_spaces_and_unicode() {
    let r = fixture("ec16 空白 資料夾");
    write(&r.join("a b.txt"), 4);
    let run = scan(&r);
    assert_eq!(run.code, 0, "{}", run.stderr);
    let l = run.lines();
    assert_eq!(
        find(&l, "file", &r.join("a b.txt"))
            .get("size_logical")
            .num(),
        4
    );
}

#[test]
fn ec16_unc_path_uses_walk_and_matches_local_scan() {
    let r = fixture("ec16_unc");
    write(&r.join("u.txt"), 9);
    std::fs::create_dir(r.join("sub")).unwrap();
    write(&r.join("sub").join("v.txt"), 11);
    // 以管理共用（\\localhost\F$\...）當作 UNC 路徑
    let s = r.to_string_lossy().to_string();
    let (drive, rest) = s.split_at(2);
    let unc = PathBuf::from(format!(r"\\localhost\{}${}", &drive[..1], rest));
    if std::fs::metadata(&unc).is_err() {
        eprintln!(
            "UNVERIFIED: administrative share not accessible: {}",
            unc.display()
        );
        return;
    }
    let run = scan(&unc);
    assert_eq!(run.code, 0, "{}", run.stderr);
    let l = run.lines();
    let root = find(&l, "dir", &unc);
    assert_eq!(root.get("rollup").get("files").num(), 2);
    assert_eq!(root.get("rollup").get("logical").num(), 20);
    assert!(root.get("path").str().starts_with(r"\\localhost\"));
    assert_eq!(l[0].get("engine").str(), "walk");
}

#[test]
fn ec17_unwritable_output_gives_exit_4_and_no_leftovers() {
    let r = fixture("ec17");
    write(&r.join("a.txt"), 1);
    // 輸出資料夾不存在
    let bad = r.join("no_such_dir").join("out.ndjson");
    let run = voltdir(&[
        "scan".as_ref(),
        r.as_os_str(),
        "-q".as_ref(),
        "--out".as_ref(),
        bad.as_os_str(),
    ]);
    assert_eq!(run.code, 4, "{}", run.stderr);
    assert!(!bad.exists());
    // 輸出路徑是一個既有的資料夾：暫存檔寫完後改名失敗，暫存檔必須被清除
    let dir_target = fixture("ec17_dirtarget");
    let out2 = dir_target.join("out");
    std::fs::create_dir(&out2).unwrap();
    let run = voltdir(&[
        "scan".as_ref(),
        r.as_os_str(),
        "-q".as_ref(),
        "--out".as_ref(),
        out2.as_os_str(),
    ]);
    assert_eq!(run.code, 4, "{}", run.stderr);
    let leftovers: Vec<_> = std::fs::read_dir(&dir_target)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(leftovers, [OsString::from("out")], "temp file left behind");
    // 成功時：檔案存在且完整、沒有暫存檔
    let good = r.join("ok.ndjson");
    let run = voltdir(&[
        "scan".as_ref(),
        r.as_os_str(),
        "-q".as_ref(),
        "--out".as_ref(),
        good.as_os_str(),
    ]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    let text = std::fs::read_to_string(&good).unwrap();
    assert!(
        text.lines()
            .last()
            .unwrap()
            .starts_with("{\"type\":\"summary\"")
    );
    let tmp: Vec<_> = std::fs::read_dir(&r)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains("voltdir-tmp"))
        .collect();
    assert!(tmp.is_empty());
}

#[test]
fn hardlinks_count_logical_per_name_and_physical_once() {
    let r = fixture("hardlinks");
    std::fs::create_dir(r.join("b")).unwrap();
    write(&r.join("a.bin"), 10_000);
    std::fs::hard_link(r.join("a.bin"), r.join("b").join("a2.bin")).unwrap();
    std::fs::hard_link(r.join("a.bin"), r.join("a3.bin")).unwrap();
    let run = scan(&r);
    assert_eq!(run.code, 0, "{}", run.stderr);
    let l = run.lines();
    let root = find(&l, "dir", &r);
    assert_eq!(root.get("rollup").get("files").num(), 3);
    assert_eq!(root.get("rollup").get("logical").num(), 30_000);
    assert_eq!(root.get("rollup").get("physical").num(), 12_288);
    let prim = find(&l, "file", &r.join("a.bin"));
    assert!(!prim.get("hardlink_secondary").bool());
    assert_eq!(prim.get("link_count").num(), 3);
    let sec = find(&l, "file", &r.join("a3.bin"));
    assert!(sec.get("hardlink_secondary").bool());
    assert_eq!(sec.get("size_physical").num(), 12_288);
    assert!(
        find(&l, "file", &r.join("b").join("a2.bin"))
            .get("hardlink_secondary")
            .bool()
    );
    // 次要名稱對所在目錄的 direct.physical 貢獻為 0
    assert_eq!(
        find(&l, "dir", &r.join("b"))
            .get("direct")
            .get("physical")
            .num(),
        0
    );
}

#[test]
fn junction_is_listed_but_not_entered() {
    let r = fixture("junction");
    let target = r.join("target_dir");
    std::fs::create_dir(&target).unwrap();
    write(&target.join("t.txt"), 40);
    let link = r.join("link_to_target");
    let st = Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(&link)
        .arg(&target)
        .output()
        .unwrap();
    assert!(st.status.success(), "mklink /J failed: {st:?}");
    let run = scan(&r);
    assert_eq!(run.code, 0, "{}", run.stderr);
    let l = run.lines();
    let j = find(&l, "dir", &link);
    assert_eq!(j.get("reparse").str(), "junction");
    assert_eq!(j.get("scan").str(), "not_followed");
    assert_eq!(j.get("rollup").get("files").num(), 0);
    assert!(
        l.iter()
            .filter(|x| x.get("type").str() != "meta" && x.get("type").str() != "summary")
            .all(|x| !x
                .get("path")
                .str()
                .to_lowercase()
                .contains("link_to_target\\t.txt"))
    );
    assert_eq!(
        find(&l, "dir", &r).get("rollup").get("logical").num(),
        40,
        "target counted once"
    );
    let _ = std::fs::remove_dir(&link);
}

#[test]
fn output_is_deterministic_except_times() {
    let r = fixture("determinism");
    for i in 0..30 {
        write(&r.join(format!("f{i}.dat")), i * 7);
        std::fs::create_dir(r.join(format!("d{i}"))).unwrap();
        write(&r.join(format!("d{i}")).join("x"), 3);
    }
    let strip = |s: String| -> Vec<String> {
        s.lines()
            .filter(|l| !l.starts_with("{\"type\":\"meta\""))
            .map(|l| match l.find("\"duration_ms\"") {
                Some(i) => l[..i].to_string(),
                None => l.to_string(),
            })
            .collect()
    };
    // 剛寫入的檔案，配置大小可能還在變動；先等檔案系統穩定再比較
    // 目錄的時間戳記在父目錄索引中是延遲更新的（開啟目錄後才同步），所以先掃一次當暖機
    let _ = scan(&r);
    std::thread::sleep(std::time::Duration::from_secs(2));
    let a = strip(scan(&r).text());
    let b = strip(
        voltdir(&[
            "scan".as_ref(),
            r.as_os_str(),
            "-q".as_ref(),
            "--threads".as_ref(),
            "1".as_ref(),
        ])
        .text(),
    );
    let c = strip(
        voltdir(&[
            "scan".as_ref(),
            r.as_os_str(),
            "-q".as_ref(),
            "--threads".as_ref(),
            "16".as_ref(),
        ])
        .text(),
    );
    for (name, other) in [("threads=1", &b), ("threads=16", &c)] {
        assert_eq!(a.len(), other.len(), "{name}: line count");
        if let Some(i) = (0..a.len()).find(|&i| a[i] != other[i]) {
            panic!(
                "{name}: first difference at line {i}\n  default: {}\n  other:   {}",
                a[i], other[i]
            );
        }
    }
}

#[test]
fn cli_exit_codes() {
    let r = fixture("cli");
    assert_eq!(
        voltdir(&["scan".as_ref(), r.join("missing").as_os_str()]).code,
        1
    );
    assert_eq!(voltdir(&["scan".as_ref()]).code, 1);
    assert_eq!(voltdir(&["--bogus".as_ref()]).code, 1);
    assert_eq!(
        voltdir(&[
            "scan".as_ref(),
            r.as_os_str(),
            "--engine".as_ref(),
            "mft".as_ref()
        ])
        .code,
        3
    );
    assert_eq!(
        voltdir(&["scan".as_ref(), r.as_os_str(), "--max-depth=-1".as_ref(),]).code,
        1
    );
    let f = r.join("file.txt");
    write(&f, 1);
    assert_eq!(
        voltdir(&["scan".as_ref(), f.as_os_str()]).code,
        1,
        "a file is not a valid root"
    );
    let v = voltdir(&["--version".as_ref()]);
    assert_eq!(v.code, 0);
    assert!(v.text().starts_with("voltdir "));
}

#[test]
fn info_reports_environment() {
    let run = voltdir(&["info".as_ref()]);
    assert_eq!(run.code, 0);
    let j = parse_json(run.text().trim());
    assert!(!j.get("tool_version").str().is_empty());
    assert!(!j.get("engines").get("walk").get("available").is_null());
    assert!(!j.get("engines").get("mft").get("available").bool());
    assert!(matches!(j.get("volumes"), J::Arr(v) if !v.is_empty()));
    let _ = j.get("elevated").bool();
}
