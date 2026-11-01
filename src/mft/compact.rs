//! `mft` 的緊湊路徑：即時 `overview`／`report` 不需要把每個檔案都放進記憶體。
//!
//! 原本的路徑先把全部記錄的名稱與欄位存起來，再建出整棵 `ScanData`（整顆 `C:\` 峰值約 1.2 GB）。
//! 這條路徑只存「解析記錄時就能決定、之後不會改變」以外的少數東西：
//!
//! * **一般檔案**（單一名稱、沒有延伸記錄、重解析資料常駐）：解析到就直接計入父目錄的合計與接收端的彙總，
//!   只留 6 位元組（父記錄編號與序號）。父目錄是否有效、是否被走訪到要等整張表讀完才知道，
//!   所以最後掃一遍這個欄位，把「父目錄無效或沒被走訪到」的檔案**重讀記錄並退出**彙總（孤兒很少）。
//! * **目錄**：記錄欄位與名稱（組路徑與走訪都需要）。
//! * **複雜檔案**（硬連結、多個名稱、有延伸記錄、非常駐重解析資料）：完整保留，最後依標準順序決定主要名稱。
//! * **前 N 名候選**：檔案在不知道路徑時只用鍵值門檻判斷「可能進入前 N 名」，把這少數檔案的名稱留下，
//!   最後才組路徑、交給接收端。
//!
//! 任何無法保證與原本路徑逐位元組相同的情況（壞記錄、`--exclude`、孤兒是前 N 名候選、孤兒太多……）
//! 都回傳 `CompactError::Fallback`，呼叫端改走原本的路徑。

use super::*;
use crate::fxhash::{FxHashMap, FxHashSet};
use crate::report::{DirView, FileView, Tallier, Tally};
use crate::snapshot::Coverage;
use std::cmp::Ordering;

const SEP: u16 = b'\\' as u16;
/// 孤兒（父目錄無效或沒被走訪到的檔案）超過這個數量就放棄緊湊路徑（每個孤兒要重讀一筆記錄）。
const MAX_UNDO: usize = 100_000;
/// 讀取區塊大小：2 MiB、佇列 3 個。1 MiB × 4 個實測省了記憶體卻慢了約 13%（系統呼叫變多），折衷。
const COMPACT_CHUNK_BYTES: u64 = 2 * 1024 * 1024;

pub struct Outcome {
    pub totals: Totals,
    pub coverage: Coverage,
    pub cancelled: u64,
    pub dirs: u64,
    pub files: u64,
    pub stats: Stats,
}

pub enum CompactError {
    /// 緊湊路徑不能保證結果相同：呼叫端應該（先 `reset` 接收端）改走原本的路徑。
    Fallback(String),
    #[allow(dead_code)] // 呼叫端交給原本的路徑重新回報；差異測試會讀它
    Failed(MftError),
}

impl From<MftError> for CompactError {
    fn from(e: MftError) -> Self {
        CompactError::Failed(e)
    }
}

struct PAcc {
    files: u64,
    logical: u64,
    physical: u64,
    latest: u64,
}

struct Cand {
    number: u32,
    parent: u32,
    name: Box<[u16]>,
    logical: u64,
    physical: u64,
    mtime: u64,
}

/// 一般檔案解析出來的值（解析時與重讀退出時共用，確保兩邊一致）。
struct Simple {
    parent: u32,
    parent_seq: u16,
    name: Vec<u16>,
    logical: u64,
    physical: u64,
    mtime: u64,
}

fn lower_ext_into(name: &[u16], out: &mut Vec<u16>) {
    out.clear();
    let Some(i) = name.iter().rposition(|&u| u == u16::from(b'.')) else {
        return;
    };
    let tail = &name[i..];
    if tail.iter().all(|&u| u < 128) {
        out.extend(tail.iter().map(|&u| {
            if (0x41..=0x5A).contains(&u) {
                u + 32
            } else {
                u
            }
        }));
    } else {
        out.extend_from_slice(&crate::names::ext_of(name));
    }
}

fn physical_of(rec: &Record, cluster: u64) -> u64 {
    let mut physical = rec
        .data_clusters
        .saturating_add(rec.wof_clusters)
        .saturating_mul(cluster);
    if rec.data_resident {
        physical = physical.saturating_add(rec.data_logical.div_ceil(8) * 8);
    }
    physical
}

fn node_of(number: u32, rec: &Record, cluster: u64) -> Node {
    let mut flags = 0;
    if rec.is_dir {
        flags |= NODE_DIR;
    }
    if rec.has_reparse {
        flags |= NODE_REPARSE;
    }
    if rec.reparse_volume {
        flags |= NODE_VOLUME_MOUNT;
    }
    if rec.data_first {
        flags |= NODE_SIZE_SET;
    }
    Node {
        number,
        seq: rec.seq,
        links: rec.links,
        flags,
        attrs: rec.si_attrs,
        tag: rec.reparse_tag,
        mtime: rec.mtime,
        logical: if rec.data_first { rec.data_logical } else { 0 },
        physical: physical_of(rec, cluster),
    }
}

