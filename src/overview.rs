//! `overview`：一次呼叫回答常見問題。
//!
//! 與 `report` 一樣只走訪一次輸入（即時掃描的結果或快照），並共用 `Sink` 介面，
//! 所以兩者的數字在建構上就一致：排序、次要排序、硬連結次要名稱的處理都沿用 `report`。

use crate::fxhash::FxHashMap;
use crate::json;
use crate::model::Totals;
use crate::names::cmp_names;
use crate::report::{DirView, FileView, Floor, Sink, Tallier, Tally, TopK, rank};
use crate::snapshot::{Coverage, write_coverage};
use crate::timefmt;
use std::cmp::Ordering;
use std::io::Write;

pub const SCHEMA: &str = "voltdir.overview/1";
const TICKS_PER_DAY: u64 = 864_000_000_000;
const SEP: u16 = b'\\' as u16;

#[derive(Clone)]
pub struct Options {
    pub top: usize,
    pub ext_top: usize,
    pub stale_days: u64,
    pub stale_min_size: u64,
    pub rank_logical: bool,
    /// `--dir-rank direct`：`top_dirs` 依直接子檔案的大小排序（預設依滾算大小）
    pub dir_direct: bool,
    /// 使用者沒有明確給 `--top`／`--ext-top`，且是 concise：輸出超過 `SIZE_BUDGET` 時自動減少筆數
    pub auto_trim: bool,
    /// 「現在」的 FILETIME
    pub now: u64,
}

/// concise 輸出的大小預算。
pub const SIZE_BUDGET: usize = 4096;
/// 超過預算時依序嘗試的每個清單筆數。
const TRIM_STEPS: [usize; 2] = [5, 3];

/// 目錄與檔案共用的清單項目；`key` 是排序依據（依 `--rank` 取 logical 或 physical）。
struct Item {
    key: u64,
    path: Vec<u16>,
    files: u64,
    logical: u64,
    physical: u64,
    /// 目錄：直接子檔案的大小（`--dir-rank direct` 時輸出）
    direct_logical: u64,
    direct_physical: u64,
    mtime: u64,
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
    fn cmp(&self, o: &Self) -> Ordering {
        self.key
            .cmp(&o.key)
            .then_with(|| cmp_names(&o.path, &self.path))
    }
}

#[derive(Default)]
struct ExtAgg {
    files: u64,
    logical: u64,
    physical: u64,
}

pub struct Collector {
    opt: Options,
    stale_cutoff: u64,
    dirs: TopK<Item>,
    files: TopK<Item>,
    stale: TopK<Item>,
    exts: FxHashMap<Vec<u16>, ExtAgg>,
    empty_files: u64,
    empty_dirs: u64,
    /// `Tallier` 路徑：各清單的候選總數與前 N 名門檻
    cand_files: u64,
    cand_stale: u64,
    floor_files: Floor,
    floor_stale: Floor,
}

impl Collector {
    pub fn new(opt: Options) -> Collector {
        Collector {
            stale_cutoff: opt
                .now
                .saturating_sub(opt.stale_days.saturating_mul(TICKS_PER_DAY)),
            dirs: TopK::new(opt.top),
            files: TopK::new(opt.top),
            stale: TopK::new(opt.top),
            exts: FxHashMap::default(),
            empty_files: 0,
            empty_dirs: 0,
            cand_files: 0,
            cand_stale: 0,
            floor_files: Floor::new(opt.top),
            floor_stale: Floor::new(opt.top),
            opt,
        }
    }

    fn stale_ok(&self, t: &Tally<'_>) -> bool {
        t.physical >= self.opt.stale_min_size && t.mtime < self.stale_cutoff
    }

    fn pick(&self, logical: u64, physical: u64) -> u64 {
        if self.opt.rank_logical {
            logical
        } else {
            physical
        }
    }
}

impl Collector {
    /// 索引重播：空檔案與空資料夾的數量直接取自索引。
    pub fn set_empties(&mut self, files: u64, dirs: u64) {
        self.empty_files = files;
        self.empty_dirs = dirs;
    }

