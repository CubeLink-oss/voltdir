//! `report`：以串流方式彙總（最大檔案、最大目錄、依副檔名、檔案數最多的目錄）。
//!
//! 即時掃描與讀取快照共用同一個 `Analyzer`，因此兩條路徑的結果在建構上就一致。

use crate::fxhash::FxHashMap;
use crate::json;
use crate::model::Totals;
use crate::names::cmp_names;
use crate::snapshot::{Coverage, write_coverage};
use crate::timefmt;
use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeSet, BinaryHeap};
use std::io::Write;

pub const SCHEMA: &str = "voltdir.report/1";
const TICKS_PER_DAY: u64 = 864_000_000_000;

#[derive(Default, Clone)]
pub struct Request {
    pub top_files: Option<usize>,
    pub top_dirs: Option<usize>,
    pub by_ext: Option<usize>,
    pub many_files: Option<usize>,
    pub min_size: Option<u64>,
    /// 小寫、含點
    pub exts: Option<BTreeSet<Vec<u16>>>,
    pub older_than_days: Option<u64>,
    pub dir_depth: Option<u64>,
    /// 「現在」的 FILETIME，供 `--older-than` 使用
    pub now: u64,
}

pub struct FileView<'a> {
    pub path: &'a [u16],
    pub ext: &'a [u16],
    pub logical: u64,
    pub physical: u64,
    pub mtime: u64,
    pub secondary: bool,
}

pub struct DirView<'a> {
    pub path: &'a [u16],
    pub depth: u64,
    pub rollup: Totals,
    pub direct: Totals,
    pub latest_mtime: u64,
}

/// 走訪快照項目的接收端；`report` 與 `overview` 共用同一條讀取路徑。
pub trait Sink {
    fn dir(&mut self, d: &DirView<'_>);
    fn file(&mut self, f: &FileView<'_>);
}

impl Tallier for Analyzer {
    fn tally(&mut self, t: &Tally<'_>, add: bool) {
        if let Some(m) = &mut self.by_ext {
            if add {
                if !m.contains_key(t.ext) {
                    m.insert(t.ext.to_vec(), ExtAgg::default());
                }
                let e = m.get_mut(t.ext).expect("inserted above");
                e.files += 1;
                e.logical += t.logical;
                if !t.secondary {
                    e.physical += t.physical;
                }
            } else if let Some(e) = m.get_mut(t.ext) {
                e.files -= 1;
                e.logical -= t.logical;
                if !t.secondary {
                    e.physical -= t.physical;
                }
                if e.files == 0 {
                    m.remove(t.ext);
                }
            }
        }
        if self.file_ok(t) {
            if add {
                self.cand_files += 1;
            } else {
                self.cand_files -= 1;
            }
        }
    }

    fn admit(&mut self, t: &Tally<'_>) -> bool {
        if !self.file_ok(t) {
            return false;
        }
        self.floor.as_mut().is_some_and(|f| f.admit(t.physical))
    }

    fn offer(&mut self, f: &FileView<'_>) {
        self.replay_top_file(f);
    }

    fn forget(&mut self, t: &Tally<'_>) -> bool {
        if !self.file_ok(t) {
            return true;
        }
        self.floor.as_mut().is_none_or(|f| f.forget(t.physical))
    }

    fn finish_files(&mut self) {
        if let Some(t) = &mut self.top_files {
            t.set_candidates(self.cand_files);
        }
    }

    fn reset(&mut self) {
        *self = Analyzer::new(self.req.clone());
    }
}

impl Sink for Analyzer {
    fn dir(&mut self, d: &DirView<'_>) {
        Analyzer::dir(self, d);
    }
    fn file(&mut self, f: &FileView<'_>) {
        Analyzer::file(self, f);
    }
}

/// 檔案層級的彙總資訊（不含路徑）。`mft` 的緊湊路徑在解析記錄時還不知道路徑（父目錄可能還沒讀到），
/// 所以把「計入彙總」「是否可能進入前 N 名」「真的放入前 N 名」拆成三個動作。
pub struct Tally<'a> {
    pub ext: &'a [u16],
    pub logical: u64,
    pub physical: u64,
    pub mtime: u64,
    pub secondary: bool,
}

/// 只記錄鍵值的前 K 名門檻：用來在不知道路徑時，先判斷一個檔案有沒有可能進入前 K 名。
/// 同分一律放行（同分的先後要看路徑），所以它是寬鬆的（候選一定包含真正的前 K 名）。
pub struct Floor {
    k: usize,
    heap: BinaryHeap<Reverse<u64>>,
    /// 曾經有檔案因為鍵值低於門檻而被擋下
    rejected: bool,
}

impl Floor {
    pub fn new(k: usize) -> Floor {
        Floor {
            k,
            heap: BinaryHeap::new(),
            rejected: false,
        }
    }