/// 這筆基底記錄若是「一般檔案」，回傳它的值；否則（複雜或沒有有效名稱）回傳 `None`。
/// `Ok(None)` 以外的第二個旗標表示它是否被略過（符號連結檔案、空名稱）。
fn simple_of(
    number: u32,
    buf: &[u8],
    rec: &Record,
    total: u32,
    cluster: u64,
    ext_bases: &FxHashSet<u32>,
) -> SimpleKind {
    if rec.is_dir || rec.has_attr_list || rec.reparse_lcn != 0 || ext_bases.contains(&number) {
        return SimpleKind::Complex;
    }
    let non_dos = rec
        .names
        .iter()
        .filter(|n| n.namespace != ntfs::NS_DOS)
        .count();
    let effective: Vec<&ntfs::NameRef> = if non_dos > 0 {
        rec.names
            .iter()
            .filter(|n| n.namespace != ntfs::NS_DOS)
            .collect()
    } else {
        rec.names.iter().collect()
    };
    match effective.len() {
        0 => return SimpleKind::Nothing,
        1 => {}
        _ => return SimpleKind::Complex,
    }
    let n = effective[0];
    // 名稱本身無效（父編號超出範圍、指向自己）：原本的路徑不會把它放進任何目錄
    if n.parent >= u64::from(total) || n.parent == u64::from(number) {
        return SimpleKind::Nothing;
    }
    let name: Vec<u16> = ntfs::name_units(buf, *n).collect();
    let skipped = SimpleKind::Skipped(n.parent as u32, n.parent_seq);
    if name.is_empty() {
        return skipped;
    }
    let node = node_of(number, rec, cluster);
    let attrs = normalize_attrs(&node);
    if attrs & ATTR_REPARSE != 0 && node.tag == TAG_SYMLINK {
        return skipped; // 檔案符號連結預設省略
    }
    SimpleKind::Simple(Simple {
        parent: n.parent as u32,
        parent_seq: n.parent_seq,
        name,
        logical: node.logical,
        physical: node.physical,
        mtime: rec.mtime,
    })
}

enum SimpleKind {
    Simple(Simple),
    Complex,
    /// 沒有任何會被放進目錄的名稱
    Nothing,
    /// 名稱有效但這個檔案被略過（符號連結、空名稱）；只為了和原本路徑一樣統計孤兒數
    Skipped(u32, u16),
}

/// 名稱的位元組池：每個字元都在 U+0000..U+00FF 的名稱存成 1 位元組，其餘存成 2 位元組（小端序），
/// `NameEnt::ns` 的最高位元 `WIDE` 標記哪一種。真實磁碟上絕大多數名稱是 ASCII，記憶體省一半。
#[derive(Default)]
struct Arena(Vec<u8>);

const WIDE: u8 = 0x80;

impl Arena {
    fn len(&self) -> usize {
        self.0.len()
    }

    fn capacity(&self) -> usize {
        self.0.capacity()
    }

    fn shrink_to_fit(&mut self) {
        self.0.shrink_to_fit();
    }

    /// 存入名稱，回傳（位移, `WIDE` 或 0）。
    fn push(&mut self, units: &[u16]) -> (u32, u8) {
        let off = self.0.len() as u32;
        if units.iter().all(|&u| u < 0x100) {
            self.0.extend(units.iter().map(|&u| u as u8));
            (off, 0)
        } else {
            for &u in units {
                self.0.extend_from_slice(&u.to_le_bytes());
            }
            (off, WIDE)
        }
    }

    /// 把名稱接在 `out` 後面。
    fn read_into(&self, ne: &NameEnt, out: &mut Vec<u16>) {
        let (o, l) = (ne.off as usize, usize::from(ne.len));
        if ne.ns & WIDE == 0 {
            out.extend(self.0[o..o + l].iter().map(|&b| u16::from(b)));
        } else {
            out.extend(
                self.0[o..o + 2 * l]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]])),
            );
        }
    }

    fn get(&self, ne: &NameEnt) -> Vec<u16> {
        let mut v = Vec::with_capacity(usize::from(ne.len));
        self.read_into(ne, &mut v);
        v
    }

    fn same(&self, ne: &NameEnt, units: &[u16]) -> bool {
        if usize::from(ne.len) != units.len() {
            return false;
        }
        let o = ne.off as usize;
        if ne.ns & WIDE == 0 {
            self.0[o..o + units.len()]
                .iter()
                .zip(units)
                .all(|(&b, &u)| u16::from(b) == u)
        } else {
            self.0[o..o + 2 * units.len()]
                .chunks_exact(2)
                .zip(units)
                .all(|(c, &u)| u16::from_le_bytes([c[0], c[1]]) == u)
        }
    }
}

struct CAcc {
    total: u32,
    cluster: u64,
    dirs: Vec<Node>,
    dir_by_num: Vec<u32>,
    dnames: Vec<NameEnt>,
    darena: Arena,
    cfiles: Vec<Node>,
    cby_num: FxHashMap<u32, u32>,
    cnames: Vec<NameEnt>,
    carena: Arena,
    ext_names: Vec<NameEnt>,
    ext_arena: Arena,
    exts: Vec<Ext>,
    ext_bases: FxHashSet<u32>,
    fparent: Vec<u32>,
    fseq: Vec<u16>,
    /// 一般檔案的父目錄累加器，鍵是（父記錄編號, 父序號）：序號不符的孤兒累加到不會被讀取的鍵，
    /// 不需要事後扣除（修改時間的最大值也無法扣除）
    pacc: FxHashMap<u64, PAcc>,
    cands: Vec<Cand>,
    pending_reparse: Vec<(u32, u64)>,
    bad: Vec<u64>,
    records: u64,
    in_use: u64,
    names_total: u64,
    self_names: u64,
    violated: bool,
    ext_scratch: Vec<u16>,
    skipped: Vec<(u32, u16)>,
}

