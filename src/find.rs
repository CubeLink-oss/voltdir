//! `find`：依中繼資料（名稱、副檔名、大小、日期、類型）篩選快照中的項目。
//!
//! 即時掃描與讀取快照共用同一個 `Matcher`；輸出的 `items` 就是快照裡的 `dir`／`file` 行。
//! 這裡不會開啟任何被掃描的檔案。

use crate::glob;
use crate::json;
use crate::names::cmp_names;
use crate::regex::Regex;
use crate::snapshot::{Coverage, write_coverage};
use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeSet, BinaryHeap};
use std::io::Write;

pub const SCHEMA: &str = "voltdir.find/1";
const TICKS_PER_DAY: u64 = 864_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Any,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Path,
    Size,
    Mtime,
}

/// 篩選條件；所有指定的條件都必須同時符合。
pub struct Filter {
    pub name_glob: Option<Vec<u16>>,
    pub name_regex: Option<Regex>,
    pub path_glob: Option<Vec<u16>>,
    pub kind: Kind,
    /// 小寫、含點
    pub exts: Option<BTreeSet<Vec<u16>>>,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    /// `mtime` 早於此值（FILETIME）
    pub older_than: Option<u64>,
    /// `mtime` 不早於此值（FILETIME）
    pub newer_than: Option<u64>,
    pub empty: bool,
}

impl Filter {
    /// 把「天數」換算成相對於 `now` 的時間界線。
    pub fn cutoff(now: u64, days: u64) -> u64 {
        now.saturating_sub(days.saturating_mul(TICKS_PER_DAY))
    }
}

/// 一個候選項目的比對所需欄位。
pub struct Cand<'a> {
    pub is_dir: bool,
    pub path: &'a [u16],
    pub name: &'a [u16],
    pub ext: &'a [u16],
    pub logical: u64,
    /// 檔案：`size_physical`；目錄：`rollup.physical`
    pub physical: u64,
    /// 檔案：`mtime`；目錄：`latest_mtime`
    pub mtime: u64,
    /// 目錄：`rollup.files` 與 `rollup.dirs` 皆為 0
    pub dir_empty: bool,
}

/// 不需要名稱與路徑就能判斷的欄位（類型、副檔名、大小、日期、空）。
/// 名稱索引先用它對欄位陣列做緊湊迴圈，通過的才去比對名稱。
pub struct Meta<'a> {
    pub is_dir: bool,
    pub ext: &'a [u16],
    pub logical: u64,
    pub physical: u64,
    pub mtime: u64,
    pub dir_empty: bool,
}

impl Filter {
    /// 類型、副檔名、大小、日期、空：不需要名稱與路徑。
    pub fn matches_meta(&self, m: &Meta<'_>) -> bool {
        if let Some(exts) = &self.exts
            && (m.is_dir || !exts.contains(m.ext))
        {
            return false;
        }
        self.matches_meta_except_ext(m)
    }

    /// 同 `matches_meta`，但不看副檔名（名稱索引已用副檔名編號先判斷過）。
    pub fn matches_meta_except_ext(&self, m: &Meta<'_>) -> bool {
        match (self.kind, m.is_dir) {
            (Kind::File, true) | (Kind::Dir, false) => return false,
            _ => {}
        }
        if self.min_size.is_some_and(|v| m.physical < v)
            || self.max_size.is_some_and(|v| m.physical > v)
        {
            return false;
        }
        if self.older_than.is_some_and(|t| m.mtime >= t)
            || self.newer_than.is_some_and(|t| m.mtime < t)
        {
            return false;
        }
        if self.empty {
            let is_empty = if m.is_dir {
                m.dir_empty
            } else {
                m.logical == 0
            };
            if !is_empty {
                return false;
            }
        }
        true
    }

    /// 比對大量名稱時用的暫存空間（沒有 `--name-regex` 時是 `None`）。
    pub fn name_scratch(&self) -> Option<crate::regex::Scratch> {
        self.name_regex.as_ref().map(Regex::scratch)
    }

    /// 同 `matches_name`，但重複使用正規表示式的暫存空間。
    pub fn matches_name_with(
        &self,
        name: &[u16],
        scratch: &mut Option<crate::regex::Scratch>,
    ) -> bool {
        if let Some(g) = &self.name_glob
            && !glob::matches(g, name)
        {
            return false;
        }
        match (&self.name_regex, scratch.as_mut()) {
            (Some(r), Some(s)) => r.is_match_with(name, s),
            (Some(r), None) => r.is_match(name),
            (None, _) => true,
        }
    }

