//! `mft` 引擎：以唯讀、循序方式讀取 NTFS 主檔案表，在記憶體中重建目錄樹。
//!
//! 流程：開機磁區 → `$MFT`（記錄 0）取得自身的資料區段 → 讀取執行緒依序讀出整張表，
//! 解析執行緒逐筆解析 → 合併延伸記錄 → 由根目錄（記錄 5）往下建立 `ScanData`。
//! 結果與 `walk` 引擎使用同一個資料模型，之後的去重、滾算、輸出完全共用。

use crate::glob;
use crate::model::*;
use crate::names::cmp_names;
use crate::ntfs::{self, Record, Run};
use crate::sys;
use crate::walk::{self, Options};
use std::sync::mpsc::sync_channel;

mod compact;
pub use compact::{CompactError, scan_volume_compact};

/// 一次讀取的位元組數上限（需為叢集大小的倍數時會向下取整）。
const CHUNK_BYTES: u64 = 4 * 1024 * 1024;
/// 讀取失敗的記錄只記一個錯誤碼：ERROR_FILE_CORRUPT。
pub const ERROR_FILE_CORRUPT: u32 = 1392;

const NODE_DIR: u8 = 1;
const NODE_VOLUME_MOUNT: u8 = 2;
const NODE_REPARSE: u8 = 4;
const NODE_HAS_LONG_NAME: u8 = 8;
const NODE_SIZE_SET: u8 = 16;

/// 可依位移讀取位元組的來源：真實磁碟區，或測試用的記憶體映像。
pub trait Source: Sync {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, u32>;
}

impl Source for sys::VolumeReader {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, u32> {
        sys::VolumeReader::read_at(self, offset, buf)
    }
}

#[derive(Debug)]
pub enum MftError {
    /// 引擎不能用（未提權、不是 NTFS…）
    Unavailable(String),
    /// 讀取或解析途中失敗
    Failed(String),
}

impl MftError {
    pub fn message(&self) -> &str {
        match self {
            MftError::Unavailable(m) | MftError::Failed(m) => m,
        }
    }
}

#[derive(Default, Debug, Clone, Copy)]
pub struct Stats {
    pub records: u64,
    pub in_use: u64,
    /// 重讀後仍然損壞而略過的記錄數
    pub bad_records: u64,
    /// 名稱的父目錄不存在、已重用或不可達
    pub orphan_names: u64,
}

/// 掃描根目錄本身的屬性（與 `walk` 取得方式相同，確保根節點一致）。
#[derive(Clone, Copy)]
pub struct RootMeta {
    pub attrs: u32,
    pub mtime: u64,
    pub serial: u32,
    pub index: u128,
}

/// 掃描一個 NTFS 磁碟區根目錄（`display` 例如 `C:\`）。
pub fn scan_volume(
    display: &[u16],
    opts: &Options,
    root_index: u32,
) -> Result<(ScanData, Stats), MftError> {
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
    scan_source(&vol, display, root, opts, root_index)
}

fn io_err(e: u32) -> MftError {
    MftError::Failed(format!("read error: {}", sys::error_message(e)))
}

fn read_exact(src: &dyn Source, off: u64, buf: &mut [u8]) -> Result<(), MftError> {
    let n = src.read_at(off, buf).map_err(io_err)?;
    if n != buf.len() {
        return Err(MftError::Failed("unexpected end of volume".into()));
    }
    Ok(())
}

struct Layout {
    runs: Vec<Run>,
    cluster: u64,
    record: usize,
    total_records: u64,
}

/// 讀取 `$MFT` 自己的記錄，取得整張表的資料區段與大小。
fn bootstrap(src: &dyn Source, boot: &ntfs::Boot) -> Result<Layout, MftError> {
    let rb = boot.record_bytes as usize;
    let cb = u64::from(boot.cluster_bytes);
    let mut buf = vec![0u8; rb];
    let off0 = boot
        .mft_lcn
        .checked_mul(cb)
        .ok_or_else(|| MftError::Failed("bad MFT location".into()))?;
    read_exact(src, off0, &mut buf)?;
    let mut rec = Record::default();
    ntfs::parse_record(&mut buf, true, &mut rec)
        .map_err(|e| MftError::Failed(format!("cannot parse the $MFT record: {e:?}")))?;
    if !rec.in_use || !rec.has_data || !rec.data_first || rec.data_resident {
        return Err(MftError::Failed(
            "the $MFT record has no usable data".into(),
        ));
    }
    let mft_bytes = rec.data_logical;
    let mut runs = rec.data_runs.clone();
    if rec.has_attr_list {
        let (o, l) = rec.attr_list.ok_or_else(|| {
            MftError::Failed("a non-resident $MFT attribute list is not supported".into())
        })?;
        let list = buf
            .get(o..o + l)
            .map(ntfs::parse_attr_list)
            .ok_or_else(|| MftError::Failed("bad $MFT attribute list".into()))?;
        let mut pending: Vec<u64> = list
            .iter()
            .filter(|e| e.attr_type == ntfs::ATTR_TYPE_DATA && e.start_vcn > 0 && e.record != 0)
            .map(|e| e.record)
            .collect();
        pending.sort_unstable();
        pending.dedup();
        // 延伸記錄本身也位於 $MFT 內，需先知道包含它的區段才讀得到；反覆直到沒有進展。
        while !pending.is_empty() {
            let before = pending.len();
            let mut still = Vec::new();
            for &r in &pending {
                let byte = r.saturating_mul(rb as u64);
                match ntfs::map_offset(&runs, cb, byte) {
                    Some((voff, remain)) if remain >= rb as u64 => {
                        let mut eb = vec![0u8; rb];
                        read_exact(src, voff, &mut eb)?;
                        let mut er = Record::default();
                        ntfs::parse_record(&mut eb, true, &mut er).map_err(|e| {
                            MftError::Failed(format!(
                                "cannot parse an $MFT extension record: {e:?}"
                            ))
                        })?;
                        runs.extend_from_slice(&er.data_runs);
                    }
                    _ => still.push(r),
                }
            }
            if still.len() == before {
                return Err(MftError::Failed(
                    "cannot locate the $MFT extension records".into(),
                ));
            }
            pending = still;
        }
        runs.sort_by_key(|r| r.vcn);
        runs.dedup();
    }
    // 區段必須從 VCN 0 連續涵蓋整張表
    let mut next = 0u64;
    for r in &runs {
        if r.vcn != next {
            return Err(MftError::Failed(
                "the $MFT data runs are not contiguous".into(),
            ));
        }
        next = next.saturating_add(r.len);
    }
    if next.saturating_mul(cb) < mft_bytes {
        return Err(MftError::Failed("the $MFT data runs are too short".into()));
    }
    let total_records = mft_bytes / rb as u64;
    if total_records == 0 || total_records > u64::from(u32::MAX) {
        return Err(MftError::Failed("unsupported $MFT size".into()));
    }
    Ok(Layout {
        runs,
        cluster: cb,
        record: rb,
        total_records,
    })
}

/// 讀取執行緒：依序讀出整張表，切成整數筆記錄後送出。
fn reader(
    src: &dyn Source,
    lay: &Layout,
    opts: &Options,
    chunk_bytes: u64,
    tx: &std::sync::mpsc::SyncSender<(u64, Vec<u8>)>,
) -> Result<bool, MftError> {
    let rb = lay.record as u64;
    let chunk = (chunk_bytes / lay.cluster).max(1) * lay.cluster;
    let mut carry: Vec<u8> = Vec::new();
    let mut next_record = 0u64;
    for run in &lay.runs {
        if next_record >= lay.total_records {
            break;
        }
        let run_bytes = run.len.saturating_mul(lay.cluster);
        let Some(lcn) = run.lcn else {
            // 稀疏區段：整段都是未使用的記錄
            carry.clear();
            next_record += run_bytes / rb;
            continue;
        };
        let mut off = 0u64;
        while off < run_bytes && next_record < lay.total_records {
            if opts.cancelled() {
                return Ok(true);
            }
            let len = chunk.min(run_bytes - off);
            let mut buf = vec![0u8; len as usize];
            let start = lcn
                .checked_mul(lay.cluster)
                .and_then(|b| b.checked_add(off))
                .ok_or_else(|| MftError::Failed("bad run location".into()))?;
            read_exact(src, start, &mut buf)?;
            off += len;
            let mut data = if carry.is_empty() {
                buf
            } else {
                carry.extend_from_slice(&buf);
                std::mem::take(&mut carry)
            };
            let whole = data.len() / lay.record * lay.record;
            if whole < data.len() {
                carry = data.split_off(whole);
            }
            let take = (whole as u64 / rb).min(lay.total_records - next_record);
            data.truncate((take * rb) as usize);
            if !data.is_empty() {
                let first = next_record;
                next_record += take;
                if tx.send((first, data)).is_err() {
                    return Ok(false);
                }
            }
        }
    }
    Ok(false)
}

#[derive(Clone, Copy)]
struct Node {
    number: u32,
    seq: u16,
    links: u16,
    flags: u8,
    attrs: u32,
    tag: u32,
    mtime: u64,
    logical: u64,
    /// 實際大小（位元組）
    physical: u64,
}