fn push_names(
    names: &mut Vec<NameEnt>,
    arena: &mut Arena,
    owner: u32,
    total: u32,
    buf: &[u8],
    rec: &Record,
) {
    // 有長名時，同一筆記錄的 DOS 短名一定無效（原本的路徑也是這樣判斷），所以不必存。
    // 有屬性清單時長名可能在延伸記錄裡，保守地全部留下。
    let skip_dos = !rec.has_attr_list && rec.names.iter().any(|n| n.namespace != ntfs::NS_DOS);
    let first = names.len();
    let mut units = [0u16; 256];
    for n in &rec.names {
        if skip_dos && n.namespace == ntfs::NS_DOS {
            continue;
        }
        let mut l = 0usize;
        for u in ntfs::name_units(buf, *n).take(units.len()) {
            units[l] = u;
            l += 1;
        }
        let units = &units[..l];
        // 同一筆記錄的其他名稱若完全相同（硬連結常見），共用同一份字元
        let (off, wide) = match names[first..]
            .iter()
            .find(|e| e.len > 0 && arena.same(e, units))
        {
            Some(e) => (e.off, e.ns & WIDE),
            None => arena.push(units),
        };
        names.push(NameEnt {
            owner,
            parent: if n.parent < u64::from(total) {
                n.parent as u32
            } else {
                u32::MAX
            },
            parent_seq: n.parent_seq,
            ns: n.namespace | wide,
            len: n.units,
            off,
        });
    }
}

impl CAcc {
    fn new(total: u64, cluster: u64) -> CAcc {
        let t = total as usize;
        CAcc {
            total: total as u32,
            cluster,
            dirs: Vec::new(),
            dir_by_num: vec![u32::MAX; t],
            dnames: Vec::new(),
            darena: Arena::default(),
            cfiles: Vec::new(),
            cby_num: FxHashMap::default(),
            cnames: Vec::new(),
            carena: Arena::default(),
            ext_names: Vec::new(),
            ext_arena: Arena::default(),
            exts: Vec::new(),
            ext_bases: FxHashSet::default(),
            fparent: vec![u32::MAX; t],
            fseq: vec![0; t],
            pacc: FxHashMap::default(),
            cands: Vec::new(),
            pending_reparse: Vec::new(),
            bad: Vec::new(),
            records: 0,
            in_use: 0,
            names_total: 0,
            self_names: 0,
            violated: false,
            ext_scratch: Vec::new(),
            skipped: Vec::new(),
        }
    }

    fn process(
        &mut self,
        first: u64,
        data: &mut [u8],
        rb: usize,
        rec: &mut Record,
        sink: &mut dyn Tallier,
    ) {
        for (k, buf) in data.chunks_exact_mut(rb).enumerate() {
            let number = first + k as u64;
            self.records += 1;
            if buf[0..4] == [0, 0, 0, 0] {
                continue;
            }
            match ntfs::parse_record(buf, false, rec) {
                Ok(()) => self.handle(number, buf, rec, sink),
                Err(_) => self.bad.push(number),
            }
        }
    }

    fn handle(&mut self, number: u64, buf: &[u8], rec: &Record, sink: &mut dyn Tallier) {
        if !rec.in_use {
            return;
        }
        self.in_use += 1;
        let is_base = !rec.is_ext;
        let owner = if is_base { number } else { rec.base };
        if owner >= u64::from(self.total) || self.darena.len() > u32::MAX as usize - 1024 {
            return;
        }
        let owner = owner as u32;
        self.names_total += rec.names.len() as u64;
        self.self_names += rec
            .names
            .iter()
            .filter(|n| n.parent == u64::from(owner))
            .count() as u64;
        if rec.reparse_lcn != 0 {
            self.pending_reparse.push((owner, rec.reparse_lcn));
        }
        if !is_base {
            if self.fparent[owner as usize] != u32::MAX {
                // 基底記錄已被當成一般檔案計入，卻出現了它的延伸記錄：交給原本的路徑
                self.violated = true;
            }
            self.ext_bases.insert(owner);
            push_names(
                &mut self.ext_names,
                &mut self.ext_arena,
                owner,
                self.total,
                buf,
                rec,
            );
            self.exts.push(Ext {
                base: owner,
                physical: physical_of(rec, self.cluster),
                logical: rec.data_logical,
                first: rec.data_first,
                has_reparse: rec.has_reparse,
                reparse_tag: rec.reparse_tag,
                reparse_volume: rec.reparse_volume,
            });
            return;
        }
        let node = node_of(owner, rec, self.cluster);
        if rec.is_dir {
            self.dir_by_num[owner as usize] = self.dirs.len() as u32;
            self.dirs.push(node);
            push_names(
                &mut self.dnames,
                &mut self.darena,
                owner,
                self.total,
                buf,
                rec,
            );
            return;
        }
        match simple_of(owner, buf, rec, self.total, self.cluster, &self.ext_bases) {
            SimpleKind::Nothing => {}
            SimpleKind::Skipped(p, seq) => self.skipped.push((p, seq)),
            SimpleKind::Complex => {
                if rec.names.is_empty() && !rec.has_attr_list && !self.ext_bases.contains(&owner) {
                    return;
                }
                self.cby_num.insert(owner, self.cfiles.len() as u32);
                self.cfiles.push(node);
                push_names(
                    &mut self.cnames,
                    &mut self.carena,
                    owner,
                    self.total,
                    buf,
                    rec,
                );
            }
            SimpleKind::Simple(s) => self.fold(owner, s, sink),
        }
    }

