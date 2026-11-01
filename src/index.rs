//! 快照旁的彙總索引：`<FILE>.idx`，格式 `voltdir.index/1`。
//!
//! 追問的典型流程是「掃一次、問很多次」。讀整份快照回答一個問題要把幾百萬行都解析一遍（整顆 `C:\`
//! 約 22 秒），但 `overview` 與 `report` 的答案只取決於少數資料：每個深度最大的資料夾、最大的檔案、
//! 完整的副檔名表、幾個計數。索引把這些「候選」和計數存下來；回答時把候選**重播**給原本的分析器
//! （`report::Analyzer`、`overview::Collector`），再補上真正的候選總數，所以輸出與讀快照逐位元組相同，
//! 排序、次要排序、`truncated` 的規則不必重寫第二份。
//!
//! 索引只在能保證答案相同時使用：記錄快照的位元組長度、修改時間與結尾 `summary` 行的雜湊，
//! 任何不符、索引損壞、請求超出索引能回答的範圍（額外篩選、低於 100 MiB 的門檻……）都忽略索引，
//! 回退讀快照，並在 stderr 印一行 `index: not used (<原因>)`。

use crate::fxhash::FxHashMap;
use crate::json;
use crate::jsonin::{self, Val};
use crate::model::ScanData;
use crate::model::Totals;
use crate::names::cmp_names;
use crate::overview;
use crate::report::{self, Analyzer, DirView, FileView, Sink, TopK, rank};
use crate::snapread::{self, Parsed};
use crate::snapshot::{Coverage, Emit, Entry, write_coverage};
use std::cmp::Ordering;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub const SCHEMA: &str = "voltdir.index/1";
/// 每個深度保留的最大資料夾數（`report --top-dirs` 上限 1000）。
const DIR_K: usize = 1000;
/// `overview` 的清單筆數上限是 100。
const OV_K: usize = 100;
/// 保留的最大檔案數與「大檔」清單的上限（`report --top-files` 上限 1000）。
const FILE_K: usize = 1000;
/// 「久未修改的大檔」能回答的最低大小門檻（預設 `--stale-min-size`）。
pub const BIG_MIN: u64 = 100 * 1024 * 1024;
/// 讀快照結尾時最多往回看的位元組數（`summary` 行含最多 100 條沒讀到的路徑）。
const TAIL_BYTES: u64 = 4 * 1024 * 1024;

/// 索引檔的路徑：快照路徑後面加 `.idx`。
pub fn path_of(snapshot: &Path) -> PathBuf {
    let mut s = snapshot.as_os_str().to_owned();
    s.push(".idx");
    PathBuf::from(s)
}

// ---- 建立 ----

struct DirRec {
    key: u64,
    path: Vec<u16>,
    depth: u64,
    rollup: Totals,
    direct: Totals,
    latest: u64,
}