    /// `--name` 與 `--name-regex`。
    pub fn matches_name(&self, name: &[u16]) -> bool {
        if let Some(g) = &self.name_glob
            && !glob::matches(g, name)
        {
            return false;
        }
        if let Some(r) = &self.name_regex
            && !r.is_match(name)
        {
            return false;
        }
        true
    }

    /// `--path`。
    pub fn matches_path(&self, path: &[u16]) -> bool {
        match &self.path_glob {
            Some(g) => glob::matches(g, path),
            None => true,
        }
    }

    /// 有 `--path` 條件時才一定要先重建路徑。
    pub fn needs_path(&self) -> bool {
        self.path_glob.is_some()
    }

    pub fn matches(&self, c: &Cand<'_>) -> bool {
        self.matches_meta(&Meta {
            is_dir: c.is_dir,
            ext: c.ext,
            logical: c.logical,
            physical: c.physical,
            mtime: c.mtime,
            dir_empty: c.dir_empty,
        }) && self.matches_name(c.name)
            && self.matches_path(c.path)
    }
}

struct Item {
    key: u64,
    path: Vec<u16>,
    raw: Vec<u8>,
}

impl PartialEq for Item {
    fn eq(&self, o: &Self) -> bool {
        self.cmp(o) == Ordering::Equal
    }
}
impl Eq for Item {}
impl PartialOrd for Item {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Item {
    /// 越大越前面：鍵較大者優先；鍵相同時路徑較小者優先（確保結果固定）。
    fn cmp(&self, o: &Self) -> Ordering {
        self.key
            .cmp(&o.key)
            .then_with(|| cmp_names(&o.path, &self.path))
    }
}

pub struct Matcher {
    filter: Filter,
    sort: Sort,
    limit: usize,
    matched: u64,
    heap: BinaryHeap<Reverse<Item>>,
}

impl Matcher {
    pub fn new(filter: Filter, sort: Sort, limit: usize) -> Matcher {
        Matcher {
            filter,
            sort,
            limit,
            matched: 0,
            heap: BinaryHeap::new(),
        }
    }

    fn key_of(&self, c: &Cand<'_>) -> u64 {
        match self.sort {
            Sort::Path => 0,
            Sort::Size => c.physical,
            Sort::Mtime => c.mtime,
        }
    }

    /// 通過篩選的項目才會呼叫 `raw` 產生輸出行；容器已滿且贏不過最差者時也不會呼叫。
    pub fn offer(&mut self, c: &Cand<'_>, raw: impl FnOnce() -> Vec<u8>) {
        if !self.filter.matches(c) {
            return;
        }
        self.offer_matched(c, raw);
    }

    pub fn filter(&self) -> &Filter {
        &self.filter
    }

    pub fn sort(&self) -> Sort {
        self.sort
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    /// 呼叫者已經確認 `c` 通過篩選（名稱索引先做便宜的條件，最後才組出路徑）。
    pub fn offer_matched(&mut self, c: &Cand<'_>, raw: impl FnOnce() -> Vec<u8>) {
        self.matched += 1;
        if self.limit == 0 {
            return;
        }
        let key = self.key_of(c);
        let wins = self.heap.len() < self.limit
            || self.heap.peek().is_some_and(|w| {
                key > w.0.key || (key == w.0.key && cmp_names(c.path, &w.0.path) == Ordering::Less)
            });
        if !wins {
            return;
        }
        if self.heap.len() >= self.limit {
            self.heap.pop();
        }
        self.heap.push(Reverse(Item {
            key,
            path: c.path.to_vec(),
            raw: raw(),
        }));
    }

    /// 回傳（符合的總筆數，依排序的輸出行，是否被 `--limit` 截斷）。
    pub fn finish(self) -> (u64, Vec<Vec<u8>>, bool) {
        let mut items: Vec<Item> = self.heap.into_iter().map(|r| r.0).collect();
        items.sort_by(|a, b| b.cmp(a));
        let truncated = self.matched > items.len() as u64;
        (
            self.matched,
            items.into_iter().map(|i| i.raw).collect(),
            truncated,
        )
    }
}

/// 快照被 `--min-size`／`--max-depth` 剪裁時，說明 `find` 的結果哪裡不完整。
pub fn limited_reasons(min_size: Option<u64>, max_depth: Option<u64>, kind: Kind) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(m) = min_size
        && kind != Kind::Dir
    {
        v.push(format!(
            "snapshot was made with --min-size {m}: files smaller than that are not in the snapshot, so file results cover only the listed files"
        ));
    }
    if let Some(d) = max_depth {
        v.push(format!(
            "snapshot was made with --max-depth {d}: entries deeper than that are not in the snapshot, so results cover only the listed entries"
        ));
    }
    v
}

pub struct Frame<'a> {
    pub meta_body: &'a [u8],
    pub matched_total: u64,
    pub items: &'a [Vec<u8>],
    pub truncated: bool,
    pub coverage: &'a Coverage,
    pub limited_reasons: &'a [String],
}