    /// 索引重播：完整的副檔名統計表。
    pub fn set_exts(&mut self, table: Vec<(Vec<u16>, u64, u64, u64)>) {
        self.exts.clear();
        for (ext, files, logical, physical) in table {
            self.exts.insert(
                ext,
                ExtAgg {
                    files,
                    logical,
                    physical,
                },
            );
        }
    }

    /// 索引重播：重播只餵入前幾名，這裡補上真正的候選總數。
    pub fn set_candidates(&mut self, dirs: u64, files: u64) {
        self.dirs.set_candidates(dirs);
        self.files.set_candidates(files);
    }

    /// 索引重播：只放入「最大目錄」清單（不動空資料夾計數）。
    pub fn replay_dir(&mut self, d: &DirView<'_>) {
        self.offer_dir(d);
    }

    /// 索引重播：只放入「最大檔案」清單。
    pub fn replay_file(&mut self, f: &FileView<'_>) {
        self.offer_file(f);
    }

    /// 索引重播：只放入「久未修改的大檔」清單（呼叫端已篩過大小與時間）。
    pub fn replay_stale(&mut self, f: &FileView<'_>) {
        let key = self.pick(f.logical, f.physical);
        self.stale.offer(
            |w| rank(key, f.path)(w.key, &w.path),
            || Item {
                key,
                path: f.path.to_vec(),
                files: 0,
                logical: f.logical,
                physical: f.physical,
                direct_logical: 0,
                direct_physical: 0,
                mtime: f.mtime,
            },
        );
    }

    /// 「久未修改」的時間門檻（FILETIME）：早於它才算。
    pub fn stale_cutoff(&self) -> u64 {
        self.stale_cutoff
    }
}

impl Tallier for Collector {
    fn tally(&mut self, t: &Tally<'_>, add: bool) {
        if add {
            if !self.exts.contains_key(t.ext) {
                self.exts.insert(t.ext.to_vec(), ExtAgg::default());
            }
            let e = self.exts.get_mut(t.ext).expect("inserted above");
            e.files += 1;
            e.logical += t.logical;
            if !t.secondary {
                e.physical += t.physical;
            }
        } else if let Some(e) = self.exts.get_mut(t.ext) {
            e.files -= 1;
            e.logical -= t.logical;
            if !t.secondary {
                e.physical -= t.physical;
            }
            if e.files == 0 {
                self.exts.remove(t.ext);
            }
        }
        let d: i64 = if add { 1 } else { -1 };
        if t.logical == 0 {
            self.empty_files = self.empty_files.wrapping_add_signed(d);
        }
        if !t.secondary {
            self.cand_files = self.cand_files.wrapping_add_signed(d);
            if self.stale_ok(t) {
                self.cand_stale = self.cand_stale.wrapping_add_signed(d);
            }
        }
    }

    fn admit(&mut self, t: &Tally<'_>) -> bool {
        if t.secondary {
            return false;
        }
        let key = self.pick(t.logical, t.physical);
        let a = self.floor_files.admit(key);
        let b = self.stale_ok(t) && self.floor_stale.admit(key);
        a || b
    }

    fn offer(&mut self, f: &FileView<'_>) {
        self.offer_file(f);
    }

    fn forget(&mut self, t: &Tally<'_>) -> bool {
        if t.secondary {
            return true;
        }
        let key = self.pick(t.logical, t.physical);
        let a = self.floor_files.forget(key);
        let b = !self.stale_ok(t) || self.floor_stale.forget(key);
        a && b
    }

    fn finish_files(&mut self) {
        self.files.set_candidates(self.cand_files);
        self.stale.set_candidates(self.cand_stale);
    }

    fn reset(&mut self) {
        *self = Collector::new(self.opt.clone());
    }
}

impl Sink for Collector {
    fn dir(&mut self, d: &DirView<'_>) {
        if d.rollup.files == 0 && d.rollup.dirs == 0 {
            self.empty_dirs += 1;
        }
        self.offer_dir(d);
    }

    fn file(&mut self, f: &FileView<'_>) {
        if f.logical == 0 {
            self.empty_files += 1;
        }
        if !self.exts.contains_key(f.ext) {
            self.exts.insert(f.ext.to_vec(), ExtAgg::default());
        }
        let e = self.exts.get_mut(f.ext).expect("inserted above");
        e.files += 1;
        e.logical += f.logical;
        if !f.secondary {
            e.physical += f.physical;
        }
        self.offer_file(f);
    }
}