impl PartialEq for DirRec {
    fn eq(&self, o: &Self) -> bool {
        self.cmp(o) == Ordering::Equal
    }
}
impl Eq for DirRec {}
impl PartialOrd for DirRec {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for DirRec {
    fn cmp(&self, o: &Self) -> Ordering {
        self.key
            .cmp(&o.key)
            .then_with(|| cmp_names(&o.path, &self.path))
    }
}

#[derive(Clone)]
struct FileRec {
    key: u64,
    path: Vec<u16>,
    ext: Vec<u16>,
    logical: u64,
    physical: u64,
    mtime: u64,
}

impl PartialEq for FileRec {
    fn eq(&self, o: &Self) -> bool {
        self.cmp(o) == Ordering::Equal
    }
}
impl Eq for FileRec {}
impl PartialOrd for FileRec {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for FileRec {
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

/// 依標準順序接收「快照裡實際輸出的項目」，累積索引需要的候選與計數。
pub struct Builder {
    by_depth: Vec<TopK<DirRec>>,
    depth_counts: Vec<u64>,
    top_rl: TopK<DirRec>,
    top_dp: TopK<DirRec>,
    top_dl: TopK<DirRec>,
    nonroot: u64,
    nonroot_direct: u64,
    many: TopK<DirRec>,
    many_candidates: u64,
    files_p: TopK<FileRec>,
    files_l: TopK<FileRec>,
    primary: u64,
    big: Vec<FileRec>,
    big_complete: bool,
    exts: FxHashMap<Vec<u16>, ExtAgg>,
    empty_files: u64,
    empty_dirs: u64,
}

impl Builder {
    pub fn new() -> Builder {
        Builder {
            by_depth: Vec::new(),
            depth_counts: Vec::new(),
            top_rl: TopK::new(OV_K),
            top_dp: TopK::new(OV_K),
            top_dl: TopK::new(OV_K),
            nonroot: 0,
            nonroot_direct: 0,
            many: TopK::new(DIR_K),
            many_candidates: 0,
            files_p: TopK::new(FILE_K),
            files_l: TopK::new(FILE_K),
            primary: 0,
            big: Vec::new(),
            big_complete: true,
            exts: FxHashMap::default(),
            empty_files: 0,
            empty_dirs: 0,
        }
    }
}

fn dir_rec(key: u64, d: &DirView<'_>) -> DirRec {
    DirRec {
        key,
        path: d.path.to_vec(),
        depth: d.depth,
        rollup: d.rollup,
        direct: d.direct,
        latest: d.latest_mtime,
    }
}

fn offer_dir(list: &mut TopK<DirRec>, key: u64, d: &DirView<'_>) {
    list.offer(|w| rank(key, d.path)(w.key, &w.path), || dir_rec(key, d));
}

fn file_rec(key: u64, f: &FileView<'_>) -> FileRec {
    FileRec {
        key,
        path: f.path.to_vec(),
        ext: f.ext.to_vec(),
        logical: f.logical,
        physical: f.physical,
        mtime: f.mtime,
    }
}

impl Sink for Builder {
    fn dir(&mut self, d: &DirView<'_>) {
        let depth = d.depth as usize;
        if self.by_depth.len() <= depth {
            self.by_depth.resize_with(depth + 1, || TopK::new(DIR_K));
            self.depth_counts.resize(depth + 1, 0);
        }
        self.depth_counts[depth] += 1;
        offer_dir(&mut self.by_depth[depth], d.rollup.physical, d);
        if d.rollup.files == 0 && d.rollup.dirs == 0 {
            self.empty_dirs += 1;
        }
        if d.direct.files > 0 {
            self.many_candidates += 1;
            offer_dir(&mut self.many, d.direct.files, d);
        }
        if d.depth > 0 {
            self.nonroot += 1;
            offer_dir(&mut self.top_rl, d.rollup.logical, d);
            if d.direct.files > 0 {
                self.nonroot_direct += 1;
                offer_dir(&mut self.top_dp, d.direct.physical, d);
                offer_dir(&mut self.top_dl, d.direct.logical, d);
            }
        }
    }

    fn file(&mut self, f: &FileView<'_>) {
        if !self.exts.contains_key(f.ext) {
            self.exts.insert(f.ext.to_vec(), ExtAgg::default());
        }
        let e = self.exts.get_mut(f.ext).expect("inserted above");
        e.files += 1;
        e.logical += f.logical;
        if !f.secondary {
            e.physical += f.physical;
        }
        if f.logical == 0 {
            self.empty_files += 1;
        }
        if f.secondary {
            return;
        }
        self.primary += 1;
        let kp = f.physical;
        self.files_p
            .offer(|w| rank(kp, f.path)(w.key, &w.path), || file_rec(kp, f));
        let kl = f.logical;
        self.files_l
            .offer(|w| rank(kl, f.path)(w.key, &w.path), || file_rec(kl, f));
        if f.physical >= BIG_MIN {
            if self.big.len() < FILE_K {
                self.big.push(file_rec(f.physical, f));
            } else {
                self.big_complete = false;
            }
        }
    }
}

fn push_dirs(out: &mut Vec<u8>, list: Vec<DirRec>) {
    out.push(b'[');
    for (i, d) in list.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.push(b'[');
        json::push_str_u16(out, &d.path);
        use std::io::Write;
        let _ = write!(
            out,
            ",{},{},{},{},{},{},{},{},{}]",
            d.depth,
            d.rollup.files,
            d.rollup.dirs,
            d.rollup.logical,
            d.rollup.physical,
            d.direct.files,
            d.direct.logical,
            d.direct.physical,
            d.latest
        );
    }
    out.push(b']');
}

fn push_files(out: &mut Vec<u8>, list: &[FileRec]) {
    out.push(b'[');
    for (i, f) in list.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.push(b'[');
        json::push_str_u16(out, &f.path);
        out.push(b',');
        json::push_str_u16(out, &f.ext);
        use std::io::Write;
        let _ = write!(out, ",{},{},{}]", f.logical, f.physical, f.mtime);
    }
    out.push(b']');
}

/// FNV-1a 64 位元（只用來偵測快照結尾是否改變，不是安全用途）。
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn mtime_ns(meta: &std::fs::Metadata) -> Result<i64, String> {
    let t = meta
        .modified()
        .map_err(|e| format!("cannot read the modified time: {e}"))?;
    let d = t
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| format!("modified time before 1970: {e}"))?;
    i64::try_from(d.as_nanos()).map_err(|_| "modified time out of range".to_string())
}