    /// 一般檔案：計入父目錄的合計與接收端的彙總；可能進入前 N 名的留下名稱。
    fn fold(&mut self, number: u32, s: Simple, sink: &mut dyn Tallier) {
        lower_ext_into(&s.name, &mut self.ext_scratch);
        let t = Tally {
            ext: &self.ext_scratch,
            logical: s.logical,
            physical: s.physical,
            mtime: s.mtime,
            secondary: false,
        };
        sink.tally(&t, true);
        if sink.admit(&t) {
            self.cands.push(Cand {
                number,
                parent: s.parent,
                name: s.name.clone().into_boxed_slice(),
                logical: s.logical,
                physical: s.physical,
                mtime: s.mtime,
            });
        }
        let p = self
            .pacc
            .entry((u64::from(s.parent) << 16) | u64::from(s.parent_seq))
            .or_insert(PAcc {
                files: 0,
                logical: 0,
                physical: 0,
                latest: 0,
            });
        p.files += 1;
        p.logical += s.logical;
        p.physical += s.physical;
        p.latest = p.latest.max(s.mtime);
        self.fparent[number as usize] = s.parent;
        self.fseq[number as usize] = s.parent_seq;
    }
}

/// 以標準順序（逐段不分大小寫比較，一個目錄內檔案與子目錄依名稱混排的前序）比較兩個名稱項目。
fn cmp_canonical(a: &[&[u16]], b: &[&[u16]]) -> Ordering {
    for (x, y) in a.iter().zip(b.iter()) {
        let o = cmp_names(x, y);
        if o != Ordering::Equal {
            return o;
        }
    }
    a.len().cmp(&b.len())
}

/// 掃描一個 NTFS 磁碟區根目錄（`display` 例如 `C:\`），結果直接交給 `sink`。
pub fn scan_volume_compact(
    display: &[u16],
    opts: &Options,
    sink: &mut dyn Tallier,
) -> Result<Outcome, CompactError> {
    let ext = walk::to_extended(display);
    let fi = sys::file_info(&ext).map_err(|e| {
        MftError::Failed(format!(
            "cannot read the volume root: {}",
            sys::error_message(e)
        ))
    })?;
    let vol = sys::VolumeReader::open(display).map_err(|e| {
        if e == 5 {
            MftError::Unavailable(
                "administrator rights are required to read the master file table".into(),
            )
        } else {
            MftError::Unavailable(format!("cannot open the volume: {}", sys::error_message(e)))
        }
    })?;
    let root = RootMeta {
        attrs: fi.attrs,
        mtime: fi.mtime,
        serial: fi.serial,
        index: u128::from(fi.index),
    };
    scan_source_compact(&vol, display, root, opts, sink)
}

/// 核心：讀取 `src` 上的 NTFS，把結果直接交給 `sink`（不建出 `ScanData`）。
pub fn scan_source_compact(
    src: &dyn Source,
    display: &[u16],
    root: RootMeta,
    opts: &Options,
    sink: &mut dyn Tallier,
) -> Result<Outcome, CompactError> {
    if !opts.exclude.is_empty() {
        return Err(CompactError::Fallback(
            "--exclude needs the full path of every entry".into(),
        ));
    }
    let mut head = vec![0u8; 4096];
    let n = src.read_at(0, &mut head).map_err(io_err)?;
    let boot = ntfs::parse_boot(&head[..n])
        .map_err(|_| MftError::Unavailable("the volume is not NTFS".into()))?;
    let lay = bootstrap(src, &boot)?;
    let rb = lay.record;

    let (tx, rx) = sync_channel::<(u64, Vec<u8>)>(3);
    let lay_ref = &lay;
    let (mut acc, read_result) = std::thread::scope(|s| {
        let h = s.spawn(move || reader(src, lay_ref, opts, COMPACT_CHUNK_BYTES, &tx));
        let mut acc = CAcc::new(lay_ref.total_records, lay_ref.cluster);
        let mut rec = Record::default();
        for (first, mut data) in rx {
            acc.process(first, &mut data, rb, &mut rec, sink);
        }
        (acc, h.join())
    });
    let cancelled = match read_result {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => {
            return Err(MftError::Failed("the reader thread panicked".into()).into());
        }
    };
    let mut stats = Stats::default();

    if cancelled {
        sink.reset();
        let mut cov = Coverage::default();
        cov.note(ScanState::Cancelled, None, display);
        sink.dir(&DirView {
            path: display,
            depth: 0,
            rollup: Totals::default(),
            direct: Totals::default(),
            latest_mtime: root.mtime,
        });
        sink.finish_files();
        return Ok(Outcome {
            totals: Totals {
                dirs: 1,
                ..Default::default()
            },
            coverage: cov,
            cancelled: 1,
            dirs: 1,
            files: 0,
            stats,
        });
    }

    // 重讀損壞的記錄一次；仍然讀不到就交給原本的路徑（它會把根標為 partial）
    let bad = std::mem::take(&mut acc.bad);
    let mut rec = Record::default();
    for number in bad {
        let byte = number * rb as u64;
        let mut buf = vec![0u8; rb];
        let ok = ntfs::map_offset(&lay.runs, lay.cluster, byte)
            .filter(|&(_, remain)| remain >= rb as u64)
            .and_then(|(voff, _)| read_exact(src, voff, &mut buf).ok())
            .is_some()
            && ntfs::parse_record(&mut buf, false, &mut rec).is_ok();
        if ok {
            acc.handle(number, &buf, &rec, sink);
        } else {
            return Err(CompactError::Fallback(
                "a master file table record could not be read".into(),
            ));
        }
    }
    if acc.violated {
        return Err(CompactError::Fallback(
            "an extension record belongs to a record already counted".into(),
        ));
    }
    stats.records = acc.records;
    stats.in_use = acc.in_use;

    // 非常駐的重解析資料：只讀開頭取得標籤
    let mut rhead = [0u8; 512];
    for (owner, lcn) in std::mem::take(&mut acc.pending_reparse) {
        let di = acc.dir_by_num[owner as usize];
        let node: &mut Node = if di != u32::MAX {
            &mut acc.dirs[di as usize]
        } else if let Some(&ci) = acc.cby_num.get(&owner) {
            &mut acc.cfiles[ci as usize]
        } else {
            continue;
        };
        if node.tag != 0 {
            continue;
        }
        if read_exact(src, lcn.saturating_mul(lay.cluster), &mut rhead).is_ok() {
            let (tag, volume) = ntfs::reparse_info(&rhead);
            node.tag = tag;
            if volume {
                node.flags |= NODE_VOLUME_MOUNT;
            }
        }
    }

    let reread = |number: u32| -> Option<Vec<u8>> {
        let byte = u64::from(number) * rb as u64;
        let mut buf = vec![0u8; rb];
        ntfs::map_offset(&lay.runs, lay.cluster, byte)
            .filter(|&(_, remain)| remain >= rb as u64)
            .and_then(|(voff, _)| read_exact(src, voff, &mut buf).ok())?;
        Some(buf)
    };

    finish(acc, display, root, stats, sink, &reread)
}