pub fn render(f: &Frame<'_>) -> Vec<u8> {
    let mut out = Vec::with_capacity(f.items.iter().map(|i| i.len() + 1).sum::<usize>() + 2048);
    out.extend_from_slice(b"{\"schema\":");
    json::push_str(&mut out, SCHEMA);
    out.extend_from_slice(b",\"meta\":");
    out.extend_from_slice(f.meta_body);
    let _ = write!(out, ",\"matched_total\":{},\"items\":[", f.matched_total);
    for (i, raw) in f.items.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.extend_from_slice(raw);
    }
    let _ = write!(
        out,
        "],\"truncated\":{},\"limited_by_snapshot\":{},\"limited_reasons\":[",
        f.truncated,
        !f.limited_reasons.is_empty()
    );
    for (i, r) in f.limited_reasons.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        json::push_str(&mut out, r);
    }
    out.extend_from_slice(b"],\"coverage\":");
    write_coverage(&mut out, f.coverage);
    out.extend_from_slice(b"}\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    fn filter() -> Filter {
        Filter {
            name_glob: None,
            name_regex: None,
            path_glob: None,
            kind: Kind::File,
            exts: None,
            min_size: None,
            max_size: None,
            older_than: None,
            newer_than: None,
            empty: false,
        }
    }

    struct C {
        is_dir: bool,
        path: Vec<u16>,
        name: Vec<u16>,
        ext: Vec<u16>,
        logical: u64,
        physical: u64,
        mtime: u64,
        dir_empty: bool,
    }

    fn file(path: &str, physical: u64, mtime: u64) -> C {
        let p = w(path);
        let name = w(path.rsplit('\\').next().unwrap());
        C {
            is_dir: false,
            ext: crate::names::ext_of(&name),
            path: p,
            name,
            logical: physical,
            physical,
            mtime,
            dir_empty: false,
        }
    }

    fn cand(c: &C) -> Cand<'_> {
        Cand {
            is_dir: c.is_dir,
            path: &c.path,
            name: &c.name,
            ext: &c.ext,
            logical: c.logical,
            physical: c.physical,
            mtime: c.mtime,
            dir_empty: c.dir_empty,
        }
    }

    #[test]
    fn filters_combine_with_and() {
        let mut f = filter();
        f.name_glob = Some(w("*.LOG"));
        f.min_size = Some(100);
        f.exts = Some([w(".log")].into_iter().collect());
        assert!(f.matches(&cand(&file(r"C:\a\x.log", 100, 0))));
        assert!(!f.matches(&cand(&file(r"C:\a\x.log", 99, 0))));
        assert!(!f.matches(&cand(&file(r"C:\a\x.txt", 500, 0))));
        f.max_size = Some(200);
        assert!(!f.matches(&cand(&file(r"C:\a\x.log", 201, 0))));
    }

    #[test]
    fn dates_are_relative_to_now() {
        let now = 1_000 * TICKS_PER_DAY;
        let mut f = filter();
        f.older_than = Some(Filter::cutoff(now, 30));
        assert!(f.matches(&cand(&file(r"C:\o", 1, now - 31 * TICKS_PER_DAY))));
        assert!(!f.matches(&cand(&file(r"C:\n", 1, now - 29 * TICKS_PER_DAY))));
        let mut g = filter();
        g.newer_than = Some(Filter::cutoff(now, 30));
        assert!(g.matches(&cand(&file(r"C:\n", 1, now - 29 * TICKS_PER_DAY))));
        assert!(!g.matches(&cand(&file(r"C:\o", 1, now - 31 * TICKS_PER_DAY))));
    }

    #[test]
    fn empty_and_kind_rules() {
        let mut f = filter();
        f.kind = Kind::Any;
        f.empty = true;
        let mut z = file(r"C:\z", 0, 0);
        z.logical = 0;
        assert!(f.matches(&cand(&z)));
        let nz = file(r"C:\nz", 5, 0);
        assert!(!f.matches(&cand(&nz)));
        let mut d = file(r"C:\d", 0, 0);
        d.is_dir = true;
        d.dir_empty = true;
        assert!(f.matches(&cand(&d)));
        d.dir_empty = false;
        assert!(!f.matches(&cand(&d)));
        // --ext 只適用檔案：目錄不符合
        let mut e = filter();
        e.kind = Kind::Any;
        e.exts = Some([w(".txt")].into_iter().collect());
        let mut dd = file(r"C:\d.txt", 0, 0);
        dd.is_dir = true;
        assert!(!e.matches(&cand(&dd)));
    }

    #[test]
    fn path_name_and_regex_matching() {
        let mut f = filter();
        f.path_glob = Some(w(r"C:\Users\*\AppData\*"));
        assert!(f.matches(&cand(&file(r"C:\Users\bob\AppData\x.bin", 1, 0))));
        assert!(!f.matches(&cand(&file(r"C:\Users\bob\Docs\x.bin", 1, 0))));
        let mut r = filter();
        r.name_regex = Some(Regex::new(&w(r"^img_\d+\.(png|jpg)$")).unwrap());
        assert!(r.matches(&cand(&file(r"C:\p\IMG_001.PNG", 1, 0))));
        assert!(!r.matches(&cand(&file(r"C:\p\img_a.png", 1, 0))));
    }

    #[test]
    fn split_filter_equals_whole() {
        let mut f = filter();
        f.kind = Kind::Any;
        f.name_glob = Some(w("*.LOG"));
        f.min_size = Some(100);
        f.path_glob = Some(w(r"C:\a\*"));
        f.exts = Some([w(".log")].into_iter().collect());
        let cands = [
            file(r"C:\a\x.log", 100, 0),
            file(r"C:\a\x.log", 99, 0),
            file(r"C:\b\x.log", 500, 0),
            file(r"C:\a\x.txt", 500, 0),
            file(r"C:\a\y.LOG", 500, 0),
        ];
        for c in &cands {
            let cc = cand(c);
            let parts = f.matches_meta(&Meta {
                is_dir: cc.is_dir,
                ext: cc.ext,
                logical: cc.logical,
                physical: cc.physical,
                mtime: cc.mtime,
                dir_empty: cc.dir_empty,
            }) && f.matches_name(cc.name)
                && f.matches_path(cc.path);
            assert_eq!(f.matches(&cc), parts, "{:?}", c.path);
        }
        assert!(f.needs_path());
        assert!(!filter().needs_path());
    }

    #[test]
    fn sorting_limit_and_total() {
        let mut f = Matcher::new(filter(), Sort::Size, 2);
        for (p, s) in [("C:\\a", 10), ("C:\\b", 30), ("C:\\c", 20), ("C:\\d", 30)] {
            let c = file(p, s, 0);
            f.offer(&cand(&c), || p.as_bytes().to_vec());
        }
        let (total, items, truncated) = f.finish();
        assert_eq!(total, 4);
        assert!(truncated);
        // 大小遞減；相同時路徑小者在前
        assert_eq!(items, vec![b"C:\\b".to_vec(), b"C:\\d".to_vec()]);
        // limit 0：只有總數
        let mut z = Matcher::new(filter(), Sort::Path, 0);
        let c = file("C:\\a", 1, 0);
        z.offer(&cand(&c), || panic!("raw must not be built"));
        let (t, i, tr) = z.finish();
        assert_eq!((t, i.len(), tr), (1, 0, true));
        // 依路徑排序
        let mut p = Matcher::new(filter(), Sort::Path, 10);
        for n in ["C:\\b", "C:\\A", "C:\\c"] {
            let c = file(n, 1, 0);
            p.offer(&cand(&c), || n.as_bytes().to_vec());
        }
        let (_, items, _) = p.finish();
        assert_eq!(
            items,
            vec![b"C:\\A".to_vec(), b"C:\\b".to_vec(), b"C:\\c".to_vec()]
        );
    }
}
