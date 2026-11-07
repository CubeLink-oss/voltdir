//! 快照旁的名稱索引：`<FILE>.nidx`，讓 `find --snapshot` 不必把整份快照解析一遍。
//!
//! `find --snapshot` 原本要串流解析每一行（258 萬檔的整顆使用者目錄約 25 秒以上），不管篩選條件
//! 是什麼，時間都花在解析。這裡把每個 `dir`／`file` 行的欄位存成欄式陣列，另外記錄原始行在快照裡
//! 的位置：查詢先對便宜的數值欄位做緊湊迴圈，通過的才比對名稱，最後才沿父目錄重建路徑；
//! 贏家再從快照讀回原始行，所以輸出與串流路徑逐位元組相同。
//!
//! 索引只是加速器：指紋（快照長度、修改時間、結尾 `summary` 行的雜湊）不符、任何區段的雜湊不符、
//! 讀回的原始行與預期不同，都回傳 `Err`，呼叫端改走串流路徑。檔案格式（小端序）：
//! 檔頭（魔術字、版本、項目數、快照指紋、區段表、檔頭雜湊）加 12 個可獨立讀取、各有雜湊的區段。

use crate::find::{Cand, Matcher, Meta, Sort};
use crate::index::{fnv1a, mtime_ns, summary_line};
use crate::jsonin;
use crate::model::ScanData;
use crate::names::ext_of;
use crate::snapshot::Layout;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const MAGIC: [u8; 4] = *b"VDNI";
pub const VERSION: u32 = 1;
/// 沒有父目錄（根），或沒有副檔名編號（目錄）。
const NONE: u32 = u32::MAX;
const FLAG_DIR: u8 = 1;
const FLAG_EMPTY: u8 = 2;

const S_LINE_OFF: usize = 0;
const S_LINE_LEN: usize = 1;
const S_PARENT: usize = 2;
const S_NAME_OFF: usize = 3;
const S_NAME_LEN: usize = 4;
const S_EXT_ID: usize = 5;
const S_FLAGS: usize = 6;
const S_PHYSICAL: usize = 7;
const S_LOGICAL: usize = 8;
const S_MTIME: usize = 9;
const S_NAMES: usize = 10;
const S_EXTS: usize = 11;
const S_COUNT: usize = 12;

/// 項目數少於這個數字時，第一階段不值得開執行緒。
const PARALLEL_MIN: usize = 200_000;

/// 魔術字、版本、項目數、快照長度、快照修改時間、summary 雜湊。
const FIXED: usize = 4 + 4 + 8 + 8 + 8 + 8;
/// 檔頭：固定欄位 + 區段表（位置、長度、雜湊）+ 檔頭自己的雜湊。
const HEADER_LEN: usize = FIXED + S_COUNT * 24 + 8;

/// 索引檔的路徑：快照路徑後面加 `.nidx`。
pub fn path_of(snapshot: &Path) -> PathBuf {
    let mut s = snapshot.as_os_str().to_owned();
    s.push(".nidx");
    PathBuf::from(s)
}

/// 區段與檔頭用的雜湊：每次吃 8 個位元組，任何單一位元組的改變都會改變結果。
fn hash64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ bytes.len() as u64;
    let mut chunks = bytes.chunks_exact(8);
    for c in &mut chunks {
        let w = u64::from_le_bytes(c.try_into().expect("8 bytes"));
        h = (h ^ w).wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(29);
    }
    let rest = chunks.remainder();
    if !rest.is_empty() {
        let mut last = [0u8; 8];
        last[..rest.len()].copy_from_slice(rest);
        h = (h ^ u64::from_le_bytes(last))
            .wrapping_mul(0x9e37_79b9_7f4a_7c15)
            .rotate_left(29);
    }
    h
}

/// 一個快照行（`dir` 或 `file`）要進索引的欄位。
pub struct Row<'a> {
    pub is_dir: bool,
    /// 目錄：與快照的 `depth` 相同；檔案：所在目錄的深度加 1
    pub depth: u64,
    pub name: &'a [u16],
    pub ext: &'a [u16],
    pub logical: u64,
    pub physical: u64,
    pub mtime: u64,
    pub dir_empty: bool,
    pub line_off: u64,
    /// 含換行
    pub line_len: u32,
}

#[derive(Default)]
pub struct Builder {
    line_off: Vec<u64>,
    line_len: Vec<u32>,
    parent: Vec<u32>,
    name_off: Vec<u32>,
    name_len: Vec<u16>,
    ext_id: Vec<u32>,
    flags: Vec<u8>,
    physical: Vec<u64>,
    logical: Vec<u64>,
    mtime: Vec<u64>,
    names: Vec<u16>,
    exts: Vec<Vec<u16>>,
    ext_ids: HashMap<Vec<u16>, u32>,
    /// 各深度最近一個目錄的項目編號
    dirs: Vec<u32>,
    bad: Option<String>,
}