/// 快照結尾的 `summary` 行（最後一個非空行）。
fn summary_line(path: &Path, len: u64) -> Result<Vec<u8>, String> {
    let mut f = std::fs::File::open(path).map_err(|e| format!("cannot open the snapshot: {e}"))?;
    let start = len.saturating_sub(TAIL_BYTES);
    f.seek(SeekFrom::Start(start))
        .map_err(|e| format!("cannot read the snapshot: {e}"))?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.read_to_end(&mut buf)
        .map_err(|e| format!("cannot read the snapshot: {e}"))?;
    while matches!(buf.last(), Some(b'\n' | b'\r')) {
        buf.pop();
    }
    let from = buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    if from == 0 && start > 0 {
        return Err("the summary line is longer than the tail window".into());
    }
    Ok(buf[from..].to_vec())
}

/// 索引檔的標頭資料。`snapshot` 為 `Some((位元組長度, 修改時間 ns, summary 行雜湊))` 時是快照旁的索引；
/// 為 `None` 時是獨立的索引。
pub struct Header<'a> {
    pub snapshot: Option<(u64, i64, u64)>,
    pub meta_body: String,
    pub min_size: Option<u64>,
    pub max_depth: Option<u64>,
    pub engine: &'a str,
    pub started: &'a str,
    pub roots: &'a [Vec<u16>],
    pub totals: Totals,
    pub coverage: &'a Coverage,
}