#[allow(clippy::too_many_lines)]
fn finish(
    mut acc: CAcc,
    display: &[u16],
    root: RootMeta,
    mut stats: Stats,
    sink: &mut dyn Tallier,
    reread: &dyn Fn(u32) -> Option<Vec<u8>>,
) -> Result<Outcome, CompactError> {
    acc.dirs.shrink_to_fit();
    acc.dnames.shrink_to_fit();
    acc.darena.shrink_to_fit();
    acc.cfiles.shrink_to_fit();
    acc.cnames.shrink_to_fit();
    acc.carena.shrink_to_fit();
    profile("after parse", &acc);
    // 延伸記錄的資料與名稱併入擁有者（目錄或複雜檔案）
    for e in std::mem::take(&mut acc.exts) {
        let di = acc.dir_by_num[e.base as usize];
        let n: &mut Node = if di != u32::MAX {
            &mut acc.dirs[di as usize]
        } else if let Some(&ci) = acc.cby_num.get(&e.base) {
            &mut acc.cfiles[ci as usize]
        } else {
            continue;
        };
        n.physical = n.physical.saturating_add(e.physical);
        if e.first && n.flags & NODE_SIZE_SET == 0 {
            n.logical = e.logical;
            n.flags |= NODE_SIZE_SET;
        }
        if e.has_reparse {
            n.flags |= NODE_REPARSE;
            if n.tag == 0 {
                n.tag = e.reparse_tag;
                if e.reparse_volume {
                    n.flags |= NODE_VOLUME_MOUNT;
                }
            }
        }
    }
    for ne in std::mem::take(&mut acc.ext_names) {
        let units = acc.ext_arena.get(&ne);
        if acc.dir_by_num[ne.owner as usize] != u32::MAX {
            let (off, wide) = acc.darena.push(&units);
            acc.dnames.push(NameEnt {
                off,
                ns: (ne.ns & !WIDE) | wide,
                ..ne
            });
        } else if acc.cby_num.contains_key(&ne.owner) {
            let (off, wide) = acc.carena.push(&units);
            acc.cnames.push(NameEnt {
                off,
                ns: (ne.ns & !WIDE) | wide,
                ..ne
            });
        }
    }
    acc.ext_arena = Arena::default();

    // 有「非 DOS」名稱的節點（DOS 短名只在沒有長名時才算數）
    for ne in &acc.dnames {
        if ne.ns & !WIDE != ntfs::NS_DOS {
            let di = acc.dir_by_num[ne.owner as usize];
            acc.dirs[di as usize].flags |= NODE_HAS_LONG_NAME;
        }
    }
    for ne in &acc.cnames {
        if ne.ns & !WIDE != ntfs::NS_DOS {
            let ci = acc.cby_num[&ne.owner];
            acc.cfiles[ci as usize].flags |= NODE_HAS_LONG_NAME;
        }
    }

    let ndirs = acc.dirs.len();
    // 名稱有效嗎：不是指向自己、DOS 短名只在沒有長名時有效、父目錄是同序號的目錄。回傳父目錄索引。
    let valid_dir = |acc: &CAcc, ne: &NameEnt, owner_flags: u8| -> Option<u32> {
        if ne.parent == u32::MAX || ne.owner == ne.parent {
            return None;
        }
        if ne.ns & !WIDE == ntfs::NS_DOS && owner_flags & NODE_HAS_LONG_NAME != 0 {
            return None;
        }
        let pi = acc.dir_by_num[ne.parent as usize];
        if pi == u32::MAX || acc.dirs[pi as usize].seq != ne.parent_seq {
            return None;
        }
        Some(pi)
    };

    let root_idx = acc.dir_by_num[ntfs::ROOT_RECORD as usize];
    if root_idx == u32::MAX {
        return Err(MftError::Failed("the root directory record was not found".into()).into());
    }

    // 子目錄清單（計數排序：依父目錄索引分組）
    let mut dstart = vec![0u32; ndirs + 1];
    let mut valid_names = 0u64;
    for ne in &acc.dnames {
        let oi = acc.dir_by_num[ne.owner as usize];
        if let Some(pi) = valid_dir(&acc, ne, acc.dirs[oi as usize].flags) {
            dstart[pi as usize + 1] += 1;
            valid_names += 1;
        }
    }
    for i in 0..ndirs {
        dstart[i + 1] += dstart[i];
    }
    let mut dlinks = vec![0u32; dstart[ndirs] as usize];
    {
        let mut fill = dstart.clone();
        for (i, ne) in acc.dnames.iter().enumerate() {
            let oi = acc.dir_by_num[ne.owner as usize];
            if let Some(pi) = valid_dir(&acc, ne, acc.dirs[oi as usize].flags) {
                dlinks[fill[pi as usize] as usize] = i as u32;
                fill[pi as usize] += 1;
            }
        }
    }

    // 走訪：哪些目錄存在於樹中、哪些被進入
    let mut entered = vec![false; ndirs];
    let mut visited_flag = vec![false; ndirs];
    let mut visited: Vec<u32> = vec![root_idx];
    let mut tparent = vec![u32::MAX; ndirs];
    let mut tname = vec![u32::MAX; ndirs];
    let mut depth = vec![0u32; ndirs];
    visited_flag[root_idx as usize] = true;
    entered[root_idx as usize] = true;
    let mut stack = vec![root_idx];
    let mut reached_names = 0u64;
    while let Some(d) = stack.pop() {
        for &ni in &dlinks[dstart[d as usize] as usize..dstart[d as usize + 1] as usize] {
            reached_names += 1;
            let ne = acc.dnames[ni as usize];
            if ne.len == 0 {
                continue;
            }
            let ci = acc.dir_by_num[ne.owner as usize];
            if visited_flag[ci as usize] {
                return Err(CompactError::Fallback(
                    "a directory has more than one valid name".into(),
                ));
            }
            let child = &acc.dirs[ci as usize];
            let attrs = normalize_attrs(child);
            let reparse = classify(child, attrs);
            let hidden_system_reparse = attrs & (ATTR_HIDDEN | ATTR_SYSTEM | ATTR_REPARSE)
                == (ATTR_HIDDEN | ATTR_SYSTEM | ATTR_REPARSE);
            visited_flag[ci as usize] = true;
            visited.push(ci);
            tparent[ci as usize] = d;
            tname[ci as usize] = ni;
            depth[ci as usize] = depth[d as usize] + 1;
            if !(hidden_system_reparse
                || matches!(
                    reparse,
                    Reparse::Symlink | Reparse::Junction | Reparse::Mount
                ))
            {
                entered[ci as usize] = true;
                stack.push(ci);
            }
        }
    }

    // 走訪用的暫存陣列用完了，先還給系統，降低後面算總計階段的峰值
    drop(dlinks);
    drop(dstart);
    drop(visited_flag);

    // 目錄的名稱與路徑
    fn comp_into(acc: &CAcc, tname: &[u32], d: u32, out: &mut Vec<u16>) {
        let ne = &acc.dnames[tname[d as usize] as usize];
        acc.darena.read_into(ne, out);
    }
    let comp = |acc: &CAcc, d: u32| -> Vec<u16> {
        let mut v = Vec::new();
        comp_into(acc, &tname, d, &mut v);
        v
    };
    let chain = |d: u32| -> Vec<u32> {
        let mut c = Vec::new();
        let mut cur = d;
        while cur != root_idx {
            c.push(cur);
            cur = tparent[cur as usize];
        }
        c.reverse();
        c
    };
    let path_of = |acc: &CAcc, d: u32, tail: Option<&[u16]>, out: &mut Vec<u16>| {
        out.clear();
        out.extend_from_slice(display);
        for c in chain(d) {
            if out.last() != Some(&SEP) {
                out.push(SEP);
            }
            comp_into(acc, &tname, c, out);
        }
        if let Some(t) = tail {
            if out.last() != Some(&SEP) {
                out.push(SEP);
            }
            out.extend_from_slice(t);
        }
    };

    // 一般檔案：父目錄無效或沒被走訪到的要退出彙總（重讀記錄）
    let mut undo: Vec<u32> = Vec::new();
    let mut simple_valid = 0u64;
    let mut simple_reached = 0u64;
    for r in 0..acc.total as usize {
        let p = acc.fparent[r];
        if p == u32::MAX {
            continue;
        }
        let pi = acc.dir_by_num[p as usize];
        let valid = pi != u32::MAX && acc.dirs[pi as usize].seq == acc.fseq[r];
        if valid {
            simple_valid += 1;
        }
        if valid && entered[pi as usize] {
            simple_reached += 1;
        } else {
            undo.push(r as u32);
        }
    }
    for &(p, seq) in &acc.skipped {
        let pi = acc.dir_by_num[p as usize];
        if pi != u32::MAX && acc.dirs[pi as usize].seq == seq {
            simple_valid += 1;
            if entered[pi as usize] {
                simple_reached += 1;
            }
        }
    }
    // 以記錄編號為索引的欄位用完了：先釋放，再配置後面的陣列，降低峰值
    acc.fparent = Vec::new();
    acc.fseq = Vec::new();
    acc.skipped = Vec::new();
    if undo.len() > MAX_UNDO {
        return Err(CompactError::Fallback(format!(
            "{} names point at directories that are not part of the tree",
            undo.len()
        )));
    }
    if !undo.is_empty() {
        let mut rec = Record::default();
        let mut ext = Vec::new();
        let none = FxHashSet::default();
        for r in undo {
            let Some(mut buf) = reread(r) else {
                return Err(CompactError::Fallback(
                    "an unreachable record could not be re-read".into(),
                ));
            };
            if ntfs::parse_record(&mut buf, false, &mut rec).is_err() {
                return Err(CompactError::Fallback(
                    "an unreachable record could not be parsed again".into(),
                ));
            }
            match simple_of(r, &buf, &rec, acc.total, acc.cluster, &none) {
                SimpleKind::Simple(s) => {
                    lower_ext_into(&s.name, &mut ext);
                    let t = Tally {
                        ext: &ext,
                        logical: s.logical,
                        physical: s.physical,
                        mtime: s.mtime,
                        secondary: false,
                    };
                    sink.tally(&t, false);
                    // 孤兒若是前 N 名候選：拿掉它；如果它佔著前 N 名的位置而且之前有檔案被擋下，
                    // 結果就可能少了本來該進來的檔案，這時交給原本的路徑
                    if let Some(i) = acc.cands.iter().position(|c| c.number == r) {
                        if !sink.forget(&t) {
                            return Err(CompactError::Fallback(format!(
                                "a file outside the tree occupies a top-N slot (record {r})"
                            )));
                        }
                        acc.cands.swap_remove(i);
                    }
                }
                _ => {
                    return Err(CompactError::Fallback(
                        "a record changed while the volume was being read".into(),
                    ));
                }
            }
        }
    }

    profile("before totals", &acc);
    // 每個目錄的直接合計：一般檔案（父目錄累加器）＋複雜檔案
    let mut direct = vec![Totals::default(); ndirs];
    let mut latest: Vec<u64> = acc.dirs.iter().map(|n| n.mtime).collect();
    latest[root_idx as usize] = root.mtime;
    for &d in &visited {
        if !entered[d as usize] {
            continue;
        }
        let dn = &acc.dirs[d as usize];
        if let Some(p) = acc
            .pacc
            .get(&((u64::from(dn.number) << 16) | u64::from(dn.seq)))
        {
            let t = &mut direct[d as usize];
            t.files += p.files;
            t.logical += p.logical;
            t.physical += p.physical;
            latest[d as usize] = latest[d as usize].max(p.latest);
        }
    }

    acc.pacc = FxHashMap::default();

    // 複雜檔案：每個名稱各算一次；主要名稱是標準順序中第一個（其餘為硬連結次要名稱）
    let mut cstart = vec![0u32; acc.cfiles.len() + 1];
    for ne in &acc.cnames {
        cstart[acc.cby_num[&ne.owner] as usize + 1] += 1;
    }
    for i in 0..acc.cfiles.len() {
        cstart[i + 1] += cstart[i];
    }
    let mut clinks = vec![0u32; acc.cnames.len()];
    {
        let mut fill = cstart.clone();
        for (i, ne) in acc.cnames.iter().enumerate() {
            let ci = acc.cby_num[&ne.owner] as usize;
            clinks[fill[ci] as usize] = i as u32;
            fill[ci] += 1;
        }
    }
    let mut path = Vec::new();
    let mut ext = Vec::new();
    let mut cname: Vec<u16> = Vec::new();
    for ci in 0..acc.cfiles.len() {
        let node = acc.cfiles[ci];
        let attrs = normalize_attrs(&node);
        let skip = attrs & ATTR_REPARSE != 0 && node.tag == TAG_SYMLINK;
        // (父目錄索引, 名稱項目)——通過有效性、走訪與略過規則的名稱
        let mut placed: Vec<(u32, u32)> = Vec::new();
        for &ni in &clinks[cstart[ci] as usize..cstart[ci + 1] as usize] {
            let ne = acc.cnames[ni as usize];
            let Some(pi) = valid_dir(&acc, &ne, node.flags) else {
                continue;
            };
            valid_names += 1;
            if !entered[pi as usize] {
                continue;
            }
            reached_names += 1;
            if ne.len == 0 || skip {
                continue;
            }
            placed.push((pi, ni));
        }
        if placed.is_empty() {
            continue;
        }
        let comps = |acc: &CAcc, pi: u32, ni: u32| -> Vec<Vec<u16>> {
            let mut v: Vec<Vec<u16>> = chain(pi).iter().map(|&c| comp(acc, c)).collect();
            let ne = &acc.cnames[ni as usize];
            v.push(acc.carena.get(ne));
            v
        };
        let mut primary = 0usize;
        if placed.len() > 1 {
            let keys: Vec<Vec<Vec<u16>>> =
                placed.iter().map(|&(pi, ni)| comps(&acc, pi, ni)).collect();
            for i in 1..placed.len() {
                let (a, b): (Vec<&[u16]>, Vec<&[u16]>) = (
                    keys[i].iter().map(Vec::as_slice).collect(),
                    keys[primary].iter().map(Vec::as_slice).collect(),
                );
                if cmp_canonical(&a, &b) == Ordering::Less {
                    primary = i;
                }
            }
        }
        for (k, &(pi, ni)) in placed.iter().enumerate() {
            let ne = acc.cnames[ni as usize];
            cname.clear();
            acc.carena.read_into(&ne, &mut cname);
            let name = &cname[..];
            let secondary = k != primary;
            lower_ext_into(name, &mut ext);
            let t = Tally {
                ext: &ext,
                logical: node.logical,
                physical: node.physical,
                mtime: node.mtime,
                secondary,
            };
            sink.tally(&t, true);
            let d = &mut direct[pi as usize];
            d.files += 1;
            d.logical += node.logical;
            if !secondary {
                d.physical += node.physical;
            }
            latest[pi as usize] = latest[pi as usize].max(node.mtime);
            path_of(&acc, pi, Some(name), &mut path);
            sink.offer(&FileView {
                path: &path,
                ext: &ext,
                logical: node.logical,
                physical: node.physical,
                mtime: node.mtime,
                secondary,
            });
        }
    }

    // 複雜檔案的資料（名稱池、名稱、節點、對照表）用完了；滾算前先釋放
    drop(clinks);
    drop(cstart);
    acc.cnames = Vec::new();
    acc.carena = Arena::default();
    acc.cfiles = Vec::new();
    acc.cby_num = FxHashMap::default();
    profile("after complex", &acc);

    // 前 N 名候選的路徑
    for c in std::mem::take(&mut acc.cands) {
        let pi = acc.dir_by_num[c.parent as usize];
        path_of(&acc, pi, Some(&c.name), &mut path);
        lower_ext_into(&c.name, &mut ext);
        sink.offer(&FileView {
            path: &path,
            ext: &ext,
            logical: c.logical,
            physical: c.physical,
            mtime: c.mtime,
            secondary: false,
        });
    }
    sink.finish_files();

    // 由下往上滾算，並把每個目錄交給接收端
    let mut rollup = direct.clone();
    for &d in visited.iter().rev() {
        let p = tparent[d as usize];
        if p != u32::MAX {
            let r = rollup[d as usize];
            let l = latest[d as usize];
            let pr = &mut rollup[p as usize];
            pr.files += r.files;
            pr.dirs += 1 + r.dirs;
            pr.logical += r.logical;
            pr.physical += r.physical;
            latest[p as usize] = latest[p as usize].max(l);
        }
    }
    let mut cov = Coverage::default();
    for &d in &visited {
        path_of(&acc, d, None, &mut path);
        let state = if entered[d as usize] {
            ScanState::Complete
        } else {
            ScanState::NotFollowed
        };
        cov.note(state, None, &path);
        sink.dir(&DirView {
            path: &path,
            depth: u64::from(depth[d as usize]),
            rollup: rollup[d as usize],
            direct: direct[d as usize],
            latest_mtime: latest[d as usize],
        });
    }

    // 孤兒（名稱的父目錄不存在、已重用或不可達），計法與原本的路徑相同
    let valid_all = valid_names + simple_valid;
    let reached_all = reached_names + simple_reached;
    stats.orphan_names = valid_all.saturating_sub(reached_all)
        + acc
            .names_total
            .saturating_sub(valid_all)
            .saturating_sub(acc.self_names);
    let r = rollup[root_idx as usize];
    Ok(Outcome {
        totals: Totals {
            files: r.files,
            dirs: visited.len() as u64,
            logical: r.logical,
            physical: r.physical,
        },
        coverage: cov,
        cancelled: 0,
        dirs: visited.len() as u64,
        files: r.files,
        stats,
    })
}

