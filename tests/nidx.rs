//! 名稱索引（`<快照>.nidx`）的整合測試：用索引回答 `find --snapshot` 的輸出，
//! 必須與沒有索引時（串流讀快照）逐位元組相同；索引壞了、過期了就回退，不能答錯。

mod common;

use common::*;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

fn write(p: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, bytes).unwrap();
}

fn run(args: &[&str]) -> Run {
    let v: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    voltdir(&v)
}

fn age(p: &Path, days: u64) {
    let f = std::fs::File::options().write(true).open(p).unwrap();
    f.set_modified(SystemTime::now() - Duration::from_secs(days * 86_400))
        .unwrap();
}

/// 有各種名稱、大小、日期、空檔、空資料夾、硬連結與多層目錄的樹。
fn tree(name: &str) -> PathBuf {
    let r = fixture(name);
    write(&r.join("a.txt"), b"hello");
    write(&r.join("B.TXT"), &[b'x'; 5000]);
    write(&r.join("papers\\報告 (1).pdf"), &[b'p'; 3000]);
    write(&r.join("papers\\報告.pdf"), &[b'p'; 2000]);
    write(&r.join("papers\\😀 emoji.txt"), b"smile");
    write(&r.join("papers\\invoice\u{202e}txt.exe"), b"bidi");
    write(
        &r.join("papers\\deep\\er\\still\\IMG_0001.JPG"),
        &[b'i'; 9000],
    );
    write(
        &r.join("papers\\deep\\er\\still\\IMG_0002.jpg"),
        &[b'i'; 100],
    );
    write(&r.join("empty.log"), b"");
    write(&r.join("noext"), b"n");
    write(&r.join("dir.with.dots\\file.tar.gz"), &[b'z'; 700]);
    std::fs::create_dir_all(r.join("emptydir")).unwrap();
    std::fs::create_dir_all(r.join("papers\\emptier")).unwrap();
    std::fs::hard_link(r.join("a.txt"), r.join("papers\\a-link.txt")).unwrap();
    age(&r.join("a.txt"), 400);
    age(&r.join("papers\\報告.pdf"), 40);
    age(&r.join("papers\\deep\\er\\still\\IMG_0002.jpg"), 3);
    r
}

fn snapshot_of(roots: &[&Path], snap: &Path, extra: &[&str]) {
    // 前一次執行留下的索引不能影響這一次（索引的指紋會擋下它，但 --no-index 的測試要看到「沒有檔案」）
    let _ = std::fs::remove_file(nidx(snap));
    let mut a: Vec<&str> = vec!["scan"];
    for r in roots {
        a.push(r.to_str().unwrap());
    }
    a.extend_from_slice(&["--out", snap.to_str().unwrap(), "-q"]);
    a.extend_from_slice(extra);
    let r = run(&a);
    assert!(r.code == 0 || r.code == 2, "scan failed: {}", r.stderr);
}

fn nidx(snap: &Path) -> PathBuf {
    let mut s = snap.as_os_str().to_owned();
    s.push(".nidx");
    PathBuf::from(s)
}

fn find_in(snap: &Path, filter: &[&str]) -> Run {
    let mut a = vec!["find", "--snapshot", snap.to_str().unwrap()];
    a.extend_from_slice(filter);
    run(&a)
}

/// 同一份快照，用索引回答與不用索引（暫時搬走 `.nidx`）回答。
fn both(snap: &Path, filter: &[&str]) -> (Run, Run) {
    let with = find_in(snap, filter);
    let ix = nidx(snap);
    let parked = ix.with_extension("nidx.parked");
    std::fs::rename(&ix, &parked).unwrap();
    let without = find_in(snap, filter);
    std::fs::rename(&parked, &ix).unwrap();
    (with, without)
}