impl Builder {
    pub fn new() -> Builder {
        Builder::default()
    }

    pub fn len(&self) -> usize {
        self.flags.len()
    }

    fn fail(&mut self, why: &str) {
        if self.bad.is_none() {
            self.bad = Some(why.to_string());
        }
    }

    /// 依快照的標準順序（前序）一筆一筆加入。
    pub fn push(&mut self, r: &Row<'_>) {
        let idx = self.flags.len();
        let depth = r.depth as usize;
        let parent = if r.is_dir {
            if depth == 0 {
                NONE
            } else if let Some(&p) = self.dirs.get(depth - 1) {
                p
            } else {
                self.fail("a folder appears before its parent folder");
                NONE
            }
        } else if depth >= 1 && depth <= self.dirs.len() {
            self.dirs[depth - 1]
        } else {
            self.fail("a file appears outside any folder");
            NONE
        };
        if r.is_dir {
            if depth > self.dirs.len() {
                self.fail("folder depth jumps by more than one level");
            } else {
                self.dirs.truncate(depth);
                self.dirs.push(idx as u32);
            }
        }
        let name_len = match u16::try_from(r.name.len()) {
            Ok(n) => n,
            Err(_) => {
                self.fail("a name is longer than 65535 UTF-16 units");
                0
            }
        };
        let name_off = match u32::try_from(self.names.len()) {
            Ok(o) => o,
            Err(_) => {
                self.fail("the names do not fit in 4 Gi UTF-16 units");
                0
            }
        };
        self.names.extend_from_slice(r.name);
        let ext_id = if r.is_dir {
            NONE
        } else if let Some(&id) = self.ext_ids.get(r.ext) {
            id
        } else {
            let id = self.exts.len() as u32;
            self.exts.push(r.ext.to_vec());
            self.ext_ids.insert(r.ext.to_vec(), id);
            id
        };
        self.line_off.push(r.line_off);
        self.line_len.push(r.line_len);
        self.parent.push(parent);
        self.name_off.push(name_off);
        self.name_len.push(name_len);
        self.ext_id.push(ext_id);
        self.flags
            .push((u8::from(r.is_dir) * FLAG_DIR) | (u8::from(r.dir_empty) * FLAG_EMPTY));
        self.physical.push(r.physical);
        self.logical.push(r.logical);
        self.mtime.push(r.mtime);
    }

    /// 序列化。`snapshot` 必須已經完整寫好（指紋取自它的長度、修改時間與結尾）。
    pub fn finish(self, snapshot: &Path) -> Result<Vec<u8>, String> {
        if let Some(why) = self.bad {
            return Err(why);
        }
        let meta =
            std::fs::metadata(snapshot).map_err(|e| format!("cannot stat the snapshot: {e}"))?;
        let bytes = meta.len();
        let ns = mtime_ns(&meta)?;
        let sum = fnv1a(&summary_line(snapshot, bytes)?);

        fn le<T: Copy, const N: usize>(v: &[T], f: impl Fn(T) -> [u8; N]) -> Vec<u8> {
            let mut out = Vec::with_capacity(v.len() * N);
            for &x in v {
                out.extend_from_slice(&f(x));
            }
            out
        }
        let mut ext_table = Vec::new();
        ext_table.extend_from_slice(&(self.exts.len() as u32).to_le_bytes());
        for e in &self.exts {
            let n = u16::try_from(e.len()).map_err(|_| "an extension is too long".to_string())?;
            ext_table.extend_from_slice(&n.to_le_bytes());
            for u in e {
                ext_table.extend_from_slice(&u.to_le_bytes());
            }
        }
        let mut secs: Vec<Vec<u8>> = vec![Vec::new(); S_COUNT];
        secs[S_LINE_OFF] = le(&self.line_off, u64::to_le_bytes);
        secs[S_LINE_LEN] = le(&self.line_len, u32::to_le_bytes);
        secs[S_PARENT] = le(&self.parent, u32::to_le_bytes);
        secs[S_NAME_OFF] = le(&self.name_off, u32::to_le_bytes);
        secs[S_NAME_LEN] = le(&self.name_len, u16::to_le_bytes);
        secs[S_EXT_ID] = le(&self.ext_id, u32::to_le_bytes);
        secs[S_FLAGS] = self.flags.clone();
        secs[S_PHYSICAL] = le(&self.physical, u64::to_le_bytes);
        secs[S_LOGICAL] = le(&self.logical, u64::to_le_bytes);
        secs[S_MTIME] = le(&self.mtime, u64::to_le_bytes);
        secs[S_NAMES] = le(&self.names, u16::to_le_bytes);
        secs[S_EXTS] = ext_table;

        let mut head = Vec::with_capacity(HEADER_LEN);
        head.extend_from_slice(&MAGIC);
        head.extend_from_slice(&VERSION.to_le_bytes());
        head.extend_from_slice(&(self.flags.len() as u64).to_le_bytes());
        head.extend_from_slice(&bytes.to_le_bytes());
        head.extend_from_slice(&ns.to_le_bytes());
        head.extend_from_slice(&sum.to_le_bytes());
        let mut off = HEADER_LEN as u64;
        for s in &secs {
            head.extend_from_slice(&off.to_le_bytes());
            head.extend_from_slice(&(s.len() as u64).to_le_bytes());
            head.extend_from_slice(&hash64(s).to_le_bytes());
            off += s.len() as u64;
        }
        let hh = hash64(&head);
        head.extend_from_slice(&hh.to_le_bytes());
        debug_assert_eq!(head.len(), HEADER_LEN);
        let mut out = head;
        out.reserve((off as usize).saturating_sub(HEADER_LEN));
        for s in &secs {
            out.extend_from_slice(s);
        }
        Ok(out)
    }
}