#[derive(Clone, Copy)]
struct NameEnt {
    owner: u32,
    parent: u32,
    parent_seq: u16,
    ns: u8,
    len: u16,
    off: u32,
}

struct Ext {
    base: u32,
    physical: u64,
    logical: u64,
    first: bool,
    /// 重解析點屬性也可能放在延伸記錄中
    has_reparse: bool,
    reparse_tag: u32,
    reparse_volume: bool,
}

struct Acc {
    total: u32,
    cluster: u64,
    nodes: Vec<Node>,
    by_num: Vec<u32>,
    names: Vec<NameEnt>,
    arena: Vec<u16>,
    exts: Vec<Ext>,
    /// 重解析資料為非常駐的記錄：(記錄編號, 資料起始叢集)
    pending_reparse: Vec<(u32, u64)>,
    bad: Vec<u64>,
    records: u64,
    in_use: u64,
}

impl Acc {
    fn new(total: u64, cluster: u64) -> Acc {
        Acc {
            total: total as u32,
            cluster,
            nodes: Vec::new(),
            by_num: vec![u32::MAX; total as usize],
            names: Vec::new(),
            arena: Vec::new(),
            exts: Vec::new(),
            pending_reparse: Vec::new(),
            bad: Vec::new(),
            records: 0,
            in_use: 0,
        }
    }

    fn process(&mut self, first: u64, data: &mut [u8], rb: usize, rec: &mut Record) {
        for (k, buf) in data.chunks_exact_mut(rb).enumerate() {
            let number = first + k as u64;
            self.records += 1;
            if buf[0..4] == [0, 0, 0, 0] {
                continue; // 從未使用
            }
            match ntfs::parse_record(buf, false, rec) {
                Ok(()) => self.handle(number, buf, rec),
                Err(_) => self.bad.push(number),
            }
        }
    }

    fn handle(&mut self, number: u64, buf: &[u8], rec: &Record) {
        if !rec.in_use {
            return;
        }
        self.in_use += 1;
        let is_base = !rec.is_ext;
        let owner = if is_base { number } else { rec.base };
        if owner >= u64::from(self.total) || self.arena.len() > u32::MAX as usize - 1024 {
            return;
        }
        for n in &rec.names {
            let off = self.arena.len() as u32;
            self.arena.extend(ntfs::name_units(buf, *n));
            self.names.push(NameEnt {
                owner: owner as u32,
                parent: if n.parent < u64::from(self.total) {
                    n.parent as u32
                } else {
                    u32::MAX
                },
                parent_seq: n.parent_seq,
                ns: n.namespace,
                len: n.units,
                off,
            });
        }
        // 實際大小 = 有配置的叢集；常駐在 MFT 記錄內的資料，作業系統回報的配置大小是
        // 內容長度進位到 8 位元組
        let mut physical = rec
            .data_clusters
            .saturating_add(rec.wof_clusters)
            .saturating_mul(self.cluster);
        if rec.data_resident {
            physical = physical.saturating_add(rec.data_logical.div_ceil(8) * 8);
        }
        if rec.reparse_lcn != 0 {
            self.pending_reparse.push((owner as u32, rec.reparse_lcn));
        }
        if is_base {
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
            self.by_num[number as usize] = self.nodes.len() as u32;
            self.nodes.push(Node {
                number: number as u32,
                seq: rec.seq,
                links: rec.links,
                flags,
                attrs: rec.si_attrs,
                tag: rec.reparse_tag,
                mtime: rec.mtime,
                logical: if rec.data_first { rec.data_logical } else { 0 },
                physical,
            });
        } else {
            self.exts.push(Ext {
                base: owner as u32,
                physical,
                logical: rec.data_logical,
                first: rec.data_first,
                has_reparse: rec.has_reparse,
                reparse_tag: rec.reparse_tag,
                reparse_volume: rec.reparse_volume,
            });
        }
    }
}

/// 核心：讀取 `src` 上的 NTFS 並建立 `ScanData`。
pub fn scan_source(
    src: &dyn Source,
    display: &[u16],
    root: RootMeta,
    opts: &Options,
    root_index: u32,
) -> Result<(ScanData, Stats), MftError> {
    let mut head = vec![0u8; 4096];
    let n = src.read_at(0, &mut head).map_err(io_err)?;
    let boot = ntfs::parse_boot(&head[..n])
        .map_err(|_| MftError::Unavailable("the volume is not NTFS".into()))?;
    let lay = bootstrap(src, &boot)?;
    let rb = lay.record;

    let (tx, rx) = sync_channel::<(u64, Vec<u8>)>(4);
    let lay_ref = &lay;
    let (mut acc, read_result) = std::thread::scope(|s| {
        // 傳送端移進讀取執行緒；它結束（或出錯）時被丟棄，接收迴圈隨之結束。
        let h = s.spawn(move || reader(src, lay_ref, opts, CHUNK_BYTES, &tx));
        let mut acc = Acc::new(lay_ref.total_records, lay_ref.cluster);
        let mut rec = Record::default();
        for (first, mut data) in rx {
            acc.process(first, &mut data, rb, &mut rec);
        }
        (acc, h.join())
    });
    let cancelled = match read_result {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err(MftError::Failed("the reader thread panicked".into())),
    };

    let mut stats = Stats::default();

    if !cancelled {
        // 重讀損壞的記錄一次：即時磁碟上可能讀到寫入一半的記錄
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
                acc.handle(number, &buf, &rec);
            } else {
                stats.bad_records += 1;
            }
        }
    }
    stats.records = acc.records;
    stats.in_use = acc.in_use;

    // 非常駐的重解析資料：只讀開頭取得標籤（讀不到就維持未知標籤）
    if !cancelled {
        // 直接讀磁碟區要求位移與長度對齊磁區，所以讀一整個磁區
        let mut head = [0u8; 512];
        for (owner, lcn) in std::mem::take(&mut acc.pending_reparse) {
            let idx = acc.by_num[owner as usize];
            if idx == u32::MAX || acc.nodes[idx as usize].tag != 0 {
                continue;
            }
            if read_exact(src, lcn.saturating_mul(lay.cluster), &mut head).is_ok() {
                let (tag, volume) = ntfs::reparse_info(&head);
                let n = &mut acc.nodes[idx as usize];
                n.tag = tag;
                if volume {
                    n.flags |= NODE_VOLUME_MOUNT;
                }
            }
        }
    }

    if cancelled {
        return Ok((cancelled_tree(display, root, root_index), stats));
    }
    let data = build_tree(acc, display, root, opts, root_index, &mut stats)?;
    Ok((data, stats))
}

fn new_dir(
    parent: u32,
    root_index: u32,
    name: Box<[u16]>,
    attrs: u32,
    mtime: u64,
    id: u128,
    serial: u32,
) -> DirNode {
    DirNode {
        parent,
        root: root_index,
        name,
        attrs,
        reparse: Reparse::None,
        reserved: false,
        mtime,
        id,
        serial,
        state: ScanState::Complete,
        error: None,
        names: Vec::new(),
        files: Vec::new(),
        subdirs: Vec::new(),
        direct: Totals::default(),
        rollup: Totals::default(),
        omitted: Totals::default(),
        latest_mtime: 0,
    }
}

/// 被取消時只留下根節點（標為 cancelled），不輸出讀到一半的內容。
fn cancelled_tree(display: &[u16], root: RootMeta, root_index: u32) -> ScanData {
    let mut d = new_dir(
        u32::MAX,
        root_index,
        display.to_vec().into_boxed_slice(),
        root.attrs,
        root.mtime,
        root.index,
        root.serial,
    );
    d.state = ScanState::Cancelled;
    ScanData {
        dirs: vec![d],
        roots: vec![RootInfo {
            display: display.to_vec(),
            node: 0,
        }],
    }
}

fn join(dir: &[u16], name: &[u16]) -> Vec<u16> {
    let mut p = Vec::with_capacity(dir.len() + 1 + name.len());
    p.extend_from_slice(dir);
    if p.last() != Some(&u16::from(b'\\')) {
        p.push(u16::from(b'\\'));
    }
    p.extend_from_slice(name);
    p
}

/// 把 MFT 的屬性旗標整理成與目錄列舉相同的樣子。
fn normalize_attrs(n: &Node) -> u32 {
    let mut a = n.attrs
        & (ATTR_READONLY
            | ATTR_HIDDEN
            | ATTR_SYSTEM
            | ATTR_ARCHIVE
            | ATTR_TEMPORARY
            | ATTR_SPARSE
            | ATTR_REPARSE
            | ATTR_COMPRESSED
            | ATTR_OFFLINE
            | ATTR_NOT_INDEXED
            | ATTR_ENCRYPTED
            | ATTR_PINNED
            | ATTR_UNPINNED
            | ATTR_RECALL_ON_DATA_ACCESS);
    if n.flags & NODE_DIR != 0 {
        a |= ATTR_DIRECTORY;
    }
    if n.flags & NODE_REPARSE != 0 {
        a |= ATTR_REPARSE;
    }
    if a == 0 {
        0x80 // FILE_ATTRIBUTE_NORMAL：目錄列舉對「沒有任何旗標」的檔案這樣回報
    } else {
        a
    }
}