const MATRIX: &[&[&str]] = &[
    &["-q"],
    &["-q", "--type", "any"],
    &["-q", "--type", "dir"],
    &["-q", "--type", "dir", "--empty"],
    &["-q", "--empty"],
    &["-q", "--type", "any", "--empty"],
    &["-q", "--ext", ".txt"],
    &["-q", "--ext", ".pdf,.jpg"],
    &["-q", "--ext", ".gz"],
    &["-q", "--name", "*.txt"],
    &["-q", "--name", "img_*"],
    &["-q", "--name", "報告*"],
    &["-q", "--name", "*emoji*"],
    &["-q", "--type", "any", "--name", "*e*"],
    &["-q", "--name-regex", r"\(\d+\)\.\w+$"],
    &["-q", "--name-regex", "^(a|b)\\.txt$"],
    &["-q", "--name-regex", "^img_\\d{4}\\.jpe?g$"],
    &["-q", "--path", "*\\papers\\*"],
    &["-q", "--path", "*\\deep\\*", "--name", "*.jpg"],
    &["-q", "--min-size", "1000"],
    &["-q", "--max-size", "1000"],
    &["-q", "--min-size", "500", "--max-size", "5000"],
    &["-q", "--older-than", "30"],
    &["-q", "--newer-than", "30"],
    &["-q", "--older-than", "1", "--newer-than", "100"],
    &["-q", "--sort", "size"],
    &["-q", "--sort", "mtime"],
    &["-q", "--sort", "size", "--limit", "2"],
    &["-q", "--sort", "mtime", "--limit", "3", "--type", "any"],
    &["-q", "--limit", "0"],
    &["-q", "--limit", "0", "--name", "*.txt"],
    &["-q", "--limit", "1"],
    &["-q", "--limit", "5", "--name", "*.txt"],
    &["-q", "--name", "no-such-name"],
    &["-q", "--ext", ".txt", "--min-size", "4000", "--path", "*"],
];

fn assert_same(snap: &Path, filter: &[&str]) {
    let (with, without) = both(snap, filter);
    assert_eq!(with.code, without.code, "exit code for {filter:?}");
    assert_eq!(with.text(), without.text(), "stdout differs for {filter:?}");
}

#[test]
fn index_answers_equal_streaming_for_the_filter_matrix() {
    let r = tree("nidx_matrix");
    let snap = r.parent().unwrap().join("nidx_matrix.ndjson");
    snapshot_of(&[&r], &snap, &[]);
    assert!(nidx(&snap).exists(), "scan should write the name index");
    for f in MATRIX {
        assert_same(&snap, f);
    }
    // 索引真的被用到（不是兩次都走串流）
    let used = find_in(&snap, &["--name", "*.txt"]);
    assert!(
        used.stderr.contains("index: used (built"),
        "{}",
        used.stderr
    );
    assert!(used.stderr.contains("ago)"), "{}", used.stderr);
}

#[test]
fn trimmed_and_multi_root_snapshots_answer_identically() {
    let a = tree("nidx_root_a");
    let b = tree("nidx_root_b");
    let dir = a.parent().unwrap();
    let multi = dir.join("nidx_multi.ndjson");
    snapshot_of(&[&a, &b], &multi, &[]);
    let shallow = dir.join("nidx_shallow.ndjson");
    snapshot_of(&[&a], &shallow, &["--max-depth", "2"]);
    let big = dir.join("nidx_big.ndjson");
    snapshot_of(&[&a], &big, &["--min-size", "1000"]);
    for snap in [&multi, &shallow, &big] {
        for f in MATRIX {
            assert_same(snap, f);
        }
    }
    // 裁剪過的快照要照樣說明 find 的結果哪裡不完整
    let t = find_in(&shallow, &["-q"]).text();
    assert!(t.contains("\"limited_by_snapshot\":true"), "{t}");
}

#[test]
fn no_index_flag_writes_no_name_index_and_quiet_stays_silent() {
    let r = tree("nidx_flag");
    let snap = r.parent().unwrap().join("nidx_flag.ndjson");
    snapshot_of(&[&r], &snap, &["--no-index"]);
    assert!(!nidx(&snap).exists());
    let plain = find_in(&snap, &["--name", "*.txt"]);
    assert!(
        plain.stderr.contains("index: not used (no name index file"),
        "{}",
        plain.stderr
    );
    snapshot_of(&[&r], &snap, &[]);
    let quiet = find_in(&snap, &["-q", "--name", "*.txt"]);
    assert_eq!(quiet.stderr, "");
}