impl Collector {
    fn offer_dir(&mut self, d: &DirView<'_>) {
        // 掃描根自身（深度 0）的值放在 totals，不列入清單
        if d.depth == 0 {
            return;
        }
        let basis = if self.opt.dir_direct {
            d.direct
        } else {
            d.rollup
        };
        // direct 排序時，沒有直接子檔案的資料夾沒有資訊量，不列入
        if self.opt.dir_direct && basis.files == 0 {
            return;
        }
        let key = self.pick(basis.logical, basis.physical);
        let (path, rollup, direct) = (d.path, d.rollup, d.direct);
        let latest = d.latest_mtime;
        self.dirs.offer(
            |w| rank(key, path)(w.key, &w.path),
            || Item {
                key,
                path: path.to_vec(),
                files: rollup.files,
                logical: rollup.logical,
                physical: rollup.physical,
                direct_logical: direct.logical,
                direct_physical: direct.physical,
                mtime: latest,
            },
        );
    }

    /// 最大檔案與久未修改的大檔兩個清單。
    fn offer_file(&mut self, f: &FileView<'_>) {
        // 硬連結次要名稱不列入最大檔案（同一份資料不重複出現），規則同 report
        if f.secondary {
            return;
        }
        let key = self.pick(f.logical, f.physical);
        let mk = || Item {
            key,
            path: f.path.to_vec(),
            files: 0,
            logical: f.logical,
            physical: f.physical,
            direct_logical: 0,
            direct_physical: 0,
            mtime: f.mtime,
        };
        self.files.offer(|w| rank(key, f.path)(w.key, &w.path), mk);
        if f.physical >= self.opt.stale_min_size && f.mtime < self.stale_cutoff {
            self.stale.offer(|w| rank(key, f.path)(w.key, &w.path), mk);
        }
    }
}

/// 輸出所需、與清單無關的部分。
pub struct Frame<'a> {
    pub roots: &'a [Vec<u16>],
    pub engine: &'a str,
    pub totals: Totals,
    pub coverage: &'a Coverage,
    pub limited_reasons: &'a [String],
    /// `--detail full` 時的 meta 本體；concise 為 None
    pub meta_body: Option<&'a [u8]>,
    /// 以最大資料夾的絕對路徑（若有）產生可直接執行的下一步命令
    pub next: &'a dyn Fn(Option<&str>) -> Vec<String>,
}

/// 去掉 `root` 這個前綴與緊接的分隔符。
fn relative_to(root: &[u16], path: &[u16]) -> Vec<u16> {
    let mut n = root.len();
    while n > 0 && root[n - 1] == SEP {
        n -= 1;
    }
    let mut cut = n.min(path.len());
    while cut < path.len() && path[cut] == SEP {
        cut += 1;
    }
    path[cut..].to_vec()
}

fn write_list(
    out: &mut Vec<u8>,
    truncated: bool,
    items: &[Item],
    mut one: impl FnMut(&mut Vec<u8>, &Item),
) {
    out.extend_from_slice(b"{\"items\":[");
    for (i, x) in items.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        one(out, x);
    }
    let _ = write!(out, "],\"truncated\":{truncated}}}");
}

/// 已排序、可重複輸出的結果；自動收斂時用不同的筆數上限輸出多次。
struct Parts {
    dirs: Vec<Item>,
    dirs_truncated: bool,
    files: Vec<Item>,
    files_truncated: bool,
    stale: Vec<Item>,
    stale_truncated: bool,
    exts: Vec<(Vec<u16>, ExtAgg)>,
    exts_truncated: bool,
    rank_logical: bool,
    dir_direct: bool,
    empty_files: u64,
    empty_dirs: u64,
}