impl Builder {
    /// 序列化成 `voltdir.index/1`（單一 JSON，結尾一個換行）。
    pub fn finish(self, h: &Header<'_>) -> Vec<u8> {
        let b = self;
        use std::io::Write;
        let mut out: Vec<u8> = Vec::with_capacity(1 << 20);
        out.extend_from_slice(b"{\"schema\":");
        json::push_str(&mut out, SCHEMA);
        match h.snapshot {
            Some((len, ns, hash)) => {
                let _ = write!(
                    out,
                    ",\"snapshot\":{{\"bytes\":{len},\"mtime_ns\":{ns},\"summary_hash\":\"{hash:016x}\"}}"
                );
            }
            None => out.extend_from_slice(b",\"snapshot\":null"),
        }
        out.extend_from_slice(b",\"meta_body\":");
        json::push_str(&mut out, &h.meta_body);
        let _ = write!(
            out,
            ",\"min_size\":{},\"max_depth\":{}",
            h.min_size.map_or("null".to_string(), |v| v.to_string()),
            h.max_depth.map_or("null".to_string(), |v| v.to_string())
        );
        out.extend_from_slice(b",\"engine\":");
        json::push_str(&mut out, h.engine);
        out.extend_from_slice(b",\"started\":");
        json::push_str(&mut out, h.started);
        out.extend_from_slice(b",\"roots\":[");
        for (i, r) in h.roots.iter().enumerate() {
            if i > 0 {
                out.push(b',');
            }
            json::push_str_u16(&mut out, r);
        }
        let _ = write!(
            out,
            "],\"totals\":{{\"files\":{},\"dirs\":{},\"logical\":{},\"physical\":{}}},\"coverage\":",
            h.totals.files, h.totals.dirs, h.totals.logical, h.totals.physical
        );
        write_coverage(&mut out, h.coverage);
        let _ = write!(
            out,
            ",\"empties\":{{\"files\":{},\"dirs\":{}}}",
            b.empty_files, b.empty_dirs
        );
        // 資料夾：每個深度前 1000 名（實際大小）與各深度的資料夾數
        let _ = write!(
            out,
            ",\"dirs\":{{\"nonroot\":{},\"nonroot_direct\":{},\"many_candidates\":{},\"depth_counts\":[",
            b.nonroot, b.nonroot_direct, b.many_candidates
        );
        for (i, c) in b.depth_counts.iter().enumerate() {
            if i > 0 {
                out.push(b',');
            }
            let _ = write!(out, "{c}");
        }
        out.extend_from_slice(b"],\"by_depth\":[");
        for (i, l) in b.by_depth.into_iter().enumerate() {
            if i > 0 {
                out.push(b',');
            }
            push_dirs(&mut out, l.into_sorted());
        }
        out.extend_from_slice(b"],\"top_rollup_logical\":");
        push_dirs(&mut out, b.top_rl.into_sorted());
        out.extend_from_slice(b",\"top_direct_physical\":");
        push_dirs(&mut out, b.top_dp.into_sorted());
        out.extend_from_slice(b",\"top_direct_logical\":");
        push_dirs(&mut out, b.top_dl.into_sorted());
        out.extend_from_slice(b",\"many\":");
        push_dirs(&mut out, b.many.into_sorted());
        out.push(b'}');
        // 檔案
        let _ = write!(
            out,
            ",\"files\":{{\"primary\":{},\"by_physical\":",
            b.primary
        );
        push_files(&mut out, &b.files_p.into_sorted());
        out.extend_from_slice(b",\"by_logical\":");
        push_files(&mut out, &b.files_l.into_sorted());
        let _ = write!(out, ",\"big_complete\":{},\"big\":", b.big_complete);
        push_files(&mut out, &b.big);
        out.push(b'}');
        // 副檔名表
        let mut exts: Vec<(&Vec<u16>, &ExtAgg)> = b.exts.iter().collect();
        exts.sort_by(|a, c| cmp_names(a.0, c.0));
        out.extend_from_slice(b",\"exts\":[");
        for (i, (e, a)) in exts.iter().enumerate() {
            if i > 0 {
                out.push(b',');
            }
            out.push(b'[');
            json::push_str_u16(&mut out, e);
            let _ = write!(out, ",{},{},{}]", a.files, a.logical, a.physical);
        }
        out.extend_from_slice(b"]}\n");
        out
    }
}

/// 為剛寫好的快照 `snapshot` 建立索引內容。`data` 與 `emit` 必須是寫出該快照時用的那份，
/// 這樣索引看到的項目才和讀快照時看到的完全一樣（例如 `--min-size` 省略的檔案）。
pub fn build(snapshot: &Path, data: &ScanData, emit: Emit) -> Result<Vec<u8>, String> {
    let meta = std::fs::metadata(snapshot).map_err(|e| format!("cannot stat the snapshot: {e}"))?;
    let (len, ns) = (meta.len(), mtime_ns(&meta)?);
    let summary = summary_line(snapshot, len)?;
    let (totals, coverage) = snapread::parse_summary(&summary)?;
    let head = {
        let f =
            std::fs::File::open(snapshot).map_err(|e| format!("cannot open the snapshot: {e}"))?;
        let mut r = BufReader::new(f);
        snapread::Reader::open(&mut r)?.meta().clone()
    };

    let mut b = Builder::new();
    let max_depth = emit.max_depth.map_or(usize::MAX, |d| d as usize);
    let mut ext_buf: Vec<u16> = Vec::new();
    crate::snapshot::for_each_entry(data, &mut |e| match e {
        Entry::Dir { path, node, depth } => {
            if depth > max_depth {
                return;
            }
            b.dir(&DirView {
                path,
                depth: depth as u64,
                rollup: node.rollup,
                direct: node.direct,
                latest_mtime: node.latest_mtime,
            });
        }
        Entry::File {
            path,
            dir,
            file,
            depth,
        } => {
            if depth > max_depth || emit.min_size.is_some_and(|m| file.physical < m) {
                return;
            }
            ext_buf.clear();
            ext_buf.extend_from_slice(&crate::names::ext_of(dir.file_name(file)));
            b.file(&FileView {
                path,
                ext: &ext_buf,
                logical: file.logical,
                physical: file.physical,
                mtime: file.mtime,
                secondary: file.secondary,
            });
        }
    });

    let hdr = Header {
        snapshot: Some((len, ns, fnv1a(&summary))),
        meta_body: String::from_utf8_lossy(&head.meta_body).into_owned(),
        min_size: head.min_size,
        max_depth: head.max_depth,
        engine: &head.engine,
        started: &head.started,
        roots: &head.roots,
        totals,
        coverage: &coverage,
    };
    Ok(b.finish(&hdr))
}

