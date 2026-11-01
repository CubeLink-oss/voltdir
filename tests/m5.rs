//! M5：硬連結與部分失敗的結束代碼（B-15）。

mod common;

use common::*;
use std::path::Path;
use std::process::Command;

fn write(p: &Path, bytes: usize) {
    std::fs::write(p, vec![b'x'; bytes]).unwrap();
}

fn junction(link: &Path, target: &Path) {
    let st = Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .unwrap();
    assert!(st.status.success(), "mklink /J failed: {st:?}");
}

#[test]
fn ec10_hardlinks_in_same_dir_and_across_dirs() {
    let r = fixture("m5_ec10");
    std::fs::create_dir(r.join("sub")).unwrap();
    write(&r.join("a.bin"), 10_000);
    std::fs::hard_link(r.join("a.bin"), r.join("a2.bin")).unwrap();
    std::fs::hard_link(r.join("a.bin"), r.join("sub").join("a3.bin")).unwrap();
    let run = scan(&r);
    assert_eq!(run.code, 0, "{}", run.stderr);
    let l = run.lines();
    let root = find(&l, "dir", &r);
    // 邏輯大小每個名稱都計入；實際大小只計一次
    assert_eq!(root.get("rollup").get("files").num(), 3);
    assert_eq!(root.get("rollup").get("logical").num(), 30_000);
    assert_eq!(root.get("rollup").get("physical").num(), 12_288);
    // 恰有一個主要名稱，其餘為次要名稱；每個名稱都帶有硬連結數 3
    let names = ["a.bin", "a2.bin"];
    let mut primaries = 0;
    for n in names {
        let f = find(&l, "file", &r.join(n));
        assert_eq!(f.get("link_count").num(), 3);
        assert_eq!(f.get("size_logical").num(), 10_000);
        if !f.get("hardlink_secondary").bool() {
            primaries += 1;
        }
    }
    let f3 = find(&l, "file", &r.join("sub").join("a3.bin"));
    if !f3.get("hardlink_secondary").bool() {
        primaries += 1;
    }
    assert_eq!(primaries, 1);
    // 次要名稱所在目錄的 direct.physical 為 0（資料只算在主要名稱那裡）
    let sub = find(&l, "dir", &r.join("sub"));
    assert_eq!(sub.get("direct").get("logical").num(), 10_000);
    assert_eq!(sub.get("direct").get("physical").num(), 0);
}

#[test]
fn d02_dedupe_applies_within_the_scanned_scope_only() {
    // 掃描子資料夾時只看得到該範圍內的名稱：範圍內只有一個名稱就不算次要
    let r = fixture("m5_scope");
    std::fs::create_dir(r.join("in")).unwrap();
    std::fs::create_dir(r.join("out")).unwrap();
    write(&r.join("in").join("a.bin"), 10_000);
    std::fs::hard_link(r.join("in").join("a.bin"), r.join("out").join("b.bin")).unwrap();
    let run = scan(&r.join("in"));
    assert_eq!(run.code, 0, "{}", run.stderr);
    let l = run.lines();
    let f = find(&l, "file", &r.join("in").join("a.bin"));
    assert!(!f.get("hardlink_secondary").bool());
    assert_eq!(f.get("size_physical").num(), 12_288);
    let root = find(&l, "dir", &r.join("in"));
    assert_eq!(root.get("rollup").get("physical").num(), 12_288);
}

#[test]
fn b11_names_behind_a_junction_are_not_seen_by_hardlink_detection() {
    // 接合點的目標只能經由接合點到達的名稱不會被進入，所以不會變成次要名稱
    let r = fixture("m5_b11");
    std::fs::create_dir(r.join("real")).unwrap();
    std::fs::create_dir(r.join("hidden_target")).unwrap();
    write(&r.join("real").join("a.bin"), 10_000);
    std::fs::hard_link(
        r.join("real").join("a.bin"),
        r.join("hidden_target").join("b.bin"),
    )
    .unwrap();
    // 掃描根 = in；in\real 是接合點，指到根外的 real
    let root = r.join("in");
    std::fs::create_dir(&root).unwrap();
    junction(&root.join("via"), &r.join("real"));
    write(&root.join("c.bin"), 10_000);
    std::fs::hard_link(root.join("c.bin"), r.join("hidden_target").join("c2.bin")).unwrap();
    let run = scan(&root);
    assert_eq!(run.code, 0, "{}", run.stderr);
    let l = run.lines();
    let c = find(&l, "file", &root.join("c.bin"));
    assert!(!c.get("hardlink_secondary").bool());
    let via = find(&l, "dir", &root.join("via"));
    assert_eq!(via.get("scan").str(), "not_followed");
    assert_eq!(via.get("rollup").get("files").num(), 0);
    // 根的合計只有 c.bin 一個檔案
    let rootn = find(&l, "dir", &root);
    assert_eq!(rootn.get("rollup").get("files").num(), 1);
    assert_eq!(rootn.get("rollup").get("physical").num(), 12_288);
}

#[test]
fn b15_unreadable_directory_gives_exit_2_and_is_reported() {
    // 基準在同樣情況下結束代碼為 0（黑箱觀察，見 CONFORMANCE）；voltdir 刻意不同。
    // 實際製造「存取被拒」的整合測試是 ec04_*；這裡確認 summary 的涵蓋範圍欄位與結束代碼一致。
    let r = fixture("m5_b15");
    let denied = r.join("denied");
    std::fs::create_dir(&denied).unwrap();
    write(&denied.join("secret.bin"), 5_000);
    write(&r.join("ok.bin"), 3_000);
    let user = std::env::var("USERNAME").unwrap();
    let deny = |on: bool| {
        let mut c = Command::new("icacls");
        c.arg(&denied);
        if on {
            c.arg("/deny").arg(format!("{user}:(OI)(CI)(RX)"));
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
    let run = scan(&r);
    assert_eq!(run.code, 2, "{}", run.stderr);
    let l = run.lines();
    let d = find(&l, "dir", &denied);
    assert_eq!(d.get("scan").str(), "unreadable");
    assert_eq!(d.get("error").get("code").num(), 5);
    let cov = summary(&l).get("coverage");
    assert_eq!(cov.get("incomplete_dirs").num(), 1);
    assert_eq!(find(&l, "dir", &r).get("rollup").get("files").num(), 1);
}