impl Collector {
    /// 輸出整個 `voltdir.overview/1`。`relative_base` 有值時所有清單路徑相對於該根。
    pub fn render(self, frame: &Frame<'_>, relative_base: Option<&[u16]>) -> Vec<u8> {
        let logical = self.opt.rank_logical;
        let total = self.exts.len();
        let mut exts: Vec<(Vec<u16>, ExtAgg)> = self.exts.into_iter().collect();
        exts.sort_by(|a, b| {
            let (ka, kb) = if logical {
                (a.1.logical, b.1.logical)
            } else {
                (a.1.physical, b.1.physical)
            };
            kb.cmp(&ka).then_with(|| cmp_names(&a.0, &b.0))
        });
        exts.truncate(self.opt.ext_top);
        let parts = Parts {
            dirs_truncated: self.dirs.truncated(),
            dirs: self.dirs.into_sorted(),
            files_truncated: self.files.truncated(),
            files: self.files.into_sorted(),
            stale_truncated: self.stale.truncated(),
            stale: self.stale.into_sorted(),
            exts_truncated: total > self.opt.ext_top,
            exts,
            rank_logical: logical,
            dir_direct: self.opt.dir_direct,
            empty_files: self.empty_files,
            empty_dirs: self.empty_dirs,
        };
        let mut out = parts.emit(frame, relative_base, None);
        // 沒有明確要求筆數的 concise 輸出有大小預算：超過就依序減少每個清單的筆數
        if self.opt.auto_trim && frame.meta_body.is_none() && out.len() > SIZE_BUDGET {
            for cap in TRIM_STEPS {
                out = parts.emit(frame, relative_base, Some(cap));
                if out.len() <= SIZE_BUDGET {
                    break;
                }
            }
        }
        out
    }
}