#[test]
fn damaged_or_stale_index_falls_back_to_the_snapshot() {
    let r = tree("nidx_damage");
    let snap = r.parent().unwrap().join("nidx_damage.ndjson");
    snapshot_of(&[&r], &snap, &[]);
    let ix = nidx(&snap);
    let good = std::fs::read(&ix).unwrap();
    let want = {
        std::fs::remove_file(&ix).unwrap();
        let w = find_in(&snap, &["-q", "--name", "*.txt"]);
        std::fs::write(&ix, &good).unwrap();
        w
    };
    // 截斷、翻轉區段中間與檔頭的一個位元組、改版本號
    let mut variants: Vec<(String, Vec<u8>)> = vec![
        ("truncated".into(), good[..good.len() / 2].to_vec()),
        ("empty".into(), Vec::new()),
    ];
    for at in [10usize, 40, 100, good.len() / 2, good.len() - 3] {
        let mut v = good.clone();
        v[at] ^= 0x20;
        variants.push((format!("flipped byte {at}"), v));
    }
    let mut ver = good.clone();
    ver[4] = 9;
    variants.push(("version".into(), ver));
    for (label, bytes) in variants {
        std::fs::write(&ix, &bytes).unwrap();
        let got = find_in(&snap, &["--name", "*.txt"]);
        assert_eq!(got.code, want.code, "{label}");
        assert!(got.stdout == want.stdout, "{label}: stdout differs");
        assert!(
            got.stderr.contains("index: not used") || got.stderr.contains("index: used"), // 位元組翻在不影響這個查詢的區段時，雜湊在讀到時才會發現
            "{label}: {}",
            got.stderr
        );
    }
    std::fs::write(&ix, &good).unwrap();
    // 快照被改過（多了一個空行：串流讀法照樣看得懂），索引就過期了
    let mut text = std::fs::read(&snap).unwrap();
    text.push(b'\n');
    std::fs::write(&snap, &text).unwrap();
    let stale = find_in(&snap, &["--name", "*.txt"]);
    assert!(
        stale
            .stderr
            .contains("the snapshot changed since the name index"),
        "{}",
        stale.stderr
    );
    assert!(
        stale.stdout == want.stdout,
        "stale index changed the answer"
    );
    // 快照與索引一起搬到別的資料夾：複製保留修改時間，索引照樣可用；不論用不用得上，答案都一樣
    let moved = snap.parent().unwrap().join("nidx_moved");
    std::fs::create_dir_all(&moved).unwrap();
    snapshot_of(&[&r], &snap, &[]);
    let (m1, m2) = (moved.join("s.ndjson"), nidx(&moved.join("s.ndjson")));
    std::fs::copy(&snap, &m1).unwrap();
    std::fs::copy(nidx(&snap), &m2).unwrap();
    let with = find_in(&m1, &["-q", "--name", "*.txt"]);
    std::fs::remove_file(&m2).unwrap();
    let plain = find_in(&m1, &["-q", "--name", "*.txt"]);
    assert_eq!(with.code, plain.code);
    assert!(with.stdout == plain.stdout, "moved snapshot answers differ");
}

#[test]
fn index_command_rebuilds_exactly_the_index_written_with_the_snapshot() {
    let a = tree("nidx_cmd_a");
    let b = tree("nidx_cmd_b");
    let snap = a.parent().unwrap().join("nidx_cmd.ndjson");
    snapshot_of(&[&a, &b], &snap, &[]);
    let written = std::fs::read(nidx(&snap)).unwrap();
    std::fs::remove_file(nidx(&snap)).unwrap();
    let r = run(&["index", snap.to_str().unwrap()]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.stdout, Vec::<u8>::new(), "index prints nothing to stdout");
    assert!(r.stderr.contains("name index written"), "{}", r.stderr);
    let rebuilt = std::fs::read(nidx(&snap)).unwrap();
    assert!(
        written == rebuilt,
        "the index rebuilt from the snapshot differs from the one written with it"
    );
    // -q 不印摘要；用重建的索引照樣逐位元組相同
    let q = run(&["index", snap.to_str().unwrap(), "-q"]);
    assert_eq!((q.code, q.stderr.as_str()), (0, ""));
    for f in MATRIX {
        assert_same(&snap, f);
    }
}

#[test]
fn index_command_refuses_bad_input_and_writes_nothing() {
    let r = tree("nidx_cmd_bad");
    let dir = r.parent().unwrap();
    let missing = dir.join("nidx_cmd_missing.ndjson");
    let _ = std::fs::remove_file(&missing);
    let e = run(&["index", missing.to_str().unwrap()]);
    assert_eq!(e.code, 1, "{}", e.stderr);
    assert!(!nidx(&missing).exists());

    let broken = dir.join("nidx_cmd_broken.ndjson");
    snapshot_of(&[&r], &broken, &["--no-index"]);
    let text = std::fs::read(&broken).unwrap();
    std::fs::write(&broken, &text[..text.len() / 2]).unwrap();
    let e = run(&["index", broken.to_str().unwrap()]);
    assert_eq!(e.code, 1, "{}", e.stderr);
    assert!(
        !nidx(&broken).exists(),
        "a truncated snapshot gets no index"
    );

    let none = run(&["index"]);
    assert_eq!(none.code, 1);
    let help = run(&["index", "--help"]);
    assert_eq!(help.code, 0);
    assert!(help.text().contains("<SNAPSHOT>.nidx"), "{}", help.text());
}