/// `VOLTDIR_PROF` 設定時，在 stderr 印出各資料結構的容量（位元組），用來剖析記憶體。
fn profile(stage: &str, acc: &CAcc) {
    if std::env::var_os("VOLTDIR_PROF").is_none() {
        return;
    }
    let mb = |b: usize| b as f64 / (1024.0 * 1024.0);
    let parts: [(&str, usize); 11] = [
        ("dirs", acc.dirs.capacity() * std::mem::size_of::<Node>()),
        ("dir_by_num", acc.dir_by_num.capacity() * 4),
        (
            "dnames",
            acc.dnames.capacity() * std::mem::size_of::<NameEnt>(),
        ),
        ("darena", acc.darena.capacity()),
        (
            "cfiles",
            acc.cfiles.capacity() * std::mem::size_of::<Node>(),
        ),
        (
            "cnames",
            acc.cnames.capacity() * std::mem::size_of::<NameEnt>(),
        ),
        ("carena", acc.carena.capacity()),
        (
            "fparent+fseq",
            acc.fparent.capacity() * 4 + acc.fseq.capacity() * 2,
        ),
        (
            "pacc",
            acc.pacc.capacity() * (8 + std::mem::size_of::<PAcc>() + 1),
        ),
        ("cands", acc.cands.capacity() * std::mem::size_of::<Cand>()),
        (
            "exts+pending",
            acc.exts.capacity() * std::mem::size_of::<Ext>() + acc.pending_reparse.capacity() * 16,
        ),
    ];
    let total: usize = parts.iter().map(|p| p.1).sum();
    eprintln!("[mft profile] {stage}: {:.0} MB in structures", mb(total));
    for (n, b) in parts {
        eprintln!("[mft profile]   {n:<14} {:>7.1} MB", mb(b));
    }
}