impl Parts {
    fn emit(
        &self,
        frame: &Frame<'_>,
        relative_base: Option<&[u16]>,
        cap: Option<usize>,
    ) -> Vec<u8> {
        let n = |len: usize| cap.map_or(len, |c| len.min(c));
        let mut out = Vec::with_capacity(4096);
        out.extend_from_slice(b"{\"schema\":");
        json::push_str(&mut out, SCHEMA);
        out.extend_from_slice(b",\"path_base\":");
        json::push_str(
            &mut out,
            if relative_base.is_some() {
                "relative"
            } else {
                "absolute"
            },
        );
        out.extend_from_slice(b",\"roots\":[");
        for (i, r) in frame.roots.iter().enumerate() {
            if i > 0 {
                out.push(b',');
            }
            json::push_str_u16(&mut out, r);
        }
        out.extend_from_slice(b"],\"engine\":");
        json::push_str(&mut out, frame.engine);
        out.extend_from_slice(b",\"rank\":");
        json::push_str(
            &mut out,
            if self.rank_logical {
                "logical"
            } else {
                "physical"
            },
        );
        if self.dir_direct {
            out.extend_from_slice(b",\"dir_rank\":\"direct\"");
        }
        let t = &frame.totals;
        let _ = write!(
            out,
            ",\"totals\":{{\"files\":{},\"dirs\":{},\"logical\":{},\"physical\":{}}}",
            t.files, t.dirs, t.logical, t.physical
        );

        let shown = |out: &mut Vec<u8>, path: &[u16]| match relative_base {
            Some(base) => json::push_str_u16(out, &relative_to(base, path)),
            None => json::push_str_u16(out, path),
        };
        let file_item = |out: &mut Vec<u8>, i: &Item| {
            out.extend_from_slice(b"{\"path\":");
            shown(out, &i.path);
            let _ = write!(
                out,
                ",\"logical\":{},\"physical\":{},\"modified\":\"",
                i.logical, i.physical
            );
            // 秒以下的部分對這個用途沒有價值，省下每項約 10 位元組
            let mut t = Vec::new();
            timefmt::push_filetime(&mut t, i.mtime);
            if let Some(dot) = t.iter().position(|&b| b == b'.') {
                t.truncate(dot);
                t.push(b'Z');
            }
            out.extend_from_slice(&t);
            out.extend_from_slice(b"\"}");
        };
        let direct = self.dir_direct;

        out.extend_from_slice(b",\"top_dirs\":");
        let first_dir = self.dirs.first().map(|d| String::from_utf16_lossy(&d.path));
        let k = n(self.dirs.len());
        write_list(
            &mut out,
            self.dirs_truncated || self.dirs.len() > k,
            &self.dirs[..k],
            |out, i| {
                out.extend_from_slice(b"{\"path\":");
                shown(out, &i.path);
                let _ = write!(
                    out,
                    ",\"logical\":{},\"physical\":{},\"files\":{}",
                    i.logical, i.physical, i.files
                );
                if direct {
                    let _ = write!(
                        out,
                        ",\"direct_logical\":{},\"direct_physical\":{}",
                        i.direct_logical, i.direct_physical
                    );
                }
                out.push(b'}');
            },
        );
        out.extend_from_slice(b",\"top_files\":");
        let k = n(self.files.len());
        write_list(
            &mut out,
            self.files_truncated || self.files.len() > k,
            &self.files[..k],
            file_item,
        );

        out.extend_from_slice(b",\"by_ext\":");
        let k = n(self.exts.len());
        let v = &self.exts[..k];
        out.extend_from_slice(b"{\"items\":[");
        for (i, (ext, a)) in v.iter().enumerate() {
            if i > 0 {
                out.push(b',');
            }
            out.extend_from_slice(b"{\"ext\":");
            json::push_str_u16(&mut out, ext);
            let _ = write!(
                out,
                ",\"files\":{},\"logical\":{},\"physical\":{}}}",
                a.files, a.logical, a.physical
            );
        }
        let _ = write!(
            out,
            "],\"truncated\":{}}}",
            self.exts_truncated || self.exts.len() > k
        );

        out.extend_from_slice(b",\"stale_large\":");
        let k = n(self.stale.len());
        write_list(
            &mut out,
            self.stale_truncated || self.stale.len() > k,
            &self.stale[..k],
            file_item,
        );

        let _ = write!(
            out,
            ",\"empties\":{{\"files\":{},\"dirs\":{}}},\"coverage\":",
            self.empty_files, self.empty_dirs
        );
        write_coverage(&mut out, frame.coverage);
        if !frame.limited_reasons.is_empty() {
            out.extend_from_slice(b",\"limited_by_snapshot\":true,\"limited_reasons\":[");
            for (i, r) in frame.limited_reasons.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                json::push_str(&mut out, r);
            }
            out.push(b']');
        }
        if let Some(c) = cap {
            // 輸出超過大小預算，各清單已減為最多 c 筆（想要更多請明確給 --top／--ext-top）
            let _ = write!(out, ",\"trimmed\":{{\"top\":{c}}}");
        }
        out.extend_from_slice(b",\"next\":[");
        for (i, n) in (frame.next)(first_dir.as_deref()).iter().enumerate() {
            if i > 0 {
                out.push(b',');
            }
            json::push_str(&mut out, n);
        }
        out.push(b']');
        if let Some(m) = frame.meta_body {
            out.extend_from_slice(b",\"meta\":");
            out.extend_from_slice(m);
        }
        out.extend_from_slice(b"}\n");
        out
    }
}

/// 可直接執行的下一步命令。`input` 是已加好引號的輸入參數：
/// `--snapshot "<FILE>"`，或一到多個掃描根。
pub fn next_commands(input: &str, from_snapshot: bool, first_dir: Option<&str>) -> Vec<String> {
    let mut v = vec![
        format!("voltdir find {input} --type dir --name node_modules --sort size --limit 20"),
        format!("voltdir report {input} --many-files 20 --top-dirs 30 --dir-depth 3"),
    ];
    if let Some(d) = first_dir {
        v.push(format!(
            "voltdir find {input} --path \"{d}\\*\" --sort size --limit 20"
        ));
    }
    if !from_snapshot {
        v.push(format!(
            "voltdir overview {input} --save-snapshot voltdir-snapshot.ndjson"
        ));
    }
    v
}