// ---- 載入與重播 ----

struct DirR {
    path: Vec<u16>,
    depth: u64,
    rollup: Totals,
    direct: Totals,
    latest: u64,
}

struct FileR {
    path: Vec<u16>,
    ext: Vec<u16>,
    logical: u64,
    physical: u64,
    mtime: u64,
}

pub struct Index {
    pub parsed: Parsed,
    empties: (u64, u64),
    nonroot: u64,
    nonroot_direct: u64,
    many_candidates: u64,
    depth_counts: Vec<u64>,
    by_depth: Vec<Vec<DirR>>,
    top_rl: Vec<DirR>,
    top_dp: Vec<DirR>,
    top_dl: Vec<DirR>,
    many: Vec<DirR>,
    primary: u64,
    by_physical: Vec<FileR>,
    by_logical: Vec<FileR>,
    big_complete: bool,
    big: Vec<FileR>,
    exts: Vec<(Vec<u16>, u64, u64, u64)>,
}

fn num(v: &Val<'_>) -> Result<u64, String> {
    v.as_u64()
        .ok_or_else(|| "a number is missing or negative".to_string())
}

fn field<'v, 'a>(v: &'v Val<'a>, k: &str) -> Result<&'v Val<'a>, String> {
    v.get(k).ok_or_else(|| format!("field '{k}' is missing"))
}

fn arr<'v, 'a>(v: &'v Val<'a>) -> Result<&'v [Val<'a>], String> {
    match v {
        Val::Arr(a) => Ok(a),
        _ => Err("an array was expected".into()),
    }
}

fn dirs_of(v: &Val<'_>) -> Result<Vec<DirR>, String> {
    arr(v)?
        .iter()
        .map(|r| {
            let a = arr(r)?;
            if a.len() != 10 {
                return Err("a folder record must have 10 fields".to_string());
            }
            Ok(DirR {
                path: a[0].as_units().ok_or("a path is not a string")?.to_vec(),
                depth: num(&a[1])?,
                rollup: Totals {
                    files: num(&a[2])?,
                    dirs: num(&a[3])?,
                    logical: num(&a[4])?,
                    physical: num(&a[5])?,
                },
                direct: Totals {
                    files: num(&a[6])?,
                    dirs: 0,
                    logical: num(&a[7])?,
                    physical: num(&a[8])?,
                },
                latest: num(&a[9])?,
            })
        })
        .collect()
}