    /// 之後發現某個已放行的檔案其實不算數（孤兒）：把它的鍵值從門檻裡拿掉。
    /// 回傳 `true` 表示拿掉之後結果仍然正確——也就是它沒有佔著前 K 名的位置，
    /// 或是從來沒有檔案被擋下過（那就沒有「本來該進來卻被擋下」的檔案）。
    pub fn forget(&mut self, key: u64) -> bool {
        let mut v = std::mem::take(&mut self.heap).into_vec();
        let present = if let Some(i) = v.iter().position(|r| r.0 == key) {
            v.swap_remove(i);
            true
        } else {
            false
        };
        self.heap = BinaryHeap::from(v);
        !(present && self.rejected)
    }

    pub fn admit(&mut self, key: u64) -> bool {
        if self.heap.len() < self.k {
            self.heap.push(Reverse(key));
            return true;
        }
        match self.heap.peek() {
            Some(Reverse(min)) if key < *min => {
                self.rejected = true;
                false
            }
            Some(Reverse(min)) => {
                if key > *min {
                    self.heap.pop();
                    self.heap.push(Reverse(key));
                }
                true
            }
            None => false, // k = 0
        }
    }
}

/// 可接受「先計入、後放入」的接收端（`mft` 緊湊路徑用）。
pub trait Tallier: Sink {
    /// 把一個檔案計入（`add`）或退出（`!add`）彙總：副檔名統計、空檔案數、前 N 名的候選總數。
    fn tally(&mut self, t: &Tally<'_>, add: bool);
    /// 這個檔案有可能進入任何一個前 N 名清單嗎？（會順便更新內部門檻）
    fn admit(&mut self, t: &Tally<'_>) -> bool;
    /// 放入前 N 名清單（已知路徑之後）。不計入彙總。
    fn offer(&mut self, f: &FileView<'_>);
    /// 先前 `admit` 放行的檔案其實不算數（孤兒）：回傳拿掉它之後結果是否仍然正確。
    fn forget(&mut self, t: &Tally<'_>) -> bool;
    /// 全部檔案處理完：把候選總數寫進各清單（決定 `truncated`）。
    fn finish_files(&mut self);
    /// 放棄目前的結果，回到剛建立的狀態（緊湊路徑放棄、改走原本的路徑時用）。
    fn reset(&mut self);
}

/// 保留前 K 名的容器：項目以 `Ord` 排序，越大越前面。
pub(crate) struct TopK<T: Ord> {
    k: usize,
    heap: BinaryHeap<Reverse<T>>,
    candidates: u64,
}

impl<T: Ord> TopK<T> {
    pub(crate) fn new(k: usize) -> Self {
        TopK {
            k,
            heap: BinaryHeap::new(),
            candidates: 0,
        }
    }

    /// `beats_worst` 在容器已滿時判斷候選是否勝過目前最差者；通過才會呼叫 `make` 配置項目。
    pub(crate) fn offer(&mut self, beats_worst: impl FnOnce(&T) -> bool, make: impl FnOnce() -> T) {
        self.candidates += 1;
        if self.heap.len() < self.k {
            self.heap.push(Reverse(make()));
        } else if self.heap.peek().is_some_and(|w| beats_worst(&w.0)) {
            self.heap.pop();
            self.heap.push(Reverse(make()));
        }
    }

    /// 索引重播用：重播只餵入前幾名，真正的候選總數由索引記錄。
    pub(crate) fn set_candidates(&mut self, n: u64) {
        self.candidates = n;
    }

    pub(crate) fn truncated(&self) -> bool {
        self.candidates > self.k as u64
    }

    pub(crate) fn into_sorted(self) -> Vec<T> {
        let mut v: Vec<T> = self.heap.into_iter().map(|r| r.0).collect();
        v.sort_by(|a, b| b.cmp(a));
        v
    }
}

struct FileItem {
    path: Vec<u16>,
    ext: Vec<u16>,
    logical: u64,
    physical: u64,
    mtime: u64,
}

/// 實際大小大者優先；相同時路徑（不分大小寫的序數比較）小者優先。
pub(crate) fn rank<'a>(key: u64, path: &'a [u16]) -> impl Fn(u64, &[u16]) -> bool + 'a {
    move |wk, wp| key > wk || (key == wk && cmp_names(path, wp) == Ordering::Less)
}

impl PartialEq for FileItem {
    fn eq(&self, o: &Self) -> bool {
        self.cmp(o) == Ordering::Equal
    }
}
impl Eq for FileItem {}
impl PartialOrd for FileItem {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for FileItem {
    fn cmp(&self, o: &Self) -> Ordering {
        self.physical
            .cmp(&o.physical)
            .then_with(|| cmp_names(&o.path, &self.path))
    }
}

struct DirItem {
    path: Vec<u16>,
    depth: u64,
    rollup: Totals,
    latest_mtime: u64,
}

impl PartialEq for DirItem {
    fn eq(&self, o: &Self) -> bool {
        self.cmp(o) == Ordering::Equal
    }
}
impl Eq for DirItem {}
impl PartialOrd for DirItem {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for DirItem {
    fn cmp(&self, o: &Self) -> Ordering {
        self.rollup
            .physical
            .cmp(&o.rollup.physical)
            .then_with(|| cmp_names(&o.path, &self.path))
    }
}

struct ManyItem {
    path: Vec<u16>,
    direct_files: u64,
    direct_logical: u64,
}

impl PartialEq for ManyItem {
    fn eq(&self, o: &Self) -> bool {
        self.cmp(o) == Ordering::Equal
    }
}
impl Eq for ManyItem {}
impl PartialOrd for ManyItem {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for ManyItem {
    fn cmp(&self, o: &Self) -> Ordering {
        self.direct_files
            .cmp(&o.direct_files)
            .then_with(|| cmp_names(&o.path, &self.path))
    }
}

#[derive(Default)]
struct ExtAgg {
    files: u64,
    logical: u64,
    physical: u64,
}

pub struct Analyzer {
    req: Request,
    older_cutoff: Option<u64>,
    top_files: Option<TopK<FileItem>>,
    top_dirs: Option<TopK<DirItem>>,
    many_files: Option<TopK<ManyItem>>,
    by_ext: Option<FxHashMap<Vec<u16>, ExtAgg>>,
    /// `Tallier` 路徑：通過篩選的檔案數與前 N 名門檻
    cand_files: u64,
    floor: Option<Floor>,
}

impl Analyzer {
    pub fn new(req: Request) -> Analyzer {
        let older_cutoff = req
            .older_than_days
            .map(|d| req.now.saturating_sub(d.saturating_mul(TICKS_PER_DAY)));
        Analyzer {
            top_files: req.top_files.map(TopK::new),
            top_dirs: req.top_dirs.map(TopK::new),
            many_files: req.many_files.map(TopK::new),
            by_ext: req.by_ext.map(|_| FxHashMap::default()),
            older_cutoff,
            cand_files: 0,
            floor: req.top_files.map(Floor::new),
            req,
        }
    }

    /// 這個檔案是否是「最大檔案」清單的候選（硬連結次要名稱與篩選條件）。
    fn file_ok(&self, t: &Tally<'_>) -> bool {
        self.top_files.is_some()
            && !t.secondary
            && self.req.min_size.is_none_or(|m| t.physical >= m)
            && self.req.exts.as_ref().is_none_or(|s| s.contains(t.ext))
            && self.older_cutoff.is_none_or(|c| t.mtime < c)
    }

    pub fn dir(&mut self, d: &DirView<'_>) {
        self.replay_top_dir(d);
        self.replay_many(d);
    }

    /// 只處理「最大目錄」的部分（也是索引重播的入口）。
    pub fn replay_top_dir(&mut self, d: &DirView<'_>) {
        if let Some(t) = &mut self.top_dirs
            && self.req.dir_depth.is_none_or(|max| d.depth <= max)
        {
            let key = d.rollup.physical;
            t.offer(
                |w| rank(key, d.path)(w.rollup.physical, &w.path),
                || DirItem {
                    path: d.path.to_vec(),
                    depth: d.depth,
                    rollup: d.rollup,
                    latest_mtime: d.latest_mtime,
                },
            );
        }
    }

    /// 只處理「直接檔案數最多的目錄」的部分（也是索引重播的入口）。
    pub fn replay_many(&mut self, d: &DirView<'_>) {
        if let Some(t) = &mut self.many_files
            && d.direct.files > 0
        {
            let key = d.direct.files;
            t.offer(
                |w| rank(key, d.path)(w.direct_files, &w.path),
                || ManyItem {
                    path: d.path.to_vec(),
                    direct_files: d.direct.files,
                    direct_logical: d.direct.logical,
                },
            );
        }
    }

    pub fn file(&mut self, f: &FileView<'_>) {
        self.add_ext(f);
        self.replay_top_file(f);
    }

    fn add_ext(&mut self, f: &FileView<'_>) {
        if let Some(m) = &mut self.by_ext {
            if !m.contains_key(f.ext) {
                m.insert(f.ext.to_vec(), ExtAgg::default());
            }
            let e = m.get_mut(f.ext).expect("inserted above");
            e.files += 1;
            e.logical += f.logical;
            if !f.secondary {
                e.physical += f.physical;
            }
        }
    }

    /// 只處理「最大檔案」的部分（也是索引重播的入口）。
    pub fn replay_top_file(&mut self, f: &FileView<'_>) {
        if let Some(t) = &mut self.top_files {
            // 硬連結次要名稱不列入最大檔案（同一份資料不重複出現）
            let ok = !f.secondary
                && self.req.min_size.is_none_or(|m| f.physical >= m)
                && self.req.exts.as_ref().is_none_or(|s| s.contains(f.ext))
                && self.older_cutoff.is_none_or(|c| f.mtime < c);
            if ok {
                let key = f.physical;
                t.offer(
                    |w| rank(key, f.path)(w.physical, &w.path),
                    || FileItem {
                        path: f.path.to_vec(),
                        ext: f.ext.to_vec(),
                        logical: f.logical,
                        physical: f.physical,
                        mtime: f.mtime,
                    },
                );
            }
        }
    }

    /// 索引重播：直接放入完整的副檔名統計表（檔案數、邏輯大小、實際大小）。
    pub fn set_ext_table(&mut self, table: Vec<(Vec<u16>, u64, u64, u64)>) {
        if let Some(m) = &mut self.by_ext {
            m.clear();
            for (ext, files, logical, physical) in table {
                m.insert(
                    ext,
                    ExtAgg {
                        files,
                        logical,
                        physical,
                    },
                );
            }
        }
    }

    /// 索引重播：重播只餵入前幾名，這裡補上真正的候選總數（影響各清單的 `truncated`）。
    pub fn set_candidates(&mut self, dirs: u64, many: u64, files: u64) {
        if let Some(t) = &mut self.top_dirs {
            t.set_candidates(dirs);
        }
        if let Some(t) = &mut self.many_files {
            t.set_candidates(many);
        }
        if let Some(t) = &mut self.top_files {
            t.set_candidates(files);
        }
    }

    /// 輸出 `reports` 物件（只含被要求的分析）。
    pub fn write_reports(self, out: &mut Vec<u8>) {
        let mut first = true;
        let mut key = |out: &mut Vec<u8>, name: &str| {
            if !first {
                out.push(b',');
            }
            first = false;
            json::push_str(out, name);
            out.push(b':');
        };
        out.push(b'{');
        if let Some(t) = self.top_files {
            key(out, "top_files");
            let truncated = t.truncated();
            write_list(out, truncated, t.into_sorted(), |out, i| {
                out.extend_from_slice(b"{\"path\":");
                json::push_str_u16(out, &i.path);
                let _ = write!(
                    out,
                    ",\"size_logical\":{},\"size_physical\":{},\"mtime\":\"",
                    i.logical, i.physical
                );
                timefmt::push_filetime(out, i.mtime);
                out.extend_from_slice(b"\",\"ext\":");
                json::push_str_u16(out, &i.ext);
                out.push(b'}');
            });
        }
        if let Some(t) = self.top_dirs {
            key(out, "top_dirs");
            let truncated = t.truncated();
            write_list(out, truncated, t.into_sorted(), |out, i| {
                out.extend_from_slice(b"{\"path\":");
                json::push_str_u16(out, &i.path);
                let _ = write!(
                    out,
                    ",\"depth\":{},\"rollup\":{{\"files\":{},\"dirs\":{},\"logical\":{},\"physical\":{}}},\"latest_mtime\":\"",
                    i.depth, i.rollup.files, i.rollup.dirs, i.rollup.logical, i.rollup.physical
                );
                timefmt::push_filetime(out, i.latest_mtime);
                out.extend_from_slice(b"\"}");
            });
        }
        if let (Some(n), Some(m)) = (self.req.by_ext, self.by_ext) {
            key(out, "by_ext");
            let total = m.len();
            let mut v: Vec<(Vec<u16>, ExtAgg)> = m.into_iter().collect();
            v.sort_by(|a, b| {
                b.1.physical
                    .cmp(&a.1.physical)
                    .then_with(|| cmp_names(&a.0, &b.0))
            });
            v.truncate(n);
            write_list(out, total > n, v, |out, (ext, a)| {
                out.extend_from_slice(b"{\"ext\":");
                json::push_str_u16(out, ext);
                let _ = write!(
                    out,
                    ",\"files\":{},\"logical\":{},\"physical\":{}}}",
                    a.files, a.logical, a.physical
                );
            });
        }
        if let Some(t) = self.many_files {
            key(out, "many_files");
            let truncated = t.truncated();
            write_list(out, truncated, t.into_sorted(), |out, i| {
                out.extend_from_slice(b"{\"path\":");
                json::push_str_u16(out, &i.path);
                let _ = write!(
                    out,
                    ",\"direct_files\":{},\"direct_logical\":{},\"avg_file_logical\":{}}}",
                    i.direct_files,
                    i.direct_logical,
                    i.direct_logical / i.direct_files.max(1)
                );
            });
        }
        out.push(b'}');
    }
}

fn write_list<T>(
    out: &mut Vec<u8>,
    truncated: bool,
    items: Vec<T>,
    mut item: impl FnMut(&mut Vec<u8>, &T),
) {
    out.extend_from_slice(b"{\"items\":[");
    for (i, x) in items.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        item(out, x);
    }
    let _ = write!(out, "],\"truncated\":{truncated}}}");
}

/// 報表的非分析部分（總計、涵蓋範圍、限制說明），即時掃描與快照共用。
pub struct Frame<'a> {
    pub meta_body: &'a [u8],
    pub totals: Totals,
    pub coverage: &'a Coverage,
    pub limited_reasons: &'a [String],
}

pub fn render(frame: &Frame<'_>, reports: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(reports.len() + frame.meta_body.len() + 1024);
    out.extend_from_slice(b"{\"schema\":");
    json::push_str(&mut out, SCHEMA);
    out.extend_from_slice(b",\"meta\":");
    out.extend_from_slice(frame.meta_body);
    let t = &frame.totals;
    let _ = write!(
        out,
        ",\"totals\":{{\"files\":{},\"dirs\":{},\"logical\":{},\"physical\":{}}},\"coverage\":",
        t.files, t.dirs, t.logical, t.physical
    );
    write_coverage(&mut out, frame.coverage);
    let _ = write!(
        out,
        ",\"limited_by_snapshot\":{},\"limited_reasons\":[",
        !frame.limited_reasons.is_empty()
    );
    for (i, r) in frame.limited_reasons.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        json::push_str(&mut out, r);
    }
    out.extend_from_slice(b"],\"reports\":");
    out.extend_from_slice(reports);
    out.extend_from_slice(b"}\n");
    out
}

/// 依快照的 `meta.options` 與被要求的分析，列出結果因快照剪裁而不完整的原因。
pub fn limited_reasons(
    min_size: Option<u64>,
    max_depth: Option<u64>,
    req: &Request,
) -> Vec<String> {
    let mut v = Vec::new();
    let file_level = req.top_files.is_some() || req.by_ext.is_some();
    if let Some(m) = min_size
        && file_level
    {
        v.push(format!(
            "snapshot was made with --min-size {m}: files smaller than that are not in the snapshot, so top_files and by_ext cover only the listed files"
        ));
    }
    if let Some(d) = max_depth {
        let mut affected = Vec::new();
        if req.top_files.is_some() {
            affected.push("top_files");
        }
        if req.by_ext.is_some() {
            affected.push("by_ext");
        }
        if req.top_dirs.is_some() {
            affected.push("top_dirs");
        }
        if req.many_files.is_some() {
            affected.push("many_files");
        }
        if !affected.is_empty() {
            v.push(format!(
                "snapshot was made with --max-depth {d}: entries deeper than that are not in the snapshot, so {} cover only the listed entries",
                affected.join(", ")
            ));
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    fn file<'a>(path: &'a [u16], ext: &'a [u16], physical: u64) -> FileView<'a> {
        FileView {
            path,
            ext,
            logical: physical,
            physical,
            mtime: 100,
            secondary: false,
        }
    }

    fn render_reports(a: Analyzer) -> String {
        let mut out = Vec::new();
        a.write_reports(&mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn top_files_orders_by_size_then_path_and_flags_truncation() {
        let mut a = Analyzer::new(Request {
            top_files: Some(2),
            ..Default::default()
        });
        for (p, s) in [(r"C:\b", 10), (r"C:\a", 10), (r"C:\c", 30), (r"C:\d", 5)] {
            let (pw, ew) = (w(p), w(""));
            a.file(&file(&pw, &ew, s));
        }
        let r = render_reports(a);
        let c = r.find("C:\\\\c").unwrap();
        let aa = r.find("C:\\\\a").unwrap();
        assert!(c < aa, "largest first: {r}");
        assert!(!r.contains("C:\\\\b"), "tie broken by path, 'a' wins: {r}");
        assert!(r.contains("\"truncated\":true"));
    }

    #[test]
    fn filters_apply_to_top_files_only() {
        let mut exts = BTreeSet::new();
        exts.insert(w(".iso"));
        let mut a = Analyzer::new(Request {
            top_files: Some(10),
            by_ext: Some(10),
            min_size: Some(50),
            exts: Some(exts),
            older_than_days: Some(1),
            now: 10 * TICKS_PER_DAY,
            ..Default::default()
        });
        let old = 5 * TICKS_PER_DAY;
        let new = 10 * TICKS_PER_DAY - 1;
        let cases = [
            (r"C:\old_big.iso", ".iso", 100, old, true),
            (r"C:\new_big.iso", ".iso", 100, new, false),
            (r"C:\old_small.iso", ".iso", 10, old, false),
            (r"C:\old_big.zip", ".zip", 100, old, false),
        ];
        for (p, e, s, m, _) in cases {
            let (pw, ew) = (w(p), w(e));
            a.file(&FileView {
                mtime: m,
                ..file(&pw, &ew, s)
            });
        }
        let r = render_reports(a);
        let top = &r[..r.find("\"by_ext\"").unwrap()];
        assert!(top.contains("old_big.iso"));
        assert!(
            !top.contains("new_big") && !top.contains("old_small") && !top.contains("old_big.zip")
        );
        // by_ext 不受篩選影響
        assert!(r.contains("\"ext\":\".zip\"") && r.contains("\"files\":3"));
    }

    #[test]
    fn hardlink_secondaries_are_not_top_files_and_add_no_physical_to_by_ext() {
        let mut a = Analyzer::new(Request {
            top_files: Some(5),
            by_ext: Some(5),
            ..Default::default()
        });
        let (p1, p2, e) = (w(r"C:\a.bin"), w(r"C:\b.bin"), w(".bin"));
        a.file(&file(&p1, &e, 100));
        a.file(&FileView {
            secondary: true,
            ..file(&p2, &e, 100)
        });
        let r = render_reports(a);
        assert!(r.contains("a.bin") && !r.contains("b.bin"), "{r}");
        assert!(
            r.contains("\"files\":2,\"logical\":200,\"physical\":100"),
            "{r}"
        );
    }

    #[test]
    fn many_files_skips_dirs_without_files_and_averages() {
        let mut a = Analyzer::new(Request {
            many_files: Some(5),
            ..Default::default()
        });
        for (p, n, l) in [(r"C:\x", 4u64, 10u64), (r"C:\empty", 0, 0), (r"C:\y", 4, 9)] {
            let pw = w(p);
            a.dir(&DirView {
                path: &pw,
                depth: 1,
                rollup: Totals::default(),
                direct: Totals {
                    files: n,
                    dirs: 0,
                    logical: l,
                    physical: 0,
                },
                latest_mtime: 0,
            });
        }
        let r = render_reports(a);
        assert!(!r.contains("empty"));
        assert!(
            r.contains("\"direct_files\":4,\"direct_logical\":10,\"avg_file_logical\":2"),
            "{r}"
        );
        assert!(r.find("C:\\\\x").unwrap() < r.find("C:\\\\y").unwrap());
        assert!(r.contains("\"truncated\":false"));
    }

    #[test]
    fn top_dirs_respects_dir_depth() {
        let mut a = Analyzer::new(Request {
            top_dirs: Some(5),
            dir_depth: Some(1),
            ..Default::default()
        });
        for (p, d, phys) in [
            (r"C:\r", 0u64, 10u64),
            (r"C:\r\deep", 2, 99),
            (r"C:\r\a", 1, 5),
        ] {
            let pw = w(p);
            a.dir(&DirView {
                path: &pw,
                depth: d,
                rollup: Totals {
                    files: 1,
                    dirs: 0,
                    logical: phys,
                    physical: phys,
                },
                direct: Totals::default(),
                latest_mtime: 0,
            });
        }
        let r = render_reports(a);
        assert!(!r.contains("deep"), "{r}");
        assert!(r.find("C:\\\\r\"").unwrap() < r.find("C:\\\\r\\\\a").unwrap());
    }

    #[test]
    fn limited_reasons_are_reported_only_for_affected_analyses() {
        let only_dirs = Request {
            top_dirs: Some(5),
            ..Default::default()
        };
        assert!(limited_reasons(Some(100), None, &only_dirs).is_empty());
        let files = Request {
            top_files: Some(5),
            ..Default::default()
        };
        assert_eq!(limited_reasons(Some(100), None, &files).len(), 1);
        assert_eq!(limited_reasons(None, Some(2), &only_dirs).len(), 1);
        assert!(limited_reasons(None, None, &files).is_empty());
    }

    /// `Tallier` 路徑（計入、門檻、放入、完成）必須和 `file()` 逐位元組相同，
    /// 包含同分（靠路徑排名）、硬連結次要名稱與篩選條件。
    #[test]
    fn tallier_path_equals_the_plain_file_path() {
        let mut seed = 99u64;
        let mut next = move |n: u64| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) % n
        };
        let exts = [".txt", ".log", "", ".bin"];
        let mut files = Vec::new();
        for i in 0..600u64 {
            let ext = exts[next(4) as usize];
            let p = format!(r"C:\d{}\f{}{}", next(7), next(40), ext);
            files.push((
                w(&p),
                w(ext),
                next(5) * 4096,
                next(5) * 4096,
                next(30),
                next(8) == 0,
                i,
            ));
        }
        let reqs = [
            Request {
                top_files: Some(5),
                by_ext: Some(10),
                ..Default::default()
            },
            Request {
                top_files: Some(3),
                min_size: Some(8192),
                by_ext: Some(2),
                ..Default::default()
            },
            Request {
                top_files: Some(4),
                exts: Some([w(".log")].into_iter().collect()),
                now: 100 * TICKS_PER_DAY,
                older_than_days: Some(70),
                ..Default::default()
            },
            Request {
                by_ext: Some(3),
                ..Default::default()
            },
        ];
        for req in reqs {
            let mut plain = Analyzer::new(req.clone());
            let mut split = Analyzer::new(req.clone());
            for (path, ext, logical, physical, mtime_days, secondary, _) in &files {
                let mtime = mtime_days * TICKS_PER_DAY;
                let fv = FileView {
                    path,
                    ext,
                    logical: *logical,
                    physical: *physical,
                    mtime,
                    secondary: *secondary,
                };
                plain.file(&fv);
                let t = Tally {
                    ext,
                    logical: *logical,
                    physical: *physical,
                    mtime,
                    secondary: *secondary,
                };
                split.tally(&t, true);
                if split.admit(&t) {
                    split.offer(&fv);
                }
            }
            split.finish_files();
            let (mut a, mut b) = (Vec::new(), Vec::new());
            plain.write_reports(&mut a);
            split.write_reports(&mut b);
            assert_eq!(String::from_utf8(a).unwrap(), String::from_utf8(b).unwrap());
        }
    }

    /// 計入後再退出，回到沒有計入的狀態（孤兒檔案的處理）。
    #[test]
    fn untallying_restores_the_aggregates() {
        let req = Request {
            top_files: Some(2),
            by_ext: Some(5),
            ..Default::default()
        };
        let mut a = Analyzer::new(req.clone());
        let mut b = Analyzer::new(req);
        let keep = Tally {
            ext: &w(".txt"),
            logical: 10,
            physical: 4096,
            mtime: 0,
            secondary: false,
        };
        let orphan = Tally {
            ext: &w(".dat"),
            logical: 5,
            physical: 4096,
            mtime: 0,
            secondary: false,
        };
        for an in [&mut a, &mut b] {
            an.tally(&keep, true);
        }
        b.tally(&orphan, true);
        b.tally(&orphan, false);
        a.finish_files();
        b.finish_files();
        let (mut x, mut y) = (Vec::new(), Vec::new());
        a.write_reports(&mut x);
        b.write_reports(&mut y);
        assert_eq!(x, y);
    }
}