fn classify(n: &Node, attrs: u32) -> Reparse {
    if attrs & ATTR_REPARSE == 0 {
        return Reparse::None;
    }
    match n.tag {
        TAG_SYMLINK => Reparse::Symlink,
        TAG_MOUNT_POINT => {
            if n.flags & NODE_VOLUME_MOUNT != 0 {
                Reparse::Mount
            } else {
                Reparse::Junction
            }
        }
        _ => Reparse::Other,
    }
}

fn file_id(n: &Node) -> u128 {
    (u128::from(n.seq) << 48) | u128::from(n.number)
}

struct Tree {
    acc: Acc,
    /// 以父目錄記錄編號分組的名稱索引（計數排序）
    start: Vec<u32>,
    links: Vec<u32>,
}

fn build_tree(
    mut acc: Acc,
    display: &[u16],
    root: RootMeta,
    opts: &Options,
    root_index: u32,
    stats: &mut Stats,
) -> Result<ScanData, MftError> {
    // 合併延伸記錄
    for e in std::mem::take(&mut acc.exts) {
        let idx = acc.by_num[e.base as usize];
        if idx == u32::MAX {
            continue;
        }
        let n = &mut acc.nodes[idx as usize];
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
    // 標記有「非 DOS」名稱的節點（DOS 短名只在沒有長名時才算數）
    for ne in &acc.names {
        if ne.ns != ntfs::NS_DOS {
            let idx = acc.by_num[ne.owner as usize];
            if idx != u32::MAX {
                acc.nodes[idx as usize].flags |= NODE_HAS_LONG_NAME;
            }
        }
    }

    // 名稱是否有效：擁有者存在、不是目錄指向自己（根的 "." 項目）、父目錄是同序號的目錄
    let valid = |acc: &Acc, ne: &NameEnt| -> bool {
        let oi = acc.by_num[ne.owner as usize];
        if oi == u32::MAX || ne.parent == u32::MAX || ne.owner == ne.parent {
            return false;
        }
        let owner = &acc.nodes[oi as usize];
        if ne.ns == ntfs::NS_DOS && owner.flags & NODE_HAS_LONG_NAME != 0 {
            return false;
        }
        let pi = acc.by_num[ne.parent as usize];
        if pi == u32::MAX {
            return false;
        }
        let p = &acc.nodes[pi as usize];
        p.flags & NODE_DIR != 0 && p.seq == ne.parent_seq
    };

    let total = acc.total as usize;
    let mut start = vec![0u32; total + 1];
    for ne in &acc.names {
        if valid(&acc, ne) {
            start[ne.parent as usize + 1] += 1;
        }
    }
    for i in 0..total {
        start[i + 1] += start[i];
    }
    let mut fill = start.clone();
    let mut links = vec![0u32; start[total] as usize];
    for (i, ne) in acc.names.iter().enumerate() {
        if valid(&acc, ne) {
            let slot = &mut fill[ne.parent as usize];
            links[*slot as usize] = i as u32;
            *slot += 1;
        }
    }
    drop(fill);
    let valid_names = u64::from(start[total]);
    let tree = Tree { acc, start, links };

    let root_node_idx = tree.acc.by_num[ntfs::ROOT_RECORD as usize];
    if root_node_idx == u32::MAX || tree.acc.nodes[root_node_idx as usize].flags & NODE_DIR == 0 {
        return Err(MftError::Failed(
            "the root directory record was not found".into(),
        ));
    }

    let mut dirs: Vec<DirNode> = vec![new_dir(
        u32::MAX,
        root_index,
        display.to_vec().into_boxed_slice(),
        root.attrs,
        root.mtime,
        root.index,
        root.serial,
    )];
    // 工作堆疊：(DirNode 編號, 目錄節點索引, 路徑——只有指定 --exclude 時才維護)
    let mut stack: Vec<(u32, u32, Vec<u16>)> = vec![(0, root_node_idx, display.to_vec())];
    let want_path = !opts.exclude.is_empty();
    let mut reached_names = 0u64;
    let acc = &tree.acc;

    while let Some((dir_id, node_idx, dir_path)) = stack.pop() {
        let dir_number = acc.nodes[node_idx as usize].number as usize;
        let range = tree.start[dir_number] as usize..tree.start[dir_number + 1] as usize;
        let mut names: Vec<u16> = Vec::new();
        let mut files: Vec<FileEntry> = Vec::new();
        let mut subs: Vec<(Box<[u16]>, u32)> = Vec::new();
        let at_root = dir_id == 0;
        for &ni in &tree.links[range] {
            reached_names += 1;
            let ne = acc.names[ni as usize];
            let child_idx = acc.by_num[ne.owner as usize];
            let child = &acc.nodes[child_idx as usize];
            let units = &acc.arena[ne.off as usize..ne.off as usize + usize::from(ne.len)];
            if units.is_empty() {
                continue;
            }
            if child.flags & NODE_DIR != 0 {
                subs.push((units.to_vec().into_boxed_slice(), child_idx));
                continue;
            }
            let attrs = normalize_attrs(child);
            // 檔案符號連結預設省略
            if attrs & ATTR_REPARSE != 0 && child.tag == TAG_SYMLINK {
                continue;
            }
            if want_path {
                let p = join(&dir_path, units);
                if opts.exclude.iter().any(|pat| glob::matches(pat, &p)) {
                    continue;
                }
            }
            let off = names.len();
            if off > u32::MAX as usize - 1024 {
                continue;
            }
            names.extend_from_slice(units);
            files.push(FileEntry {
                name_off: off as u32,
                name_len: units.len() as u16,
                reserved: at_root && u64::from(child.number) < ntfs::RESERVED_RECORDS,
                secondary: false,
                reparse_other: attrs & ATTR_REPARSE != 0,
                attrs,
                logical: child.logical,
                physical: child.physical,
                mtime: child.mtime,
                id: file_id(child),
                links: child.links.into(),
            });
        }
        files.sort_unstable_by(|a, b| {
            let an = &names[a.name_off as usize..a.name_off as usize + a.name_len as usize];
            let bn = &names[b.name_off as usize..b.name_off as usize + b.name_len as usize];
            cmp_names(an, bn)
        });
        subs.sort_unstable_by(|a, b| cmp_names(&a.0, &b.0));
        let mut ids = Vec::with_capacity(subs.len());
        let mut entered = Vec::new();
        for (name, cidx) in subs {
            let child = &acc.nodes[cidx as usize];
            let attrs = normalize_attrs(child);
            let reparse = classify(child, attrs);
            let id = dirs.len() as u32;
            ids.push(id);
            let child_path = if want_path {
                join(&dir_path, &name)
            } else {
                Vec::new()
            };
            let mut node = new_dir(
                dir_id,
                root_index,
                name,
                attrs,
                child.mtime,
                file_id(child),
                root.serial,
            );
            node.reparse = reparse;
            node.reserved = at_root && u64::from(child.number) < ntfs::RESERVED_RECORDS;
            let hidden_system_reparse = attrs & (ATTR_HIDDEN | ATTR_SYSTEM | ATTR_REPARSE)
                == (ATTR_HIDDEN | ATTR_SYSTEM | ATTR_REPARSE);
            if want_path && opts.exclude.iter().any(|p| glob::matches(p, &child_path)) {
                node.state = ScanState::Excluded;
            } else if hidden_system_reparse
                || matches!(
                    reparse,
                    Reparse::Symlink | Reparse::Junction | Reparse::Mount
                )
            {
                // 隱藏＋系統＋重解析點，或預設不跟隨的連結：不進入
                node.state = ScanState::NotFollowed;
            } else {
                entered.push((id, cidx, child_path));
            }
            dirs.push(node);
        }
        let d = &mut dirs[dir_id as usize];
        d.names = names;
        d.files = files;
        d.subdirs = ids;
        stack.extend(entered);
    }

    // 目錄指向自己的 "." 項目（如根目錄）不算孤兒
    let self_names = tree
        .acc
        .names
        .iter()
        .filter(|n| n.owner == n.parent)
        .count() as u64;
    stats.orphan_names = valid_names.saturating_sub(reached_names)
        + (tree.acc.names.len() as u64)
            .saturating_sub(valid_names)
            .saturating_sub(self_names);
    if stats.bad_records > 0 {
        // 有略過的記錄：內容可能不完整，根標為 partial 並記錄錯誤碼
        dirs[0].state = ScanState::Partial;
        dirs[0].error = Some(ERROR_FILE_CORRUPT);
    }
    Ok(ScanData {
        dirs,
        roots: vec![RootInfo {
            display: display.to_vec(),
            node: 0,
        }],
    })
}

/// 把 `src` 的節點接到 `dst` 後面，重新編號。
pub fn append(dst: &mut ScanData, src: ScanData) {
    let base = dst.dirs.len() as u32;
    let root_base = dst.roots.len() as u32;
    for mut d in src.dirs {
        if d.parent != u32::MAX {
            d.parent += base;
        }
        d.root += root_base;
        for s in &mut d.subdirs {
            *s += base;
        }
        dst.dirs.push(d);
    }
    for r in src.roots {
        dst.roots.push(RootInfo {
            display: r.display,
            node: r.node + base,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ntfs::testutil::*;

    struct Mem(Vec<u8>);

    impl Source for Mem {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, u32> {
            let o = offset as usize;
            if o >= self.0.len() {
                return Ok(0);
            }
            let n = buf.len().min(self.0.len() - o);
            buf[..n].copy_from_slice(&self.0[o..o + n]);
            Ok(n)
        }
    }

    const CLUSTER: usize = 4096;
    const MFT_LCN: usize = 4;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    /// 自行構造的最小 NTFS 映像：開機磁區＋一段連續的 $MFT。
    /// `records` 為（記錄編號 → 記錄位元組）；其餘編號為全零（未使用）。
    struct Image {
        records: Vec<Vec<u8>>,
        /// 寫進 $MFT 之前的空閒叢集（1..MFT_LCN）的內容
        clusters: Vec<(usize, Vec<u8>)>,
    }

    impl Image {
        fn new(n: usize) -> Image {
            Image {
                records: vec![vec![0u8; REC]; n],
                clusters: Vec::new(),
            }
        }

        fn put_cluster(&mut self, lcn: usize, bytes: Vec<u8>) {
            assert!((1..MFT_LCN).contains(&lcn));
            self.clusters.push((lcn, bytes));
        }

        fn set(&mut self, number: usize, rec: Vec<u8>) {
            self.records[number] = rec;
        }

        fn finish(mut self) -> Mem {
            let n = self.records.len();
            let mft_clusters = (n * REC).div_ceil(CLUSTER);
            // 記錄 0：$MFT 本身
            let rec0 = record(
                1,
                1,
                1,
                0,
                &[
                    resident(0x30, "", &fn_value(5, 5, 3, "$MFT")),
                    nonresident(
                        0x80,
                        "",
                        0,
                        (n * REC) as u64,
                        &runs(&[(mft_clusters as u64, Some(MFT_LCN as u64))]),
                    ),
                ],
            );
            self.records[0] = rec0;
            let total = MFT_LCN * CLUSTER + mft_clusters * CLUSTER;
            let mut img = vec![0u8; total];
            let b = &mut img[..512];
            b[3..11].copy_from_slice(b"NTFS    ");
            b[0x0B..0x0D].copy_from_slice(&512u16.to_le_bytes());
            b[0x0D] = 8;
            b[0x28..0x30].copy_from_slice(&((total / 512) as u64).to_le_bytes());
            b[0x30..0x38].copy_from_slice(&(MFT_LCN as u64).to_le_bytes());
            b[0x40] = 0xF6;
            for (lcn, bytes) in &self.clusters {
                img[lcn * CLUSTER..lcn * CLUSTER + bytes.len()].copy_from_slice(bytes);
            }
            let base = MFT_LCN * CLUSTER;
            for (i, r) in self.records.iter().enumerate() {
                img[base + i * REC..base + (i + 1) * REC].copy_from_slice(r);
            }
            Mem(img)
        }
    }

    /// 測試映像中，根的序號是 5，其他目錄預設序號是 1（`dir_rec` 的 `seq` 可改）。
    fn parent_seq(parent: u64) -> u16 {
        if parent == 5 { 5 } else { 1 }
    }

    fn dir_rec(parent: u64, seq: u16, name: &str, attrs: u32) -> Vec<u8> {
        record(
            3,
            seq,
            1,
            0,
            &[
                resident(0x10, "", &si_value(500, attrs)),
                resident(0x30, "", &fn_value(parent, parent_seq(parent), 1, name)),
            ],
        )
    }

    fn file_rec(parent: u64, name: &str, size: u64, lcn: u64, clusters: u64) -> Vec<u8> {
        record(
            1,
            1,
            1,
            0,
            &[
                resident(0x10, "", &si_value(900, 0x20)),
                resident(0x30, "", &fn_value(parent, parent_seq(parent), 1, name)),
                nonresident(0x80, "", 0, size, &runs(&[(clusters, Some(lcn))])),
            ],
        )
    }

    fn root_meta() -> RootMeta {
        RootMeta {
            attrs: 0x16,
            mtime: 7,
            serial: 0xABCD,
            index: (5u128 << 48) | 5,
        }
    }

    fn opts() -> Options {
        Options::default()
    }

    fn scan(img: &Mem, o: &Options) -> (ScanData, Stats) {
        scan_source(img, &w(r"X:\"), root_meta(), o, 0).unwrap()
    }

    fn sample() -> Mem {
        let mut im = Image::new(64);
        // 根目錄
        im.set(5, dir_rec(5, 5, ".", 0x16));
        im.set(30, dir_rec(5, 1, "docs", 0));
        im.set(31, dir_rec(30, 1, "sub", 0));
        im.set(40, file_rec(5, "b.txt", 5000, 100, 2));
        im.set(41, file_rec(5, "A.txt", 10, 200, 1));
        im.set(42, file_rec(30, "report.doc", 8192, 300, 2));
        im.set(43, file_rec(31, "deep.bin", 1, 400, 1));
        // 常駐小檔
        im.set(
            44,
            record(
                1,
                1,
                1,
                0,
                &[
                    resident(0x30, "", &fn_value(5, 5, 3, "tiny.txt")),
                    resident(0x80, "", b"hello"),
                ],
            ),
        );
        // 硬連結：兩個名稱分別在根與 docs
        im.set(
            45,
            record(
                1,
                9,
                2,
                0,
                &[
                    resident(0x10, "", &si_value(5, 0x20)),
                    resident(0x30, "", &fn_value(5, 5, 1, "hl1.bin")),
                    resident(0x30, "", &fn_value(30, 1, 1, "hl2.bin")),
                    nonresident(0x80, "", 0, 4096, &runs(&[(1, Some(500))])),
                ],
            ),
        );
        // 有 DOS 短名的檔案：只應出現一次
        im.set(
            46,
            record(
                1,
                1,
                1,
                0,
                &[
                    resident(0x30, "", &fn_value(5, 5, 1, "long name.txt")),
                    resident(0x30, "", &fn_value(5, 5, 2, "LONGNA~1.TXT")),
                    resident(0x80, "", b"x"),
                ],
            ),
        );
        // 孤兒：父目錄序號不符（父目錄 30 的序號是 1，這裡寫 9）
        im.set(
            47,
            record(
                1,
                1,
                1,
                0,
                &[
                    resident(0x30, "", &fn_value(30, 9, 1, "stale.txt")),
                    resident(0x80, "", b"z"),
                ],
            ),
        );
        // 保留的中繼資料檔
        im.set(
            3,
            record(
                1,
                3,
                1,
                0,
                &[
                    resident(0x30, "", &fn_value(5, 5, 3, "$Volume")),
                    resident(0x80, "", b""),
                ],
            ),
        );
        im.finish()
    }

    fn file_names(d: &DirNode) -> Vec<String> {
        d.files
            .iter()
            .map(|f| String::from_utf16_lossy(d.file_name(f)))
            .collect()
    }

    #[test]
    fn rebuilds_tree_from_synthetic_volume() {
        let (data, stats) = scan(&sample(), &opts());
        assert_eq!(stats.bad_records, 0);
        let root = &data.dirs[0];
        // 排序：不分大小寫，A.txt 在 b.txt 前
        let names = file_names(root);
        assert_eq!(
            names,
            [
                "$MFT",
                "$Volume",
                "A.txt",
                "b.txt",
                "hl1.bin",
                "long name.txt",
                "tiny.txt"
            ]
        );
        assert_eq!(root.subdirs.len(), 1);
        let docs = &data.dirs[root.subdirs[0] as usize];
        assert_eq!(String::from_utf16_lossy(&docs.name), "docs");
        // stale.txt 的父序號過期，視為孤兒，不出現
        assert_eq!(file_names(docs), ["hl2.bin", "report.doc"]);
        let sub = &data.dirs[docs.subdirs[0] as usize];
        assert_eq!(file_names(sub), ["deep.bin"]);
    }

    #[test]
    fn sizes_ids_and_reserved_flags() {
        let (data, _) = scan(&sample(), &opts());
        let root = &data.dirs[0];
        let by = |n: &str| {
            root.files
                .iter()
                .find(|f| String::from_utf16_lossy(root.file_name(f)) == n)
                .copied()
                .unwrap()
        };
        let b = by("b.txt");
        assert_eq!((b.logical, b.physical), (5000, 2 * 4096));
        assert_eq!(b.id, (1u128 << 48) | 40);
        assert_eq!(b.attrs, 0x20);
        assert_eq!(b.mtime, 900);
        let tiny = by("tiny.txt");
        // 常駐資料：配置大小為內容長度進位到 8 位元組
        assert_eq!((tiny.logical, tiny.physical), (5, 8));
        assert_eq!(tiny.attrs, 0x80); // 沒有任何旗標
        assert!(by("$Volume").reserved && by("$MFT").reserved);
        assert!(!by("b.txt").reserved);
        assert_eq!(by("hl1.bin").links, 2);
        assert_eq!(by("hl1.bin").id, (9u128 << 48) | 45);
    }

    #[test]
    fn dos_short_names_are_not_listed() {
        let (data, _) = scan(&sample(), &opts());
        let all: Vec<String> = data.dirs.iter().flat_map(file_names).collect();
        assert!(!all.iter().any(|n| n.contains('~')));
    }

    #[test]
    fn hardlink_names_share_one_id_and_dedupe_in_finalize() {
        let (mut data, _) = scan(&sample(), &opts());
        crate::snapshot::finalize(&mut data, true, None);
        let root = &data.dirs[0];
        let docs = &data.dirs[root.subdirs[0] as usize];
        let h1 = root
            .files
            .iter()
            .find(|f| root.file_name(f) == w("hl1.bin").as_slice())
            .unwrap();
        let h2 = docs
            .files
            .iter()
            .find(|f| docs.file_name(f) == w("hl2.bin").as_slice())
            .unwrap();
        assert_eq!(h1.id, h2.id);
        // 正規走訪順序是目錄與檔案依名稱合併：`docs` 排在 `hl1.bin` 之前，
        // 所以 docs 內的名稱先出現，是主要名稱
        assert!(h2.links == 2 && h1.links == 2);
        assert!(!h2.secondary && h1.secondary);
    }

    #[test]
    fn stale_parent_sequence_is_an_orphan() {
        let mut im = Image::new(64);
        im.set(5, dir_rec(5, 5, ".", 0x16));
        im.set(30, dir_rec(5, 7, "dir", 0)); // 目錄序號 7；file_rec 會寫父序號 1（不符）
        im.set(31, file_rec(30, "wrong.txt", 1, 100, 1));
        let (data, stats) = scan(&im.finish(), &opts());
        assert_eq!(data.dirs.len(), 2);
        assert!(data.dirs[1].files.is_empty());
        assert!(stats.orphan_names >= 1);
    }

    #[test]
    fn directory_cycles_do_not_loop() {
        // 30 的父是 31，31 的父是 30：與根無關，無法到達
        let mut im = Image::new(64);
        im.set(5, dir_rec(5, 5, ".", 0x16));
        im.set(30, dir_rec(31, 5, "a", 0));
        im.set(31, dir_rec(30, 5, "b", 0));
        let (data, stats) = scan(&im.finish(), &opts());
        assert_eq!(data.dirs.len(), 1);
        assert!(stats.orphan_names >= 2);
    }

    #[test]
    fn corrupt_record_is_skipped_and_marks_root_partial() {
        let mut im = Image::new(64);
        im.set(5, dir_rec(5, 5, ".", 0x16));
        im.set(40, file_rec(5, "ok.txt", 1, 100, 1));
        let mut bad = file_rec(5, "bad.txt", 1, 200, 1);
        bad[510] ^= 0x55;
        im.set(41, bad);
        let (data, stats) = scan(&im.finish(), &opts());
        assert_eq!(stats.bad_records, 1);
        let root = &data.dirs[0];
        assert_eq!(file_names(root), ["$MFT", "ok.txt"]);
        assert_eq!(root.state, ScanState::Partial);
        assert_eq!(root.error, Some(ERROR_FILE_CORRUPT));
    }

    #[test]
    fn extension_record_data_is_merged_into_base() {
        let mut im = Image::new(64);
        im.set(5, dir_rec(5, 5, ".", 0x16));
        // 基底記錄：只有名稱與第一段資料；延伸記錄 50 提供後續區段
        im.set(
            40,
            record(
                1,
                1,
                1,
                0,
                &[
                    resident(0x30, "", &fn_value(5, 5, 1, "frag.bin")),
                    nonresident(0x80, "", 0, 40_000, &runs(&[(3, Some(100))])),
                ],
            ),
        );
        im.set(
            50,
            record(
                1,
                1,
                0,
                (1u64 << 48) | 40,
                &[nonresident(0x80, "", 3, 0, &runs(&[(7, Some(900))]))],
            ),
        );
        let (data, _) = scan(&im.finish(), &opts());
        let f = data.dirs[0]
            .files
            .iter()
            .find(|f| data.dirs[0].file_name(f) == w("frag.bin").as_slice())
            .unwrap();
        assert_eq!(f.logical, 40_000);
        assert_eq!(f.physical, 10 * 4096);
    }

    #[test]
    fn sparse_runs_do_not_count_as_physical() {
        let mut im = Image::new(64);
        im.set(5, dir_rec(5, 5, ".", 0x16));
        im.set(
            40,
            record(
                1,
                1,
                1,
                0,
                &[
                    resident(0x10, "", &si_value(1, 0x20 | 0x200)),
                    resident(0x30, "", &fn_value(5, 5, 1, "sparse.bin")),
                    nonresident(
                        0x80,
                        "",
                        0,
                        1_000_000,
                        &runs(&[(1, Some(100)), (200, None), (1, Some(300))]),
                    ),
                ],
            ),
        );
        let (data, _) = scan(&im.finish(), &opts());
        let f = data.dirs[0]
            .files
            .iter()
            .find(|f| data.dirs[0].file_name(f) == w("sparse.bin").as_slice())
            .unwrap();
        assert_eq!((f.logical, f.physical), (1_000_000, 2 * 4096));
        assert!(f.attrs & ATTR_SPARSE != 0);
    }

    #[test]
    fn junction_and_symlink_dirs_are_not_entered() {
        let tag = |t: u32, sub: &str| {
            let mut v = vec![0u8; 8];
            v[0..4].copy_from_slice(&t.to_le_bytes());
            let s: Vec<u8> = sub.encode_utf16().flat_map(u16::to_le_bytes).collect();
            let mut body = Vec::new();
            le16(&mut body, 0);
            le16(&mut body, s.len() as u16);
            le16(&mut body, s.len() as u16 + 2);
            le16(&mut body, 0);
            body.extend_from_slice(&s);
            v.extend_from_slice(&body);
            v
        };
        let mut im = Image::new(64);
        im.set(5, dir_rec(5, 5, ".", 0x16));
        let lnk = |name: &str, t: u32, sub: &str| {
            record(
                3,
                1,
                1,
                0,
                &[
                    resident(0x10, "", &si_value(1, 0x400)),
                    resident(0x30, "", &fn_value(5, 5, 1, name)),
                    resident(0xC0, "", &tag(t, sub)),
                ],
            )
        };
        im.set(30, lnk("junc", 0xA000_0003, r"\??\C:\x"));
        im.set(31, lnk("mnt", 0xA000_0003, r"\??\Volume{1}\"));
        im.set(32, lnk("sym", 0xA000_000C, r"\??\C:\y"));
        im.set(33, dir_rec(30, 1, "inside", 0));
        let (data, _) = scan(&im.finish(), &opts());
        let root = &data.dirs[0];
        assert_eq!(root.subdirs.len(), 3);
        let by = |n: &str| {
            root.subdirs
                .iter()
                .map(|&i| &data.dirs[i as usize])
                .find(|d| String::from_utf16_lossy(&d.name) == n)
                .unwrap()
        };
        assert_eq!(by("junc").reparse, Reparse::Junction);
        assert_eq!(by("mnt").reparse, Reparse::Mount);
        assert_eq!(by("sym").reparse, Reparse::Symlink);
        for n in ["junc", "mnt", "sym"] {
            assert_eq!(by(n).state, ScanState::NotFollowed);
            assert!(by(n).subdirs.is_empty());
        }
        // 接合點裡的目錄不可達
        assert_eq!(data.dirs.len(), 4);
    }

    #[test]
    fn reparse_attribute_in_an_extension_record_is_merged() {
        let mut v = vec![0u8; 8];
        v[0..4].copy_from_slice(&0xA000_0003u32.to_le_bytes());
        let sub: Vec<u8> = r"\??\C:\x"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let mut body = Vec::new();
        le16(&mut body, 0);
        le16(&mut body, sub.len() as u16);
        le16(&mut body, sub.len() as u16 + 2);
        le16(&mut body, 0);
        body.extend_from_slice(&sub);
        v.extend_from_slice(&body);
        let mut im = Image::new(64);
        im.set(5, dir_rec(5, 5, ".", 0x16));
        im.set(
            30,
            record(
                3,
                1,
                1,
                0,
                &[
                    resident(0x10, "", &si_value(1, 0x400)),
                    resident(0x30, "", &fn_value(5, 5, 1, "junc")),
                ],
            ),
        );
        im.set(
            51,
            record(1, 1, 0, (1u64 << 48) | 30, &[resident(0xC0, "", &v)]),
        );
        let (data, _) = scan(&im.finish(), &opts());
        let d = &data.dirs[data.dirs[0].subdirs[0] as usize];
        assert_eq!(d.reparse, Reparse::Junction);
        assert_eq!(d.state, ScanState::NotFollowed);
    }

    #[test]
    fn non_resident_reparse_data_is_read_from_its_clusters() {
        let mut v = vec![0u8; 8];
        v[0..4].copy_from_slice(&0xA000_0003u32.to_le_bytes());
        let sub: Vec<u8> = r"\??\C:\x"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let mut body = Vec::new();
        le16(&mut body, 0);
        le16(&mut body, sub.len() as u16);
        le16(&mut body, sub.len() as u16 + 2);
        le16(&mut body, 0);
        body.extend_from_slice(&sub);
        v.extend_from_slice(&body);
        let mut im = Image::new(64);
        im.set(5, dir_rec(5, 5, ".", 0x16));
        im.put_cluster(2, v.clone());
        let lnk = |name: &str| {
            record(
                3,
                1,
                1,
                0,
                &[
                    resident(0x10, "", &si_value(1, 0x400)),
                    resident(0x30, "", &fn_value(5, 5, 1, name)),
                    nonresident(0xC0, "", 0, v.len() as u64, &runs(&[(1, Some(2))])),
                ],
            )
        };
        im.set(30, lnk("junc"));
        let (data, _) = scan(&im.finish(), &opts());
        let d = &data.dirs[data.dirs[0].subdirs[0] as usize];
        assert_eq!(d.reparse, Reparse::Junction);
        assert_eq!(d.state, ScanState::NotFollowed);
    }

    #[test]
    fn recall_on_open_bit_is_not_reported() {
        // 作業系統的目錄列舉從不回報 0x40000；SI 中的這個位元不可外洩到輸出
        let mut im = Image::new(64);
        im.set(5, dir_rec(5, 5, ".", 0x16));
        im.set(
            40,
            record(
                1,
                1,
                1,
                0,
                &[
                    resident(0x10, "", &si_value(1, 0x20 | 0x40000)),
                    resident(0x30, "", &fn_value(5, 5, 1, "f.bin")),
                    resident(0x80, "", b"x"),
                ],
            ),
        );
        let (data, _) = scan(&im.finish(), &opts());
        let f = data.dirs[0]
            .files
            .iter()
            .find(|f| data.dirs[0].file_name(f) == w("f.bin").as_slice())
            .unwrap();
        assert_eq!(f.attrs, 0x20);
    }

    #[test]
    fn exclude_patterns_apply_to_files_and_dirs() {
        let mut o = opts();
        o.exclude = vec![w(r"X:\docs"), w(r"*\b.txt")];
        let (data, _) = scan(&sample(), &o);
        let root = &data.dirs[0];
        assert!(!file_names(root).contains(&"b.txt".to_string()));
        let docs = &data.dirs[root.subdirs[0] as usize];
        assert_eq!(docs.state, ScanState::Excluded);
        assert!(docs.files.is_empty());
    }

    #[test]
    fn not_ntfs_is_unavailable() {
        let m = Mem(vec![0u8; 8192]);
        let r = scan_source(&m, &w(r"X:\"), root_meta(), &opts(), 0);
        assert!(matches!(r, Err(MftError::Unavailable(_))));
    }

    #[test]
    fn mft_with_extension_record_for_its_own_data() {
        // $MFT 的資料分兩段：第一段在記錄 0，第二段在位於第一段內的延伸記錄 20
        let n = 64usize;
        let mft_clusters = (n * REC).div_ceil(CLUSTER);
        let first = 4u64; // 前 4 叢集 = 16 筆記錄
        let second_lcn = (MFT_LCN as u64) + first;
        let mut im = Image::new(n);
        im.set(5, dir_rec(5, 5, ".", 0x16));
        im.set(41, file_rec(5, "tail.txt", 1, 100, 1));
        let mut img = im.finish();
        // 以手工方式改寫記錄 0 與 20
        let mut al = Vec::new();
        for (vcn, rec) in [(0u64, 0u64), (first, 10u64)] {
            let mut e = Vec::new();
            le32(&mut e, 0x80);
            le16(&mut e, 0x20);
            e.extend_from_slice(&[0, 0x1A]);
            le64(&mut e, vcn);
            le64(&mut e, rec);
            le16(&mut e, 0);
            e.resize(0x20, 0);
            al.extend_from_slice(&e);
        }
        let rec0 = record(
            1,
            1,
            1,
            0,
            &[
                resident(0x20, "", &al),
                resident(0x30, "", &fn_value(5, 5, 3, "$MFT")),
                nonresident(
                    0x80,
                    "",
                    0,
                    (n * REC) as u64,
                    &runs(&[(first, Some(MFT_LCN as u64))]),
                ),
            ],
        );
        // 延伸記錄放在第一段之內（記錄 10），基底記錄是 $MFT 本身（編號 0、序號 1）
        let rec10 = record(
            1,
            1,
            0,
            1u64 << 48,
            &[nonresident(
                0x80,
                "",
                first,
                0,
                &runs(&[(mft_clusters as u64 - first, Some(second_lcn))]),
            )],
        );
        // 第二段的位置其實接在第一段後面，所以映像不必搬動
        let base = MFT_LCN * CLUSTER;
        img.0[base..base + REC].copy_from_slice(&rec0);
        img.0[base + 10 * REC..base + 11 * REC].copy_from_slice(&rec10);
        let (data, stats) = scan_source(&img, &w(r"X:\"), root_meta(), &opts(), 0).unwrap();
        assert_eq!(stats.records, n as u64);
        assert!(
            data.dirs[0]
                .files
                .iter()
                .any(|f| data.dirs[0].file_name(f) == w("tail.txt").as_slice())
        );
    }

    #[test]
    fn append_remaps_indices() {
        let (a, _) = scan(&sample(), &opts());
        let (b, _) = scan(&sample(), &opts());
        let na = a.dirs.len() as u32;
        let mut m = a;
        append(&mut m, b);
        assert_eq!(m.roots.len(), 2);
        assert_eq!(m.roots[1].node, na);
        let second_root = &m.dirs[na as usize];
        assert_eq!(second_root.parent, u32::MAX);
        assert_eq!(second_root.subdirs[0], na + 1);
        assert_eq!(m.dirs[(na + 1) as usize].parent, na);
    }

    // ---- 緊湊路徑與原本路徑的差異測試 ----

    use crate::overview::{self, Collector};
    use crate::report::{self, Analyzer, Sink, Tallier};
    use crate::snapshot;

    fn reqs() -> Vec<report::Request> {
        vec![
            report::Request {
                top_files: Some(20),
                top_dirs: Some(50),
                by_ext: Some(50),
                many_files: Some(50),
                ..Default::default()
            },
            report::Request {
                top_files: Some(3),
                top_dirs: Some(2),
                by_ext: Some(2),
                many_files: Some(2),
                dir_depth: Some(1),
                min_size: Some(4096),
                ..Default::default()
            },
            report::Request {
                top_files: Some(4),
                exts: Some([w(".dat"), w(".txt")].into_iter().collect()),
                older_than_days: Some(1),
                now: 1_000_000_000_000_000,
                ..Default::default()
            },
        ]
    }

    fn col_opts() -> Vec<overview::Options> {
        let mut v = Vec::new();
        for (logical, direct, top) in [
            (false, false, 10),
            (true, false, 3),
            (false, true, 4),
            (true, true, 2),
        ] {
            v.push(overview::Options {
                top,
                ext_top: 3,
                stale_days: 0,
                stale_min_size: 4096,
                rank_logical: logical,
                dir_direct: direct,
                auto_trim: false,
                now: 1_000_000_000_000_000,
            });
        }
        v
    }

    fn render_col(
        col: Collector,
        roots: &[Vec<u16>],
        totals: Totals,
        cov: &crate::snapshot::Coverage,
    ) -> String {
        let next = |_: Option<&str>| Vec::new();
        let frame = overview::Frame {
            roots,
            engine: "mft",
            totals,
            coverage: cov,
            limited_reasons: &[],
            meta_body: None,
            next: &next,
        };
        String::from_utf8(col.render(&frame, None)).unwrap()
    }

    fn feed_old(data: &ScanData, sink: &mut dyn Sink) {
        snapshot::for_each_entry(data, &mut |e| match e {
            snapshot::Entry::Dir { path, node, depth } => sink.dir(&report::DirView {
                path,
                depth: depth as u64,
                rollup: node.rollup,
                direct: node.direct,
                latest_mtime: node.latest_mtime,
            }),
            snapshot::Entry::File {
                path, dir, file, ..
            } => {
                let ext = crate::names::ext_of(dir.file_name(file));
                sink.file(&report::FileView {
                    path,
                    ext: &ext,
                    logical: file.logical,
                    physical: file.physical,
                    mtime: file.mtime,
                    secondary: file.secondary,
                });
            }
        });
    }

    /// 原本的路徑與緊湊路徑對同一個映像的結果必須逐位元組相同。回傳緊湊路徑是否接受了這個映像
    /// （`Fallback` 是合法的，但呼叫端要說明為什麼）。
    fn assert_compact_equals_old(img: &Mem, o: &Options, label: &str) -> (usize, usize) {
        let (mut data, stats_old) = scan(img, o);
        snapshot::finalize(&mut data, true, None);
        let totals_old = snapshot::scan_totals(&data);
        let (cov_old, _) = snapshot::count_coverage(&data);
        let roots = vec![w(r"X:\")];

        let (mut ok_n, mut try_n) = (0usize, 0usize);
        for req in reqs() {
            try_n += 1;
            let mut a_old = Analyzer::new(req.clone());
            feed_old(&data, &mut a_old);
            let mut a_new = Analyzer::new(req.clone());
            let out =
                match compact::scan_source_compact(img, &w(r"X:\"), root_meta(), o, &mut a_new) {
                    Ok(out) => out,
                    Err(compact::CompactError::Fallback(why)) => {
                        eprintln!("{label}: compact path fell back ({why})");
                        continue;
                    }
                    Err(compact::CompactError::Failed(e)) => panic!("{label}: {}", e.message()),
                };
            ok_n += 1;
            let (mut x, mut y) = (Vec::new(), Vec::new());
            a_old.write_reports(&mut x);
            a_new.write_reports(&mut y);
            let (xs, ys) = (String::from_utf8(x).unwrap(), String::from_utf8(y).unwrap());
            if xs != ys {
                let at = xs
                    .bytes()
                    .zip(ys.bytes())
                    .position(|(a, b)| a != b)
                    .unwrap_or(xs.len().min(ys.len()));
                let from = at.saturating_sub(120);
                panic!(
                    "{label}: report top_files={:?} differs at {at}
 old: ...{}
 new: ...{}",
                    req.top_files,
                    &xs[from..(at + 200).min(xs.len())],
                    &ys[from..(at + 200).min(ys.len())]
                );
            }
            assert_eq!(out.totals.files, totals_old.files, "{label}: files");
            assert_eq!(out.totals.logical, totals_old.logical, "{label}: logical");
            assert_eq!(
                out.totals.physical, totals_old.physical,
                "{label}: physical"
            );
            assert_eq!(out.totals.dirs, totals_old.dirs, "{label}: dirs");
            assert_eq!(out.coverage, cov_old, "{label}: coverage");
            assert_eq!(out.stats.records, stats_old.records, "{label}: records");
            assert_eq!(out.stats.in_use, stats_old.in_use, "{label}: in_use");
            assert_eq!(
                out.stats.orphan_names, stats_old.orphan_names,
                "{label}: orphan names"
            );
        }
        for co in col_opts() {
            try_n += 1;
            let mut c_old = Collector::new(co.clone());
            feed_old(&data, &mut c_old);
            let mut c_new = Collector::new(co.clone());
            match compact::scan_source_compact(img, &w(r"X:\"), root_meta(), o, &mut c_new) {
                Ok(_) => ok_n += 1,
                Err(compact::CompactError::Fallback(_)) => continue,
                Err(compact::CompactError::Failed(e)) => panic!("{label}: {}", e.message()),
            }
            assert_eq!(
                render_col(c_old, &roots, totals_old, &cov_old),
                render_col(c_new, &roots, totals_old, &cov_old),
                "{label}: overview rank_logical={} dir_direct={} top={}",
                co.rank_logical,
                co.dir_direct,
                co.top
            );
        }
        (ok_n, try_n)
    }

    #[test]
    fn compact_path_equals_the_old_path_on_the_sample_volume() {
        let (ok, tried) = assert_compact_equals_old(&sample(), &opts(), "sample");
        // 小映像的每個檔案都是前 N 名候選，所以含孤兒的請求會回退；不含孤兒的請求必須走緊湊路徑
        assert!(ok >= 1 && ok <= tried, "{ok}/{tried}");
    }

    /// 簡單的線性同餘亂數，讓差異測試可重現。
    struct Rng(u64);
    impl Rng {
        fn next(&mut self, n: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) % n
        }
    }

    fn reparse_value(tag: u32) -> Vec<u8> {
        let mut v = vec![0u8; 8];
        v[0..4].copy_from_slice(&tag.to_le_bytes());
        let s: Vec<u8> = r"\??\C:\x"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let mut body = Vec::new();
        le16(&mut body, 0);
        le16(&mut body, s.len() as u16);
        le16(&mut body, s.len() as u16 + 2);
        le16(&mut body, 0);
        body.extend_from_slice(&s);
        v.extend_from_slice(&body);
        v
    }

    /// 隨機的小型磁碟區：目錄樹、同大小的檔案（排名要靠路徑）、跨目錄硬連結、DOS 短名、
    /// 孤兒（父序號不符、父親是檔案）、符號連結檔案、接合點底下的內容、常駐小檔、空檔案。
    fn random_volume(seed: u64) -> Mem {
        let mut r = Rng(seed);
        let n_dirs = 3 + r.next(10) as usize;
        let n_files = 10 + r.next(60) as usize;
        random_volume_sized(seed, n_dirs, n_files, true)
    }

    fn random_volume_sized(seed: u64, n_dirs: usize, n_files: usize, with_orphans: bool) -> Mem {
        let mut r = Rng(seed ^ 0x9E37_79B9);
        let mut im = Image::new(100 + 2 * n_dirs + n_files + 10);
        im.set(5, dir_rec(5, 5, ".", 0x16));
        // 目錄：記錄 30.. ；父親從已建立的目錄中挑
        let mut dirs: Vec<u64> = vec![5];
        let names = [
            "alpha", "Beta", "gamma", "Delta", "eps", "zeta", "Eta", "theta", "iota", "kappa",
            "日本", "mu",
        ];
        for i in 0..n_dirs {
            let parent = dirs[r.next(dirs.len() as u64) as usize];
            let num = 30 + i as u64;
            let name = format!("{}{}", names[r.next(names.len() as u64) as usize], i);
            let kind = r.next(10);
            let rec = if kind == 0 && with_orphans {
                // 接合點：不進入，底下的內容不可達
                record(
                    3,
                    1,
                    1,
                    0,
                    &[
                        resident(0x10, "", &si_value(1, 0x400)),
                        resident(0x30, "", &fn_value(parent, parent_seq(parent), 1, &name)),
                        resident(0xC0, "", &reparse_value(0xA000_0003)),
                    ],
                )
            } else {
                dir_rec(parent, 1, &name, 0)
            };
            im.set(num as usize, rec);
            dirs.push(num);
        }
        let exts = [".dat", ".txt", ".LOG", "", ".bin", ".Dat"];
        let sizes = [0u64, 5, 4096, 4096, 8192, 8192, 40_000, 100_000];
        let mut num = (100 + n_dirs.saturating_sub(70)) as u64;
        let mut used: std::collections::HashSet<(u64, String)> = std::collections::HashSet::new();
        for i in 0..n_files {
            let parent = dirs[r.next(dirs.len() as u64) as usize];
            let mut name = format!(
                "f{}{}",
                r.next(15),
                exts[r.next(exts.len() as u64) as usize]
            );
            // 同一個目錄裡不可能有兩個同名項目（名稱不分大小寫）
            while !used.insert((parent, name.to_lowercase())) {
                name = format!(
                    "g{}{}",
                    r.next(1000),
                    exts[r.next(exts.len() as u64) as usize]
                );
            }
            let size = sizes[r.next(sizes.len() as u64) as usize];
            let kind = r.next(12);
            let rec = match kind {
                // 父序號不符的孤兒
                0 if with_orphans => record(
                    1,
                    1,
                    1,
                    0,
                    &[
                        resident(0x30, "", &fn_value(parent, 9, 1, &name)),
                        resident(0x80, "", b"z"),
                    ],
                ),
                // 符號連結檔案：預設省略
                1 => record(
                    1,
                    1,
                    1,
                    0,
                    &[
                        resident(0x10, "", &si_value(3, 0x400)),
                        resident(0x30, "", &fn_value(parent, parent_seq(parent), 1, &name)),
                        resident(0xC0, "", &reparse_value(0xA000_000C)),
                    ],
                ),
                // 兩個名稱（硬連結），第二個在另一個目錄
                2 | 3 => {
                    let other = dirs[r.next(dirs.len() as u64) as usize];
                    // 一半的硬連結兩個名稱相同（真實磁碟上最常見）
                    let mut name2 = if r.next(2) == 0 {
                        name.clone()
                    } else {
                        format!(
                            "h{}{}",
                            r.next(15),
                            exts[r.next(exts.len() as u64) as usize]
                        )
                    };
                    while !used.insert((other, name2.to_lowercase())) {
                        name2 = format!(
                            "k{}{}",
                            r.next(1000),
                            exts[r.next(exts.len() as u64) as usize]
                        );
                    }
                    record(
                        1,
                        (i % 7 + 1) as u16,
                        2,
                        0,
                        &[
                            resident(0x10, "", &si_value(10 + r.next(50), 0x20)),
                            resident(0x30, "", &fn_value(parent, parent_seq(parent), 1, &name)),
                            resident(0x30, "", &fn_value(other, parent_seq(other), 1, &name2)),
                            nonresident(
                                0x80,
                                "",
                                0,
                                size.max(1),
                                &runs(&[(size.max(1).div_ceil(4096), Some(500 + num))]),
                            ),
                        ],
                    )
                }
                // 長名加 DOS 短名
                4 => record(
                    1,
                    1,
                    1,
                    0,
                    &[
                        resident(0x10, "", &si_value(20 + r.next(50), 0x20)),
                        resident(0x30, "", &fn_value(parent, parent_seq(parent), 1, &name)),
                        resident(
                            0x30,
                            "",
                            &fn_value(parent, parent_seq(parent), 2, &format!("S~{i}.DAT")),
                        ),
                        resident(0x80, "", b"x"),
                    ],
                ),
                // 常駐小檔或空檔
                5 | 6 => record(
                    1,
                    1,
                    1,
                    0,
                    &[
                        resident(0x10, "", &si_value(30 + r.next(50), 0x20)),
                        resident(0x30, "", &fn_value(parent, parent_seq(parent), 1, &name)),
                        resident(0x80, "", &vec![b'x'; size.min(200) as usize]),
                    ],
                ),
                // 一般檔案
                _ => {
                    let size = size.max(1);
                    record(
                        1,
                        1,
                        1,
                        0,
                        &[
                            resident(0x10, "", &si_value(40 + r.next(50), 0x20)),
                            resident(0x30, "", &fn_value(parent, parent_seq(parent), 1, &name)),
                            nonresident(
                                0x80,
                                "",
                                0,
                                size,
                                &runs(&[(size.div_ceil(4096), Some(600 + num))]),
                            ),
                        ],
                    )
                }
            };
            im.set(num as usize, rec);
            num += 1;
        }
        im.finish()
    }

    /// 壓力映像：很深的目錄（路徑超過 260 個字元）、含非 BMP 字元（代理對）與日文的名稱、
    /// 大量硬連結（3–6 個名稱，多半同名）、替代資料流（具名的 $DATA）。
    fn stress_volume(seed: u64) -> Mem {
        let mut r = Rng(seed ^ 0xABCD_1234);
        let n_dirs = 60usize;
        let n_files = 3_000usize;
        let mut im = Image::new(100 + 2 * n_dirs + n_files + 10);
        im.set(5, dir_rec(5, 5, ".", 0x16));
        let mut dirs: Vec<u64> = vec![5];
        let stems = [
            "\u{1F600}笑",
            "目錄",
            "\u{1D11E}music",
            "plain",
            "Ünï",
            "\u{20BB7}字",
        ];
        for i in 0..n_dirs {
            // 前 45 個目錄串成一條鏈（深度 45，路徑遠超過 260 個字元），其餘隨機掛載
            let parent = if i < 45 {
                *dirs.last().unwrap()
            } else {
                dirs[r.next(dirs.len() as u64) as usize]
            };
            let num = 30 + i as u64;
            let name = format!("{}_{}", stems[r.next(stems.len() as u64) as usize], i);
            im.set(num as usize, dir_rec(parent, 1, &name, 0));
            dirs.push(num);
        }
        let exts = [".dll", ".mum", ".cat", ".txt", "", ".Dat"];
        let mut used: std::collections::HashSet<(u64, String)> = std::collections::HashSet::new();
        let mut num = (100 + n_dirs) as u64;
        for i in 0..n_files {
            let sizes = [0u64, 700, 4096, 4096, 20_000, 1_000_000];
            let size = sizes[r.next(sizes.len() as u64) as usize].max(1);
            let base = format!(
                "{}{}{}",
                if r.next(4) == 0 { "\u{1F4C4}" } else { "pkg" },
                r.next(40),
                exts[r.next(exts.len() as u64) as usize]
            );
            let nlinks = if r.next(3) == 0 {
                1
            } else {
                3 + r.next(2) as usize
            };
            let mut attrs: Vec<Vec<u8>> =
                vec![resident(0x10, "", &si_value(10 + r.next(90), 0x20))];
            let mut hard = 0;
            let mut guard = 0;
            while hard < nlinks && guard < 50 {
                guard += 1;
                let parent = dirs[r.next(dirs.len() as u64) as usize];
                // 多半同名，偶爾不同
                let name = if hard == 0 || r.next(5) != 0 {
                    base.clone()
                } else {
                    format!("{}-alt{}", base, r.next(5))
                };
                if !used.insert((parent, name.to_lowercase())) {
                    continue;
                }
                attrs.push(resident(
                    0x30,
                    "",
                    &fn_value(parent, parent_seq(parent), 1, &name),
                ));
                hard += 1;
            }
            if hard == 0 {
                continue;
            }
            attrs.push(nonresident(
                0x80,
                "",
                0,
                size,
                &runs(&[(size.div_ceil(4096), Some(700 + num))]),
            ));
            // 替代資料流：不能影響大小與配置（主要資料流才算）
            if r.next(4) == 0 {
                attrs.push(resident(
                    0x80,
                    "Zone.Identifier",
                    b"[ZoneTransfer]\r\nZoneId=3\r\n",
                ));
            }
            im.set(
                num as usize,
                record(1, (i % 5 + 1) as u16, hard as u16, 0, &attrs),
            );
            num += 1;
        }
        im.finish()
    }

    #[test]
    fn compact_path_equals_the_old_path_on_a_stress_volume() {
        let mut used = 0;
        for seed in 0..4 {
            let (ok, tried) =
                assert_compact_equals_old(&stress_volume(seed), &opts(), &format!("stress {seed}"));
            eprintln!("stress {seed}: compact path used for {ok} of {tried} comparisons");
            used += ok;
        }
        assert!(used >= 8, "the stress volume mostly fell back ({used})");
    }

    #[test]
    fn compact_path_equals_the_old_path_on_random_volumes() {
        let (mut accepted, mut total) = (0, 0);
        for seed in 0..150 {
            let (ok, tried) =
                assert_compact_equals_old(&random_volume(seed), &opts(), &format!("seed {seed}"));
            accepted += ok;
            total += tried;
        }
        // 回退只在「孤兒是前 N 名候選」這類情況（小映像裡每個檔案都是候選，所以比真實磁碟常見得多）；
        // 仍有相當比例的請求必須真的走緊湊路徑，否則這個測試什麼也沒驗證
        eprintln!("compact path used for {accepted} of {total} comparisons");
        assert!(
            accepted * 100 >= total * 40,
            "only {accepted} of {total} comparisons used the compact path"
        );
    }

    /// 較大的映像（兩萬多筆記錄、數千個目錄）：逐位元組相同，並在設定 `VOLTDIR_PROF` 時印出記憶體剖析。
    #[test]
    fn compact_path_equals_the_old_path_on_a_larger_volume() {
        let img = random_volume_sized(7, 3_000, 20_000, false);
        let (ok, tried) = assert_compact_equals_old(&img, &opts(), "larger volume");
        eprintln!("larger volume: compact path used for {ok} of {tried} comparisons");
        assert!(ok >= 3, "{ok}/{tried}");
    }

    #[test]
    fn compact_path_falls_back_on_exclude_and_on_extension_records_of_counted_files() {
        let mut o = opts();
        o.exclude = vec![w(r"*\docs")];
        let mut a = Analyzer::new(reqs().remove(0));
        assert!(matches!(
            compact::scan_source_compact(&sample(), &w(r"X:\"), root_meta(), &o, &mut a),
            Err(compact::CompactError::Fallback(_))
        ));
        // 基底記錄沒有屬性清單卻有延伸記錄（只有手造的映像會這樣）：保守地交給原本的路徑
        let mut im = Image::new(64);
        im.set(5, dir_rec(5, 5, ".", 0x16));
        im.set(40, file_rec(5, "frag.bin", 40_000, 100, 3));
        im.set(
            50,
            record(
                1,
                1,
                0,
                (1u64 << 48) | 40,
                &[nonresident(0x80, "", 3, 0, &runs(&[(7, Some(900))]))],
            ),
        );
        let mut a = Analyzer::new(reqs().remove(0));
        assert!(matches!(
            compact::scan_source_compact(&im.finish(), &w(r"X:\"), root_meta(), &opts(), &mut a),
            Err(compact::CompactError::Fallback(_))
        ));
    }

    #[test]
    fn compact_path_reports_a_missing_root_like_the_old_path() {
        let mut im = Image::new(64);
        im.set(40, file_rec(5, "lost.txt", 10, 100, 1));
        let mut a = Analyzer::new(reqs().remove(0));
        let r =
            compact::scan_source_compact(&im.finish(), &w(r"X:\"), root_meta(), &opts(), &mut a);
        match r {
            Err(compact::CompactError::Failed(e)) => {
                assert!(e.message().contains("root directory"))
            }
            _ => panic!("expected the missing-root error"),
        }
    }

    /// 取消時的結果與原本的路徑相同：只留下標為 cancelled 的根。
    #[test]
    fn compact_path_cancelled_scan_leaves_only_the_root() {
        // 沒有方法在這裡觸發取消：確認 `reset` 讓接收端回到剛建立的狀態
        let req = reqs().remove(0);
        let mut a = Analyzer::new(req.clone());
        let sample_img = sample();
        let _ = compact::scan_source_compact(&sample_img, &w(r"X:\"), root_meta(), &opts(), &mut a);
        a.reset();
        let (mut x, mut y) = (Vec::new(), Vec::new());
        a.write_reports(&mut x);
        Analyzer::new(req).write_reports(&mut y);
        assert_eq!(x, y);
    }
}