/// 寫快照時順便建索引：每個項目的欄位取自掃描資料，位置取自 `layout`。
/// 欄位的來源與 `snapshot::write_dir`／`write_file` 寫進該行的值相同，
/// 所以和 `snapread` 讀回的 `Rec` 一致（`tests/nidx.rs` 逐項比對）。
pub fn build_from_scan(
    snapshot: &Path,
    data: &ScanData,
    layout: &Layout,
) -> Result<Vec<u8>, String> {
    let mut b = Builder::new();
    let mut off = layout.head_len;
    for (it, &len) in layout.items.iter().zip(&layout.line_lens) {
        let node = &data.dirs[it.dir as usize];
        if it.idx == u32::MAX {
            b.push(&Row {
                is_dir: true,
                depth: u64::from(it.depth),
                name: &node.name,
                ext: &[],
                logical: node.rollup.logical,
                physical: node.rollup.physical,
                mtime: node.latest_mtime,
                dir_empty: node.rollup.files == 0 && node.rollup.dirs == 0,
                line_off: off,
                line_len: len,
            });
        } else {
            let f = &node.files[it.idx as usize];
            let name = node.file_name(f);
            b.push(&Row {
                is_dir: false,
                depth: u64::from(it.depth),
                name,
                ext: &ext_of(name),
                logical: f.logical,
                physical: f.physical,
                mtime: f.mtime,
                dir_empty: false,
                line_off: off,
                line_len: len,
            });
        }
        off += u64::from(len);
    }
    b.finish(snapshot)
}

#[derive(Clone, Copy, Default)]
struct Sec {
    off: u64,
    len: u64,
    hash: u64,
}

pub struct NameIndex {
    file: File,
    n: usize,
    secs: [Sec; S_COUNT],
}

fn damaged(why: &str) -> String {
    format!("the name index is damaged ({why})")
}

fn rd_u64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

