//! M3 整合測試：`--engine` 的選擇與不可用時的行為。
//! 需要提權與夾具磁碟區的比對在 `scripts/New-FixtureVolume.ps1`＋`scripts/Compare-Engines.ps1`，
//! 這裡只在已提權且設定 `VOLTDIR_FIXTURE_VOLUME`（例如 `V:\`）時才做磁碟區冒煙測試。

mod common;

use common::*;

fn is_elevated() -> bool {
    let r = voltdir(&["info".as_ref()]);
    assert_eq!(r.code, 0);
    r.get_elevated()
}

trait ElevatedExt {
    fn get_elevated(&self) -> bool;
}

impl ElevatedExt for Run {
    fn get_elevated(&self) -> bool {
        let j = parse_json(self.text().trim());
        j.get("elevated").bool()
    }
}

#[test]
fn mft_on_a_subfolder_is_unavailable_exit_3() {
    let r = fixture("m3_sub");
    let run = voltdir(&[
        "scan".as_ref(),
        r.as_os_str(),
        "--engine".as_ref(),
        "mft".as_ref(),
        "-q".as_ref(),
    ]);
    assert_eq!(run.code, 3, "{}", run.stderr);
    assert!(run.stdout.is_empty(), "no output may be written");
    assert!(run.stderr.contains("whole-volume"), "{}", run.stderr);
}

#[test]
fn mft_with_follow_links_is_unavailable_exit_3() {
    let r = fixture("m3_follow");
    let run = voltdir(&[
        "scan".as_ref(),
        r.as_os_str(),
        "--engine".as_ref(),
        "mft".as_ref(),
        "--follow-links".as_ref(),
        "junction".as_ref(),
        "-q".as_ref(),
    ]);
    assert_eq!(run.code, 3, "{}", run.stderr);
    assert!(run.stdout.is_empty());
}

#[test]
fn unprivileged_mft_on_a_volume_root_exits_3_without_output() {
    if is_elevated() {
        return; // 已提權時這個情境不成立
    }
    let run = voltdir(&[
        "scan".as_ref(),
        "C:\\".as_ref(),
        "--engine".as_ref(),
        "mft".as_ref(),
        "-q".as_ref(),
    ]);
    assert_eq!(run.code, 3, "{}", run.stderr);
    assert!(run.stdout.is_empty());
    assert!(
        run.stderr.contains("administrator rights"),
        "{}",
        run.stderr
    );
}

#[test]
fn auto_uses_walk_for_a_subfolder_and_reports_it_in_meta() {
    let r = fixture("m3_auto");
    std::fs::write(r.join("a.txt"), b"hello").unwrap();
    let run = scan(&r);
    assert_eq!(run.code, 0, "{}", run.stderr);
    let l = run.lines();
    assert_eq!(l[0].get("type").str(), "meta");
    assert_eq!(l[0].get("engine").str(), "walk");
}

#[test]
fn info_explains_why_mft_is_unavailable_when_not_elevated() {
    if is_elevated() {
        return;
    }
    let run = voltdir(&["info".as_ref()]);
    let j = parse_json(run.text().trim());
    let mft = j.get("engines").get("mft");
    assert!(!mft.get("available").bool());
    assert!(mft.get("reason").str().contains("administrator"));
    assert!(j.get("engines").get("walk").get("available").bool());
}

#[test]
fn elevated_mft_scan_of_the_fixture_volume_lists_reserved_items() {
    let Ok(vol) = std::env::var("VOLTDIR_FIXTURE_VOLUME") else {
        return;
    };
    if !is_elevated() {
        return;
    }
    let run = voltdir(&[
        "scan".as_ref(),
        vol.as_ref(),
        "--engine".as_ref(),
        "mft".as_ref(),
        "-q".as_ref(),
    ]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    let l = run.lines();
    assert_eq!(l[0].get("engine").str(), "mft");
    let reserved: Vec<&str> = l
        .iter()
        .filter(|x| x.get("type").str() == "file" && x.get("reserved").bool())
        .map(|x| x.get("name").str())
        .collect();
    assert!(reserved.contains(&"$MFT"), "{reserved:?}");
    assert!(reserved.contains(&"$LogFile"), "{reserved:?}");
}