/// 只有索引（沒有快照）時的下一步命令：索引答得出來的追問，加一條重新掃描並存快照的命令，
/// 那樣才有 `find` 與 `diff`。
pub fn next_commands_index(index: &str, roots: &str) -> Vec<String> {
    vec![
        format!("voltdir report --index {index} --many-files 20 --top-dirs 30 --dir-depth 3"),
        format!("voltdir overview --index {index} --rank logical"),
        format!("voltdir overview --index {index} --dir-rank direct"),
        format!("voltdir overview {roots} --save-snapshot voltdir-snapshot.ndjson"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn relative_paths_drop_the_root_and_the_separator() {
        assert_eq!(
            relative_to(&w(r"C:\r"), &w(r"C:\r\media\big")),
            w(r"media\big")
        );
        assert_eq!(relative_to(&w(r"C:\"), &w(r"C:\x\y")), w(r"x\y"));
        assert_eq!(relative_to(&w(r"c:\R\"), &w(r"C:\r\a.txt")), w("a.txt"));
    }

    fn collector(logical: bool) -> Collector {
        Collector::new(Options {
            top: 2,
            ext_top: 8,
            stale_days: 365,
            stale_min_size: 100,
            rank_logical: logical,
            dir_direct: false,
            auto_trim: false,
            now: 1_000_000 * TICKS_PER_DAY,
        })
    }

    fn file<'a>(path: &'a [u16], ext: &'a [u16], logical: u64, physical: u64) -> FileView<'a> {
        FileView {
            path,
            ext,
            logical,
            physical,
            mtime: 0,
            secondary: false,
        }
    }

    /// 物理大小與邏輯大小的順序相反的合成案例：--rank 決定誰排第一。
    #[test]
    fn rank_decides_the_order_of_files() {
        let (a, b, ext) = (w(r"C:\r\a.bin"), w(r"C:\r\b.bin"), w(".bin"));
        for (logical, first) in [(false, &b), (true, &a)] {
            let mut c = collector(logical);
            c.file(&file(&a, &ext, 900, 1000));
            c.file(&file(&b, &ext, 100, 4096));
            let top: Vec<Item> = c.files.into_sorted();
            assert_eq!(&top[0].path, first, "logical={logical}");
        }
    }

    #[test]
    fn secondary_hardlink_names_are_not_listed_and_stale_needs_size_and_age() {
        let mut c = collector(false);
        let (p, q, ext) = (w(r"C:\r\p"), w(r"C:\r\q"), w(""));
        let mut f = file(&p, &ext, 500, 500);
        f.secondary = true;
        c.file(&f);
        c.file(&file(&q, &ext, 500, 500));
        assert_eq!(c.files.into_sorted().len(), 1);
        // mtime 0 遠早於 365 天前，且 500 >= 100：只有非次要名稱進入 stale
        assert_eq!(c.stale.into_sorted().len(), 1);
    }

    fn render_plain(col: Collector) -> String {
        let cov = Coverage::default();
        let next = |_: Option<&str>| Vec::new();
        let roots = vec![w(r"C:\")];
        let frame = Frame {
            roots: &roots,
            engine: "walk",
            totals: Totals::default(),
            coverage: &cov,
            limited_reasons: &[],
            meta_body: None,
            next: &next,
        };
        String::from_utf8(col.render(&frame, None)).unwrap()
    }

    /// `Tallier` 路徑（計入、門檻、放入、完成）必須和 `file()` 逐位元組相同。
    #[test]
    fn tallier_path_equals_the_plain_file_path() {
        let mut seed = 7u64;
        let mut next = move |n: u64| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) % n
        };
        let exts = [".txt", ".log", "", ".bin"];
        let mut files = Vec::new();
        for _ in 0..600u64 {
            let ext = exts[next(4) as usize];
            let p = format!(r"C:\d{}\f{}{}", next(7), next(40), ext);
            files.push((
                w(&p),
                w(ext),
                next(5) * 4096,
                next(5) * 4096,
                next(500),
                next(8) == 0,
            ));
        }
        for (logical_rank, top) in [(false, 5usize), (true, 3), (false, 1)] {
            let mk = || {
                Collector::new(Options {
                    top,
                    ext_top: 3,
                    stale_days: 100,
                    stale_min_size: 8192,
                    rank_logical: logical_rank,
                    dir_direct: false,
                    auto_trim: false,
                    now: 1_000 * TICKS_PER_DAY,
                })
            };
            let (mut plain, mut split) = (mk(), mk());
            for (path, ext, logical, physical, days, secondary) in &files {
                let mtime = days * TICKS_PER_DAY;
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
            assert_eq!(
                render_plain(plain),
                render_plain(split),
                "rank_logical={logical_rank} top={top}"
            );
        }
    }
}