impl NameIndex {
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.n
    }

    /// 讀取 `snapshot` 旁的索引並確認它仍然對應這份快照。失敗時回傳要印在
    /// `index: not used (...)` 的原因。
    pub fn load(snapshot: &Path) -> Result<NameIndex, String> {
        let ipath = path_of(snapshot);
        let mut file = match File::open(&ipath) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err("no name index file next to the snapshot".into());
            }
            Err(e) => return Err(format!("cannot read the name index: {e}")),
        };
        let flen = file
            .metadata()
            .map_err(|e| format!("cannot read the name index: {e}"))?
            .len();
        if flen < HEADER_LEN as u64 {
            return Err(damaged("too short"));
        }
        let mut head = vec![0u8; HEADER_LEN];
        file.read_exact(&mut head)
            .map_err(|e| format!("cannot read the name index: {e}"))?;
        if head[..4] != MAGIC {
            return Err("not a name index file".into());
        }
        let version = u32::from_le_bytes(head[4..8].try_into().expect("4 bytes"));
        if version != VERSION {
            return Err(format!("unsupported name index version {version}"));
        }
        if hash64(&head[..HEADER_LEN - 8]) != rd_u64(&head, HEADER_LEN - 8) {
            return Err(damaged("header checksum"));
        }
        let n = rd_u64(&head, 8);
        let (snap_bytes, snap_ns, snap_sum) = (
            rd_u64(&head, 16),
            rd_u64(&head, 24) as i64,
            rd_u64(&head, 32),
        );
        let mut secs = [Sec::default(); S_COUNT];
        for (i, s) in secs.iter_mut().enumerate() {
            let at = FIXED + i * 24;
            *s = Sec {
                off: rd_u64(&head, at),
                len: rd_u64(&head, at + 8),
                hash: rd_u64(&head, at + 16),
            };
            let end = s
                .off
                .checked_add(s.len)
                .ok_or_else(|| damaged("section range"))?;
            if s.off < HEADER_LEN as u64 || end > flen {
                return Err(damaged("section outside the file"));
            }
        }
        let want = |i: usize, unit: u64| -> Result<(), String> {
            if n.checked_mul(unit) == Some(secs[i].len) {
                Ok(())
            } else {
                Err(damaged("section length does not match the entry count"))
            }
        };
        want(S_LINE_OFF, 8)?;
        want(S_LINE_LEN, 4)?;
        want(S_PARENT, 4)?;
        want(S_NAME_OFF, 4)?;
        want(S_NAME_LEN, 2)?;
        want(S_EXT_ID, 4)?;
        want(S_FLAGS, 1)?;
        want(S_PHYSICAL, 8)?;
        want(S_LOGICAL, 8)?;
        want(S_MTIME, 8)?;
        if secs[S_NAMES].len % 2 != 0 {
            return Err(damaged("names section has an odd length"));
        }
        let n = usize::try_from(n).map_err(|_| damaged("entry count"))?;

        // 先驗證它對應的是這份快照
        let meta =
            std::fs::metadata(snapshot).map_err(|e| format!("cannot stat the snapshot: {e}"))?;
        if meta.len() != snap_bytes {
            return Err(
                "the snapshot changed since the name index was written (size differs)".into(),
            );
        }
        if mtime_ns(&meta)? != snap_ns {
            return Err(
                "the snapshot changed since the name index was written (modified time differs)"
                    .into(),
            );
        }
        if fnv1a(&summary_line(snapshot, meta.len())?) != snap_sum {
            return Err(
                "the snapshot changed since the name index was written (summary differs)".into(),
            );
        }
        Ok(NameIndex { file, n, secs })
    }

    fn bytes(&self, s: usize) -> Result<Vec<u8>, String> {
        let sec = self.secs[s];
        let mut f = &self.file;
        f.seek(SeekFrom::Start(sec.off))
            .map_err(|e| format!("cannot read the name index: {e}"))?;
        let mut v = vec![0u8; sec.len as usize];
        f.read_exact(&mut v)
            .map_err(|e| format!("cannot read the name index: {e}"))?;
        if hash64(&v) != sec.hash {
            return Err(damaged("section checksum"));
        }
        Ok(v)
    }

    fn u64s(&self, s: usize) -> Result<Vec<u64>, String> {
        Ok(self
            .bytes(s)?
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().expect("8 bytes")))
            .collect())
    }

    fn u32s(&self, s: usize) -> Result<Vec<u32>, String> {
        Ok(self
            .bytes(s)?
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().expect("4 bytes")))
            .collect())
    }

    fn u16s(&self, s: usize) -> Result<Vec<u16>, String> {
        Ok(self
            .bytes(s)?
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes(c.try_into().expect("2 bytes")))
            .collect())
    }

    fn ext_table(&self) -> Result<Vec<Vec<u16>>, String> {
        let b = self.bytes(S_EXTS)?;
        let bad = || damaged("extension table");
        let count = u32::from_le_bytes(b.get(..4).ok_or_else(bad)?.try_into().expect("4 bytes"));
        let mut at = 4usize;
        let mut out = Vec::new();
        for _ in 0..count {
            let n = u16::from_le_bytes(
                b.get(at..at + 2)
                    .ok_or_else(bad)?
                    .try_into()
                    .expect("2 bytes"),
            ) as usize;
            at += 2;
            let raw = b.get(at..at + n * 2).ok_or_else(bad)?;
            out.push(
                raw.chunks_exact(2)
                    .map(|c| u16::from_le_bytes(c.try_into().expect("2 bytes")))
                    .collect(),
            );
            at += n * 2;
        }
        if at != b.len() {
            return Err(bad());
        }
        Ok(out)
    }

    /// 回答 `matcher` 裡的 `find` 查詢。成功時 `matcher` 的內容與串流讀快照相同；
    /// 失敗（索引損壞、讀回的原始行與預期不同）時呼叫端必須丟掉 `matcher`，改走串流路徑。
    pub fn query(&self, snapshot: &Path, matcher: &mut Matcher) -> Result<(), String> {
        self.query_with(snapshot, matcher, PARALLEL_MIN)
    }

    /// 同 `query`；項目數達到 `parallel_min` 才把第一階段分給多個核心（測試用來強制兩種模式）。
    fn query_with(
        &self,
        snapshot: &Path,
        matcher: &mut Matcher,
        parallel_min: usize,
    ) -> Result<(), String> {
        let n = self.n;
        let (need_ext, need_name, need_logical, need_phys, need_mtime, limit, need_path_filter) = {
            let f = matcher.filter();
            (
                f.exts.is_some(),
                f.name_glob.is_some() || f.name_regex.is_some(),
                f.empty,
                f.min_size.is_some() || f.max_size.is_some() || matcher.sort() == Sort::Size,
                f.older_than.is_some() || f.newer_than.is_some() || matcher.sort() == Sort::Mtime,
                matcher.limit(),
                f.needs_path(),
            )
        };
        let need_path = limit > 0 || need_path_filter;
        let need_names = need_name || need_path;

        let flags = self.bytes(S_FLAGS)?;
        let ext_id = if need_ext {
            self.u32s(S_EXT_ID)?
        } else {
            Vec::new()
        };
        let logical = if need_logical {
            self.u64s(S_LOGICAL)?
        } else {
            Vec::new()
        };
        let physical = if need_phys {
            self.u64s(S_PHYSICAL)?
        } else {
            Vec::new()
        };
        let mtime = if need_mtime {
            self.u64s(S_MTIME)?
        } else {
            Vec::new()
        };
        let (name_off, name_len, names) = if need_names {
            (
                self.u32s(S_NAME_OFF)?,
                self.u16s(S_NAME_LEN)?,
                self.u16s(S_NAMES)?,
            )
        } else {
            (Vec::new(), Vec::new(), Vec::new())
        };
        let parent = if need_path {
            self.u32s(S_PARENT)?
        } else {
            Vec::new()
        };
        let (line_off, line_len) = if limit > 0 {
            (self.u64s(S_LINE_OFF)?, self.u32s(S_LINE_LEN)?)
        } else {
            (Vec::new(), Vec::new())
        };
        let exts = if need_ext {
            self.ext_table()?
        } else {
            Vec::new()
        };

        // 載入的欄位先做一次結構檢查，之後的迴圈才能放心用索引取值
        if need_names {
            for i in 0..n {
                let end = name_off[i] as usize + name_len[i] as usize;
                if end > names.len() {
                    return Err(damaged("a name lies outside the names section"));
                }
            }
        }
        if need_path {
            for (i, &p) in parent.iter().enumerate() {
                if p != NONE && p as usize >= i {
                    return Err(damaged("a parent does not precede its child"));
                }
            }
        }
        // 副檔名編號通過篩選與否，每個編號只算一次
        let ext_pass: Vec<bool> = if need_ext {
            let set = matcher.filter().exts.as_ref().expect("checked above");
            exts.iter().map(|e| set.contains(e)).collect()
        } else {
            Vec::new()
        };
        if need_ext {
            for (i, &id) in ext_id.iter().enumerate() {
                let is_dir = flags[i] & FLAG_DIR != 0;
                if !is_dir && id as usize >= exts.len() {
                    return Err(damaged("an extension number is out of range"));
                }
            }
        }

        // 第一階段（便宜的條件 + 名稱）：每個項目互相獨立，切成區塊在所有核心上同時做，
        // 通過的編號依原順序串起來；第二階段才一個一個交給 `matcher`，所以結果與單執行緒完全相同。
        let pass: Vec<u32> = {
            let filter = matcher.filter();
            let stage = |lo: usize, hi: usize| -> Vec<u32> {
                let mut out = Vec::new();
                let mut scratch = filter.name_scratch();
                for i in lo..hi {
                    let fl = flags[i];
                    let is_dir = fl & FLAG_DIR != 0;
                    if need_ext && (is_dir || !ext_pass[ext_id[i] as usize]) {
                        continue;
                    }
                    let at = |v: &Vec<u64>| if v.is_empty() { 0 } else { v[i] };
                    let m = Meta {
                        is_dir,
                        ext: &[],
                        logical: at(&logical),
                        physical: at(&physical),
                        mtime: at(&mtime),
                        dir_empty: fl & FLAG_EMPTY != 0,
                    };
                    if !filter.matches_meta_except_ext(&m) {
                        continue;
                    }
                    if need_name {
                        let name = &names[name_off[i] as usize..][..name_len[i] as usize];
                        if !filter.matches_name_with(name, &mut scratch) {
                            continue;
                        }
                    }
                    out.push(i as u32);
                }
                out
            };
            let workers = std::thread::available_parallelism()
                .map_or(4, |c| c.get())
                .min(16);
            if n < parallel_min || workers < 2 {
                stage(0, n)
            } else {
                let chunk = n.div_ceil(workers);
                std::thread::scope(|sc| {
                    let handles: Vec<_> = (0..workers)
                        .map(|w| {
                            let (lo, hi) = ((w * chunk).min(n), ((w + 1) * chunk).min(n));
                            let stage = &stage;
                            sc.spawn(move || stage(lo, hi))
                        })
                        .collect();
                    let mut all = Vec::new();
                    for h in handles {
                        all.extend(h.join().expect("filter worker"));
                    }
                    all
                })
            }
        };

        let mut snap = if limit > 0 {
            Some(File::open(snapshot).map_err(|e| format!("cannot open the snapshot: {e}"))?)
        } else {
            None
        };
        let failure: std::cell::RefCell<Option<String>> = std::cell::RefCell::new(None);
        let mut path: Vec<u16> = Vec::new();
        let mut chain: Vec<u32> = Vec::new();
        let sep = u16::from(b'\\');

        for &iu in &pass {
            let i = iu as usize;
            let fl = flags[i];
            let is_dir = fl & FLAG_DIR != 0;
            let at = |v: &Vec<u64>| if v.is_empty() { 0 } else { v[i] };
            let (logical_i, physical_i, mtime_i) = (at(&logical), at(&physical), at(&mtime));
            let name: &[u16] = if need_names {
                &names[name_off[i] as usize..name_off[i] as usize + name_len[i] as usize]
            } else {
                &[]
            };
            if need_path {
                chain.clear();
                let mut cur = i as u32;
                while cur != NONE {
                    chain.push(cur);
                    cur = parent[cur as usize];
                }
                path.clear();
                for &c in chain.iter().rev() {
                    let c = c as usize;
                    let nm =
                        &names[name_off[c] as usize..name_off[c] as usize + name_len[c] as usize];
                    if path.is_empty() {
                        path.extend_from_slice(nm);
                    } else {
                        if path.last() != Some(&sep) {
                            path.push(sep);
                        }
                        path.extend_from_slice(nm);
                    }
                }
                if need_path_filter && !matcher.filter().matches_path(&path) {
                    continue;
                }
            }
            let cand = Cand {
                is_dir,
                path: &path,
                name,
                ext: &[],
                logical: logical_i,
                physical: physical_i,
                mtime: mtime_i,
                dir_empty: fl & FLAG_EMPTY != 0,
            };
            matcher.offer_matched(&cand, || {
                let Some(f) = snap.as_mut() else {
                    return Vec::new();
                };
                match read_line(f, line_off[i], line_len[i], is_dir, &path) {
                    Ok(l) => l,
                    Err(e) => {
                        failure.borrow_mut().get_or_insert(e);
                        Vec::new()
                    }
                }
            });
            if failure.borrow().is_some() {
                break;
            }
        }
        match failure.into_inner() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

/// 從快照讀回一個項目的原始行（去掉換行），並確認它真的是預期的那一行。
fn read_line(
    f: &mut File,
    off: u64,
    len: u32,
    is_dir: bool,
    path: &[u16],
) -> Result<Vec<u8>, String> {
    let wrong = |why: &str| format!("the snapshot line does not match the name index ({why})");
    f.seek(SeekFrom::Start(off))
        .map_err(|e| format!("cannot read the snapshot: {e}"))?;
    let mut buf = vec![0u8; len as usize];
    f.read_exact(&mut buf)
        .map_err(|_| wrong("line outside the file"))?;
    while matches!(buf.last(), Some(b'\n' | b'\r')) {
        buf.pop();
    }
    let prefix: &[u8] = if is_dir {
        b"{\"type\":\"dir\""
    } else {
        b"{\"type\":\"file\""
    };
    if !buf.starts_with(prefix) {
        return Err(wrong("wrong line type"));
    }
    let v = jsonin::parse(&buf).map_err(|_| wrong("not valid JSON"))?;
    match v.get("path").and_then(jsonin::Val::as_units) {
        Some(p) if p == path => {}
        _ => return Err(wrong("different path")),
    }
    drop(v);
    Ok(buf)
}

/// 快照到現在過了多久（以快照檔的修改時間算起）。
pub fn snapshot_age_seconds(snapshot: &Path) -> Option<u64> {
    let m = std::fs::metadata(snapshot).ok()?.modified().ok()?;
    std::time::SystemTime::now()
        .duration_since(m)
        .ok()
        .map(|d| d.as_secs())
}

/// 給人看的年齡：`less than a minute`、`N minutes`、`N hours`（48 小時內）、`N days`。
pub fn age_text(secs: u64) -> String {
    fn unit(n: u64, name: &str) -> String {
        if n == 1 {
            format!("1 {name}")
        } else {
            format!("{n} {name}s")
        }
    }
    if secs < 60 {
        "less than a minute".to_string()
    } else if secs < 3600 {
        unit(secs / 60, "minute")
    } else if secs < 48 * 3600 {
        unit(secs / 3600, "hour")
    } else {
        unit(secs / 86400, "day")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    use crate::find::{Filter, Kind};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn filter() -> Filter {
        Filter {
            name_glob: None,
            name_regex: None,
            path_glob: None,
            kind: Kind::Any,
            exts: None,
            min_size: None,
            max_size: None,
            older_than: None,
            newer_than: None,
            empty: false,
        }
    }

    /// 手寫一份最小的快照與對應的索引；回傳（快照路徑，索引位元組，各項目的原始行）。
    fn mini() -> (PathBuf, Vec<u8>, Vec<String>) {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "voltdir-nidx-unit-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let snap = dir.join("s.ndjson");
        let entries = [
            (true, 0u64, r"C:\r", "", 0u64),
            (false, 1, "a.txt", ".txt", 10),
            (true, 1, "sub", "", 0),
            (false, 2, "資料.TXT", ".txt", 20),
        ];
        let lines: Vec<String> = [
            r#"{"type":"dir","path":"C:\\r","name":"C:\\r"}"#,
            r#"{"type":"file","path":"C:\\r\\a.txt","name":"a.txt"}"#,
            r#"{"type":"dir","path":"C:\\r\\sub","name":"sub"}"#,
            r#"{"type":"file","path":"C:\\r\\sub\\資料.TXT","name":"資料.TXT"}"#,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let mut text = String::from("{\"type\":\"meta\"}\n");
        let mut offs = Vec::new();
        for l in &lines {
            offs.push(text.len() as u64);
            text.push_str(l);
            text.push('\n');
        }
        text.push_str("{\"type\":\"summary\"}\n");
        std::fs::write(&snap, &text).unwrap();
        let mut b = Builder::new();
        for (i, (is_dir, depth, name, ext, size)) in entries.iter().enumerate() {
            let (name, ext) = (w(name), w(ext));
            b.push(&Row {
                is_dir: *is_dir,
                depth: *depth,
                name: &name,
                ext: &ext,
                logical: *size,
                physical: *size,
                mtime: 5,
                dir_empty: false,
                line_off: offs[i],
                line_len: lines[i].len() as u32 + 1,
            });
        }
        let bytes = b.finish(&snap).unwrap();
        std::fs::write(path_of(&snap), &bytes).unwrap();
        (snap, bytes, lines)
    }

    #[test]
    fn roundtrip_and_query() {
        let (snap, _, lines) = mini();
        let ix = NameIndex::load(&snap).unwrap();
        assert_eq!(ix.len(), 4);
        let mut f = filter();
        f.kind = Kind::File;
        f.name_glob = Some(w("*.txt"));
        let mut m = Matcher::new(f, Sort::Path, 10);
        ix.query(&snap, &mut m).unwrap();
        let (total, items, truncated) = m.finish();
        assert_eq!((total, truncated), (2, false));
        assert_eq!(
            items,
            vec![lines[1].clone().into_bytes(), lines[3].clone().into_bytes()]
        );
        // 只要總數：不讀欄位以外的任何東西
        let mut f = filter();
        f.kind = Kind::Dir;
        let mut m = Matcher::new(f, Sort::Path, 0);
        ix.query(&snap, &mut m).unwrap();
        assert_eq!(m.finish().0, 2);
        // 路徑條件：只有 sub 底下的
        let mut f = filter();
        f.path_glob = Some(w(r"C:\r\sub\*"));
        let mut m = Matcher::new(f, Sort::Path, 10);
        ix.query(&snap, &mut m).unwrap();
        assert_eq!(m.finish().1, vec![lines[3].clone().into_bytes()]);
    }

    #[test]
    fn parallel_and_sequential_stages_give_the_same_answer() {
        let (snap, _, _) = mini();
        let ix = NameIndex::load(&snap).unwrap();
        let mut filters: Vec<Box<dyn Fn() -> Filter>> = Vec::new();
        filters.push(Box::new(filter));
        filters.push(Box::new(|| {
            let mut f = filter();
            f.name_glob = Some(w("*.txt"));
            f
        }));
        filters.push(Box::new(|| {
            let mut f = filter();
            f.name_regex = Some(crate::regex::Regex::new(&w(r"^(a|資料)\.(txt)$")).unwrap());
            f
        }));
        filters.push(Box::new(|| {
            let mut f = filter();
            f.exts = Some([w(".txt")].into_iter().collect());
            f.min_size = Some(15);
            f
        }));
        filters.push(Box::new(|| {
            let mut f = filter();
            f.path_glob = Some(w(r"C:\r\sub\*"));
            f
        }));
        for (k, make) in filters.iter().enumerate() {
            for sort in [Sort::Path, Sort::Size, Sort::Mtime] {
                for limit in [0usize, 1, 10] {
                    let mut seq = Matcher::new(make(), sort, limit);
                    ix.query_with(&snap, &mut seq, usize::MAX).unwrap();
                    let mut par = Matcher::new(make(), sort, limit);
                    ix.query_with(&snap, &mut par, 0).unwrap();
                    assert!(
                        seq.finish() == par.finish(),
                        "filter {k} sort {sort:?} limit {limit}"
                    );
                }
            }
        }
    }

    #[test]
    fn every_damaged_byte_is_refused() {
        let (snap, bytes, _) = mini();
        // 一個用到所有欄位的查詢，任何區段的雜湊錯誤都會在讀取時被發現
        let mut f = filter();
        f.exts = Some([w(".txt")].into_iter().collect());
        f.name_glob = Some(w("*"));
        f.min_size = Some(0);
        f.older_than = Some(u64::MAX);
        f.empty = false;
        for pos in 0..bytes.len() {
            let mut bad = bytes.clone();
            bad[pos] ^= 0x01;
            std::fs::write(path_of(&snap), &bad).unwrap();
            let outcome = NameIndex::load(&snap).and_then(|ix| {
                let mut f = filter();
                f.exts = Some([w(".txt")].into_iter().collect());
                f.name_glob = Some(w("*"));
                f.min_size = Some(0);
                f.older_than = Some(u64::MAX);
                f.empty = true;
                let mut m = Matcher::new(f, Sort::Mtime, 5);
                ix.query(&snap, &mut m)
            });
            assert!(outcome.is_err(), "flipping byte {pos} was not noticed");
        }
        // 截斷到任何長度也不能 panic
        for cut in 0..bytes.len() {
            std::fs::write(path_of(&snap), &bytes[..cut]).unwrap();
            assert!(NameIndex::load(&snap).is_err(), "cut at {cut}");
        }
        std::fs::write(path_of(&snap), &bytes).unwrap();
        // 快照改過（長度不同）→ 索引過期
        let mut text = std::fs::read(&snap).unwrap();
        text.extend_from_slice(b"\n");
        std::fs::write(&snap, &text).unwrap();
        let why = NameIndex::load(&snap).err().unwrap();
        assert!(why.contains("snapshot changed"), "{why}");
        let _ = f;
    }

    #[test]
    fn age_text_units() {
        assert_eq!(age_text(0), "less than a minute");
        assert_eq!(age_text(59), "less than a minute");
        assert_eq!(age_text(60), "1 minute");
        assert_eq!(age_text(120), "2 minutes");
        assert_eq!(age_text(3599), "59 minutes");
        assert_eq!(age_text(3600), "1 hour");
        assert_eq!(age_text(172_799), "47 hours");
        assert_eq!(age_text(172_800), "2 days");
    }

    #[test]
    fn hash_sees_every_byte() {
        let base: Vec<u8> = (0..37u8).collect();
        let h = hash64(&base);
        for i in 0..base.len() {
            for bit in 0..8 {
                let mut m = base.clone();
                m[i] ^= 1 << bit;
                assert_ne!(hash64(&m), h, "byte {i} bit {bit}");
            }
        }
        assert_ne!(hash64(&base[..36]), h);
    }

    #[test]
    fn builder_assigns_parents_by_depth() {
        let mut b = Builder::new();
        let row = |is_dir, depth, name: &[u16]| Row {
            is_dir,
            depth,
            name: Box::leak(name.to_vec().into_boxed_slice()),
            ext: &[],
            logical: 0,
            physical: 0,
            mtime: 0,
            dir_empty: false,
            line_off: 0,
            line_len: 1,
        };
        b.push(&row(true, 0, &w("C:\\r"))); // 0
        b.push(&row(false, 1, &w("a.txt"))); // 1  parent 0
        b.push(&row(true, 1, &w("sub"))); // 2  parent 0
        b.push(&row(false, 2, &w("b.txt"))); // 3  parent 2
        b.push(&row(true, 1, &w("sub2"))); // 4  parent 0
        b.push(&row(false, 2, &w("資料.txt"))); // 5  parent 4
        b.push(&row(true, 0, &w("D:\\q"))); // 6  second root
        b.push(&row(false, 1, &w("c"))); // 7  parent 6
        assert_eq!(b.parent, [NONE, 0, 0, 2, 0, 4, NONE, 6]);
        let mut orphan = Builder::new();
        orphan.push(&row(false, 1, &w("x")));
        assert!(orphan.bad.is_some());
        let mut jump = Builder::new();
        jump.push(&row(true, 0, &w("C:\\r")));
        jump.push(&row(true, 2, &w("deep")));
        assert!(jump.bad.is_some());
    }
}