fn files_of(v: &Val<'_>) -> Result<Vec<FileR>, String> {
    arr(v)?
        .iter()
        .map(|r| {
            let a = arr(r)?;
            if a.len() != 5 {
                return Err("a file record must have 5 fields".to_string());
            }
            Ok(FileR {
                path: a[0].as_units().ok_or("a path is not a string")?.to_vec(),
                ext: a[1]
                    .as_units()
                    .ok_or("an extension is not a string")?
                    .to_vec(),
                logical: num(&a[2])?,
                physical: num(&a[3])?,
                mtime: num(&a[4])?,
            })
        })
        .collect()
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

impl Index {
    /// 讀取獨立的索引檔（`overview --save-index` 寫出的，沒有對應的快照可比對）。
    pub fn load_standalone(path: &Path) -> Result<Index, String> {
        let text =
            std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let line: &[u8] = text
            .strip_suffix(
                b"
",
            )
            .unwrap_or(&text);
        let v = jsonin::parse(line)
            .map_err(|e| format!("{} is not a voltdir index ({e})", path.display()))?;
        let schema = v.get("schema").and_then(Val::as_string).unwrap_or_default();
        if schema != SCHEMA {
            return Err(format!(
                "{} is not a voltdir index (schema '{schema}', expected {SCHEMA})",
                path.display()
            ));
        }
        Self::from_value(&v).map_err(|e| format!("the index {} is damaged ({e})", path.display()))
    }

    /// 讀取 `snapshot` 旁的索引並確認它仍然對應這份快照。失敗時回傳要印在 `index: not used (...)` 的原因。
    pub fn load(snapshot: &Path) -> Result<Index, String> {
        let ipath = path_of(snapshot);
        let text = match std::fs::read(&ipath) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err("no index file next to the snapshot".into());
            }
            Err(e) => return Err(format!("cannot read the index: {e}")),
        };
        let line: &[u8] = text.strip_suffix(b"\n").unwrap_or(&text);
        let v = jsonin::parse(line).map_err(|e| format!("the index is damaged ({e})"))?;
        let damaged = |e: String| format!("the index is damaged ({e})");
        let schema = field(&v, "schema")
            .map_err(damaged)?
            .as_string()
            .unwrap_or_default();
        if schema != SCHEMA {
            return Err(format!("unsupported index schema '{schema}'"));
        }
        // 先驗證它對應的是這份快照
        let snap = field(&v, "snapshot").map_err(damaged)?;
        let (bytes, ns) = (
            num(field(snap, "bytes").map_err(damaged)?).map_err(damaged)?,
            field(snap, "mtime_ns").map_err(damaged)?,
        );
        let meta =
            std::fs::metadata(snapshot).map_err(|e| format!("cannot stat the snapshot: {e}"))?;
        if meta.len() != bytes {
            return Err("the snapshot changed since the index was written (size differs)".into());
        }
        let ns_now = mtime_ns(&meta)?;
        let ns_idx = match ns {
            Val::Num(n) => *n,
            _ => return Err(damaged("mtime_ns".into())),
        };
        if ns_now != ns_idx {
            return Err(
                "the snapshot changed since the index was written (modified time differs)".into(),
            );
        }
        let want = field(snap, "summary_hash")
            .map_err(damaged)?
            .as_string()
            .unwrap_or_default();
        let have = format!("{:016x}", fnv1a(&summary_line(snapshot, meta.len())?));
        if want != have {
            return Err(
                "the snapshot changed since the index was written (summary differs)".into(),
            );
        }
        Self::from_value(&v).map_err(|e| format!("the index is damaged ({e})"))
    }

    fn from_value(v: &Val<'_>) -> Result<Index, String> {
        let s = |k: &str| -> Result<String, String> {
            Ok(field(v, k)?.as_string().unwrap_or_default())
        };
        let opt = |k: &str| -> Result<Option<u64>, String> {
            let x = field(v, k)?;
            Ok(x.as_u64())
        };
        let totals_v = field(v, "totals")?;
        let totals = Totals {
            files: num(field(totals_v, "files")?)?,
            dirs: num(field(totals_v, "dirs")?)?,
            logical: num(field(totals_v, "logical")?)?,
            physical: num(field(totals_v, "physical")?)?,
        };
        let coverage: Coverage = snapread::coverage_of(field(v, "coverage")?, 0)?;
        let roots = arr(field(v, "roots")?)?
            .iter()
            .filter_map(|r| r.as_units().map(<[u16]>::to_vec))
            .collect();
        let parsed = Parsed {
            meta_body: s("meta_body")?.into_bytes(),
            min_size: opt("min_size")?,
            max_depth: opt("max_depth")?,
            totals,
            coverage,
            roots,
            started: s("started")?,
            engine: s("engine")?,
        };
        let emp = field(v, "empties")?;
        let d = field(v, "dirs")?;
        let f = field(v, "files")?;
        let mut by_depth = Vec::new();
        for l in arr(field(d, "by_depth")?)? {
            by_depth.push(dirs_of(l)?);
        }
        let mut exts = Vec::new();
        for r in arr(field(v, "exts")?)? {
            let a = arr(r)?;
            if a.len() != 4 {
                return Err("an extension record must have 4 fields".into());
            }
            exts.push((
                a[0].as_units()
                    .ok_or("an extension is not a string")?
                    .to_vec(),
                num(&a[1])?,
                num(&a[2])?,
                num(&a[3])?,
            ));
        }
        Ok(Index {
            parsed,
            empties: (num(field(emp, "files")?)?, num(field(emp, "dirs")?)?),
            nonroot: num(field(d, "nonroot")?)?,
            nonroot_direct: num(field(d, "nonroot_direct")?)?,
            many_candidates: num(field(d, "many_candidates")?)?,
            depth_counts: arr(field(d, "depth_counts")?)?
                .iter()
                .map(num)
                .collect::<Result<_, _>>()?,
            by_depth,
            top_rl: dirs_of(field(d, "top_rollup_logical")?)?,
            top_dp: dirs_of(field(d, "top_direct_physical")?)?,
            top_dl: dirs_of(field(d, "top_direct_logical")?)?,
            many: dirs_of(field(d, "many")?)?,
            primary: num(field(f, "primary")?)?,
            by_physical: files_of(field(f, "by_physical")?)?,
            by_logical: files_of(field(f, "by_logical")?)?,
            big_complete: field(f, "big_complete")?.as_bool().unwrap_or(false),
            big: files_of(field(f, "big")?)?,
            exts,
        })
    }

    /// 把 `overview` 的答案重播給 `col`。請求超出索引能回答的範圍時回傳原因，且不動 `col`。
    pub fn replay_overview(
        &self,
        col: &mut overview::Collector,
        o: &overview::Options,
    ) -> Result<(), String> {
        if o.stale_min_size < BIG_MIN {
            return Err(format!(
                "--stale-min-size is below the {} MiB the index keeps",
                BIG_MIN >> 20
            ));
        }
        if !self.big_complete {
            return Err("the index lists only the first 1000 files of 100 MiB or more".into());
        }
        let dirs: Vec<&DirR> = match (o.dir_direct, o.rank_logical) {
            (false, false) => self
                .by_depth
                .iter()
                .skip(1)
                .flat_map(|l| l.iter())
                .collect(),
            (false, true) => self.top_rl.iter().collect(),
            (true, false) => self.top_dp.iter().collect(),
            (true, true) => self.top_dl.iter().collect(),
        };
        let dir_candidates = if o.dir_direct {
            self.nonroot_direct
        } else {
            self.nonroot
        };
        for d in dirs {
            col.replay_dir(&DirView {
                path: &d.path,
                depth: d.depth,
                rollup: d.rollup,
                direct: d.direct,
                latest_mtime: d.latest,
            });
        }
        let files = if o.rank_logical {
            &self.by_logical
        } else {
            &self.by_physical
        };
        for f in files {
            col.replay_file(&view(f));
        }
        col.set_candidates(dir_candidates, self.primary);
        let cutoff = col.stale_cutoff();
        for f in &self.big {
            if f.physical >= o.stale_min_size && f.mtime < cutoff {
                col.replay_stale(&view(f));
            }
        }
        col.set_exts(self.exts.clone());
        col.set_empties(self.empties.0, self.empties.1);
        Ok(())
    }

    /// 把 `report` 的答案重播給 `an`。
    pub fn replay_report(&self, an: &mut Analyzer, req: &report::Request) -> Result<(), String> {
        if req.min_size.is_some() || req.exts.is_some() || req.older_than_days.is_some() {
            return Err("the index cannot apply --min-size, --ext or --older-than".into());
        }
        for n in [req.top_files, req.top_dirs, req.by_ext, req.many_files]
            .into_iter()
            .flatten()
        {
            if n > FILE_K {
                return Err("the index keeps at most 1000 items per list".into());
            }
        }
        let mut dir_candidates = 0;
        if req.top_dirs.is_some() {
            let last = req
                .dir_depth
                .map_or(usize::MAX, |d| usize::try_from(d).unwrap_or(usize::MAX));
            for (depth, list) in self.by_depth.iter().enumerate() {
                if depth > last {
                    break;
                }
                dir_candidates += self.depth_counts.get(depth).copied().unwrap_or(0);
                for d in list {
                    an.replay_top_dir(&dview(d));
                }
            }
        }
        if req.many_files.is_some() {
            for d in &self.many {
                an.replay_many(&dview(d));
            }
        }
        if req.top_files.is_some() {
            for f in &self.by_physical {
                an.replay_top_file(&view(f));
            }
        }
        if req.by_ext.is_some() {
            an.set_ext_table(self.exts.clone());
        }
        an.set_candidates(dir_candidates, self.many_candidates, self.primary);
        Ok(())
    }
}

fn view(f: &FileR) -> FileView<'_> {
    FileView {
        path: &f.path,
        ext: &f.ext,
        logical: f.logical,
        physical: f.physical,
        mtime: f.mtime,
        secondary: false,
    }
}

fn dview(d: &DirR) -> DirView<'_> {
    DirView {
        path: &d.path,
        depth: d.depth,
        rollup: d.rollup,
        direct: d.direct,
        latest_mtime: d.latest,
    }
}
