//! `walk` 引擎：多執行緒目錄走訪。每個工作單位是一個目錄；批次列舉、子項先在執行緒內排序。

use crate::dirbuf::{self, RawEntry};
use crate::glob;
use crate::model::*;
use crate::names::cmp_names;
use crate::sys::{self, AlignedBuf, DirHandle};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

pub(crate) const BATCH_BYTES: usize = 256 * 1024;
/// 內容存放在 MFT 記錄內的檔案不會超過一筆記錄（最大 4 KB）；超過就一定不是常駐。
const RESIDENT_MAX: u64 = 4096;
const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_PATH_NOT_FOUND: u32 = 3;
const ERROR_INVALID_DATA: u32 = 13;
/// 偵測到連結造成迴圈時記錄的錯誤碼（ERROR_CANT_RESOLVE_FILENAME）。
pub const ERROR_LINK_CYCLE: u32 = 1921;

/// NTFS 保留的中繼資料檔名（公開的 NTFS 格式文件所列），只在磁碟區根目錄下比對。
const RESERVED_NAMES: [&str; 11] = [
    "$MFT", "$MFTMIRR", "$LOGFILE", "$VOLUME", "$ATTRDEF", "$BITMAP", "$BOOT", "$BADCLUS",
    "$SECURE", "$UPCASE", "$EXTEND",
];

pub struct RootSpec {
    /// 顯示用 Win32 路徑（不含 `\\?\`）
    pub display: Vec<u16>,
}

/// 掃描選項（影響走訪本身者；輸出剪裁在 `snapshot`）。
#[derive(Default)]
pub struct Options {
    pub threads: usize,
    /// 不分大小寫的路徑萬用字元
    pub exclude: Vec<Vec<u16>>,
    /// `exclude` 的編譯版本，同時記錄每個樣式是否至少命中過一次；掃描結束後用來指出「什麼都沒命中」的樣式。
    /// 與 `exclude` 的樣式數不同（例如預設值是空的）時不使用，改用通用比對器。
    pub exclude_set: Arc<glob::ExcludeSet>,
    pub follow_mount: bool,
    pub follow_symlink: bool,
    pub follow_junction: bool,
    pub deadline: Option<Instant>,
}

impl Options {
    /// 建立排除樣式與它的編譯版本（含命中記錄）。
    pub fn with_exclude(exclude: Vec<Vec<u16>>) -> (Vec<Vec<u16>>, Arc<glob::ExcludeSet>) {
        let set = Arc::new(glob::ExcludeSet::new(&exclude));
        (exclude, set)
    }

    /// 這個路徑是否被任何 `--exclude` 樣式排除。
    pub(crate) fn is_excluded(&self, path: &[u16]) -> bool {
        if self.exclude.is_empty() {
            return false;
        }
        if self.exclude_set.len() != self.exclude.len() {
            return self.exclude.iter().any(|p| glob::matches(p, path));
        }
        self.exclude_set.is_match(path)
    }

    /// 掃描結束後，從頭到尾沒有命中任何項目的 `--exclude` 樣式。
    pub fn unmatched_excludes(&self) -> Vec<&[u16]> {
        if self.exclude_set.len() != self.exclude.len() {
            return Vec::new();
        }
        self.exclude_set
            .unmatched()
            .into_iter()
            .map(|i| self.exclude[i].as_slice())
            .collect()
    }

    pub(crate) fn follows_any(&self) -> bool {
        self.follow_mount || self.follow_symlink || self.follow_junction
    }

    fn follows(&self, r: Reparse) -> bool {
        match r {
            Reparse::Mount => self.follow_mount,
            Reparse::Symlink => self.follow_symlink,
            Reparse::Junction => self.follow_junction,
            _ => false,
        }
    }

    pub fn cancelled(&self) -> bool {
        sys::interrupted() || self.deadline.is_some_and(|d| Instant::now() >= d)
    }
}

/// 把 Win32 路徑轉成延伸長度路徑（`\\?\`）。
pub fn to_extended(display: &[u16]) -> Vec<u16> {
    let s = |x: &str| x.encode_utf16().collect::<Vec<u16>>();
    let prefix = s(r"\\?\");
    if display.starts_with(&prefix) {
        return display.to_vec();
    }
    let unc = s(r"\\");
    let mut out;
    if display.starts_with(&unc) {
        out = s(r"\\?\UNC\");
        out.extend_from_slice(&display[2..]);
    } else {
        out = prefix;
        out.extend_from_slice(display);
    }
    out
}

/// 延伸長度路徑轉回顯示用 Win32 路徑。
fn to_display(ext: &[u16]) -> Vec<u16> {
    let s = |x: &str| x.encode_utf16().collect::<Vec<u16>>();
    let unc = s(r"\\?\UNC\");
    if ext.starts_with(&unc) {
        let mut out = s(r"\\");
        out.extend_from_slice(&ext[unc.len()..]);
        return out;
    }
    let prefix = s(r"\\?\");
    if ext.starts_with(&prefix) {
        return ext[prefix.len()..].to_vec();
    }
    ext.to_vec()
}

pub fn is_volume_root(display: &[u16]) -> bool {
    display.len() == 3 && display[1] == u16::from(b':') && display[2] == u16::from(b'\\')
}

fn is_reserved_name(name: &RawEntry<'_>) -> bool {
    RESERVED_NAMES.iter().any(|r| {
        let upper: Vec<u16> = name
            .name_units()
            .map(|u| {
                if (0x61..=0x7A).contains(&u) {
                    u - 32
                } else {
                    u
                }
            })
            .collect();
        upper.iter().copied().eq(r.encode_utf16())
    })
}

pub(crate) struct SubOut {
    pub(crate) name: Box<[u16]>,
    pub(crate) attrs: u32,
    pub(crate) reparse: Reparse,
    pub(crate) reserved: bool,
    pub(crate) mtime: u64,
    pub(crate) id: u128,
    pub(crate) state: ScanState,
    pub(crate) error: Option<u32>,
    /// 是否要進入（建立工作）
    pub(crate) enter: bool,
    /// 經由連結進入（需以跟隨方式開啟）
    pub(crate) follow_open: bool,
    /// 進入後的磁碟區序號與目錄識別碼（迴圈偵測用）
    pub(crate) serial: u32,
    pub(crate) ident: (u32, u128),
}

pub(crate) struct EnumOut {
    pub(crate) names: Vec<u16>,
    pub(crate) files: Vec<FileEntry>,
    pub(crate) subs: Vec<SubOut>,
    pub(crate) state: ScanState,
    pub(crate) error: Option<u32>,
}

impl EnumOut {
    fn failed(state: ScanState, error: u32) -> EnumOut {
        EnumOut {
            names: Vec::new(),
            files: Vec::new(),
            subs: Vec::new(),
            state,
            error: Some(error),
        }
    }
}

pub(crate) fn join(dir: &[u16], name: &[u16]) -> Vec<u16> {
    let mut p = Vec::with_capacity(dir.len() + 1 + name.len());
    p.extend_from_slice(dir);
    if p.last() != Some(&u16::from(b'\\')) {
        p.push(u16::from(b'\\'));
    }
    p.extend_from_slice(name);
    p
}

fn classify_dir_reparse(attrs: u32, tag: u32, child_path: impl Fn() -> Vec<u16>) -> Reparse {
    if attrs & ATTR_REPARSE == 0 {
        return Reparse::None;
    }
    // 批次列舉對部分連結（如 C:\Documents and Settings）回報標籤 0，改以重解析資料補查
    let tag = if tag == 0 {
        sys::reparse_tag(&child_path()).unwrap_or(0)
    } else {
        tag
    };
    match tag {
        TAG_SYMLINK => Reparse::Symlink,
        TAG_MOUNT_POINT => {
            // 掛載點與接合點使用相同標籤，以替代名稱是否為 Volume GUID 區分
            let is_mount = sys::reparse_substitute_name(&child_path())
                .map(|n| {
                    let pre: Vec<u16> = r"\??\Volume{".encode_utf16().collect();
                    n.starts_with(&pre)
                })
                .unwrap_or(false);
            if is_mount {
                Reparse::Mount
            } else {
                Reparse::Junction
            }
        }
        _ => Reparse::Other,
    }
}

struct Ctx<'a> {
    opts: &'a Options,
    dir_path: &'a [u16],
    vol_root: bool,
    serial: u32,
    chain: &'a [(u32, u128)],
}

pub(crate) fn enumerate(task: &Task, opts: &Options, buf: &mut AlignedBuf) -> EnumOut {
    let handle = match DirHandle::open(&task.path, task.follow_open) {
        Ok(h) => h,
        Err(e) => {
            let state = if e == ERROR_FILE_NOT_FOUND || e == ERROR_PATH_NOT_FOUND {
                ScanState::Missing
            } else {
                ScanState::Unreadable
            };
            return EnumOut::failed(state, e);
        }
    };
    let ctx = Ctx {
        opts,
        dir_path: &task.path,
        vol_root: task.vol_root,
        serial: task.serial,
        chain: &task.chain,
    };
    let mut out = EnumOut {
        names: Vec::new(),
        files: Vec::new(),
        subs: Vec::new(),
        state: ScanState::Complete,
        error: None,
    };
    let mut first = true;
    loop {
        let n = match handle.read_batch(buf, first) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                out.state = if first {
                    ScanState::Unreadable
                } else {
                    ScanState::Partial
                };
                out.error = Some(e);
                break;
            }
        };
        first = false;
        let bytes = buf.as_bytes(n);
        let parsed = dirbuf::for_each_entry(bytes, |e| handle_entry(&mut out, e, &ctx));
        if parsed.is_err() {
            out.state = ScanState::Partial;
            out.error = Some(ERROR_INVALID_DATA);
            break;
        }
    }
    out.names.shrink_to_fit();
    out.files.shrink_to_fit();
    let names = &out.names;
    out.files.sort_unstable_by(|a, b| {
        let an = &names[a.name_off as usize..a.name_off as usize + a.name_len as usize];
        let bn = &names[b.name_off as usize..b.name_off as usize + b.name_len as usize];
        cmp_names(an, bn)
    });
    out.subs
        .sort_unstable_by(|a, b| cmp_names(&a.name, &b.name));
    out
}

fn excluded(opts: &Options, child_ext: &[u16]) -> bool {
    if opts.exclude.is_empty() {
        return false;
    }
    opts.is_excluded(&to_display(child_ext))
}

fn handle_entry(out: &mut EnumOut, e: RawEntry<'_>, cx: &Ctx<'_>) {
    let dot = u16::from(b'.');
    // 名稱放在堆疊緩衝區（絕大多數名稱不到 260 個單位），避免每個項目配置一次記憶體
    let mut name_buf = [0u16; 260];
    let name_heap: Vec<u16>;
    let n_units = e.name.len() / 2;
    let units: &[u16] = if n_units <= name_buf.len() {
        for (slot, c) in name_buf.iter_mut().zip(e.name.chunks_exact(2)) {
            *slot = u16::from_le_bytes([c[0], c[1]]);
        }
        &name_buf[..n_units]
    } else {
        name_heap = e.name_units().collect();
        &name_heap
    };
    if units == [dot] || units == [dot, dot] || units.is_empty() {
        return;
    }
    let reserved = cx.vol_root && is_reserved_name(&e);
    if e.attrs & ATTR_DIRECTORY != 0 {
        let child = join(cx.dir_path, units);
        let reparse = classify_dir_reparse(e.attrs, e.reparse_tag, || child.clone());
        let hidden_system_reparse = e.attrs & (ATTR_HIDDEN | ATTR_SYSTEM | ATTR_REPARSE)
            == (ATTR_HIDDEN | ATTR_SYSTEM | ATTR_REPARSE);
        let mut sub = SubOut {
            name: units.to_vec().into_boxed_slice(),
            attrs: e.attrs,
            reparse,
            reserved,
            mtime: e.mtime,
            id: e.id,
            state: ScanState::Pending,
            error: None,
            enter: true,
            follow_open: false,
            serial: cx.serial,
            ident: (cx.serial, e.id),
        };
        if excluded(cx.opts, &child) {
            sub.state = ScanState::Excluded;
            sub.enter = false;
        } else if hidden_system_reparse {
            // 隱藏＋系統＋重解析點：永遠不進入，不受 --follow-links 影響
            sub.state = ScanState::NotFollowed;
            sub.enter = false;
        } else if matches!(
            reparse,
            Reparse::Symlink | Reparse::Junction | Reparse::Mount
        ) {
            if cx.opts.follows(reparse) {
                match sys::file_info_follow(&child) {
                    Ok(fi) => {
                        let ident = (fi.serial, u128::from(fi.index));
                        if cx.chain.contains(&ident) {
                            // 連結指回自己的祖先：偵測到迴圈，不進入
                            sub.state = ScanState::NotFollowed;
                            sub.error = Some(ERROR_LINK_CYCLE);
                            sub.enter = false;
                        } else {
                            sub.follow_open = true;
                            sub.serial = fi.serial;
                            sub.ident = ident;
                        }
                    }
                    Err(code) => {
                        sub.state = ScanState::Unreadable;
                        sub.error = Some(code);
                        sub.enter = false;
                    }
                }
            } else {
                sub.state = ScanState::NotFollowed;
                sub.enter = false;
            }
        }
        out.subs.push(sub);
    } else {
        // 檔案符號連結預設省略
        if e.attrs & ATTR_REPARSE != 0 && e.reparse_tag == TAG_SYMLINK {
            return;
        }
        if !cx.opts.exclude.is_empty() && excluded(cx.opts, &join(cx.dir_path, units)) {
            return;
        }
        let off = out.names.len();
        if off > u32::MAX as usize - 1024 || units.len() > u16::MAX as usize {
            return;
        }
        // 稀疏、壓縮、其他重解析點（如 WOF 壓縮）、或列舉回報 0 但邏輯大小不為 0 的檔案：
        // 目錄列舉的配置大小不可靠，改以檔案資訊查詢（雲端佔位檔不查，避免觸發下載）
        let mut physical = e.physical;
        let flagged = e.attrs & (ATTR_SPARSE | ATTR_COMPRESSED | ATTR_REPARSE) != 0;
        let cloud =
            e.attrs & (ATTR_OFFLINE | ATTR_RECALL_ON_OPEN | ATTR_RECALL_ON_DATA_ACCESS) != 0;
        // 配置大小為 0 但有內容、又沒有任何特殊旗標的小檔案，內容存放在 MFT 記錄內（常駐）。
        // 作業系統對它回報「內容長度進位到 8 位元組」，不必為此開啟檔案。
        let resident =
            !flagged && !cloud && e.physical == 0 && e.logical > 0 && e.logical <= RESIDENT_MAX;
        if resident {
            physical = e.logical.div_ceil(8) * 8;
        }
        let sparse_or_compressed = flagged || (e.physical == 0 && e.logical > 0 && !resident);
        if sparse_or_compressed && !cloud {
            let path = join(cx.dir_path, units);
            physical = match sys::standard_allocation(&path) {
                Ok(a) if a > 0 || e.logical == 0 => a,
                // 配置大小為 0 但有內容：可能是過濾器壓縮（如 WOF），改問壓縮後大小
                _ => sys::allocation_size(&path).unwrap_or(physical),
            };
        }
        out.names.extend_from_slice(units);
        out.files.push(FileEntry {
            name_off: off as u32,
            name_len: units.len() as u16,
            reserved,
            secondary: false,
            reparse_other: e.attrs & ATTR_REPARSE != 0,
            attrs: e.attrs,
            logical: e.logical,
            physical,
            mtime: e.mtime,
            id: e.id,
            links: 0,
        });
    }
}

pub(crate) struct Task {
    pub(crate) id: u32,
    pub(crate) path: Vec<u16>,
    pub(crate) vol_root: bool,
    pub(crate) follow_open: bool,
    pub(crate) serial: u32,
    /// 祖先（含自己）的 (磁碟區序號, 識別碼)；只有啟用 --follow-links 時才維護
    pub(crate) chain: Vec<(u32, u128)>,
}

struct Pool<'a> {
    state: Mutex<PoolState>,
    cv: Condvar,
    store: Mutex<Vec<DirNode>>,
    opts: &'a Options,
}

struct PoolState {
    stack: Vec<Task>,
    active: usize,
}

fn worker(pool: &Pool<'_>) {
    let mut buf = AlignedBuf::new(BATCH_BYTES);
    loop {
        let task = {
            let mut st = pool.state.lock().unwrap();
            loop {
                if let Some(t) = st.stack.pop() {
                    st.active += 1;
                    break Some(t);
                }
                if st.active == 0 {
                    break None;
                }
                st = pool.cv.wait(st).unwrap();
            }
        };
        let Some(task) = task else {
            pool.cv.notify_all();
            return;
        };

        // 逾時或收到中斷：不再列舉，該目錄標為 cancelled
        if pool.opts.cancelled() {
            pool.store.lock().unwrap()[task.id as usize].state = ScanState::Cancelled;
            let mut st = pool.state.lock().unwrap();
            st.active -= 1;
            drop(st);
            pool.cv.notify_all();
            continue;
        }

        let out = enumerate(&task, pool.opts, &mut buf);

        let follow_any = pool.opts.follows_any();
        let mut new_tasks = Vec::new();
        {
            let mut store = pool.store.lock().unwrap();
            let root = store[task.id as usize].root;
            let mut ids = Vec::with_capacity(out.subs.len());
            for s in out.subs {
                let id = store.len() as u32;
                ids.push(id);
                if s.enter {
                    let chain = if follow_any {
                        let mut c = task.chain.clone();
                        c.push(s.ident);
                        c
                    } else {
                        Vec::new()
                    };
                    new_tasks.push(Task {
                        id,
                        path: join(&task.path, &s.name),
                        vol_root: false,
                        follow_open: s.follow_open,
                        serial: s.serial,
                        chain,
                    });
                }
                store.push(DirNode {
                    parent: task.id,
                    root,
                    name: s.name,
                    attrs: s.attrs,
                    reparse: s.reparse,
                    reserved: s.reserved,
                    mtime: s.mtime,
                    id: s.id,
                    serial: s.serial,
                    state: s.state,
                    error: s.error,
                    names: Vec::new(),
                    files: Vec::new(),
                    subdirs: Vec::new(),
                    direct: Totals::default(),
                    rollup: Totals::default(),
                    omitted: Totals::default(),
                    latest_mtime: 0,
                });
            }
            let node = &mut store[task.id as usize];
            node.names = out.names;
            node.files = out.files;
            node.subdirs = ids;
            node.state = out.state;
            node.error = out.error;
        }
        let mut st = pool.state.lock().unwrap();
        st.stack.extend(new_tasks);
        st.active -= 1;
        drop(st);
        pool.cv.notify_all();
    }
}

/// 掃描所有根，回傳整棵樹。
pub fn scan(roots: &[RootSpec], opts: &Options) -> ScanData {
    let mut store = Vec::new();
    let mut infos = Vec::new();
    let mut tasks = Vec::new();
    for (i, r) in roots.iter().enumerate() {
        let ext = to_extended(&r.display);
        let (attrs, mtime, serial, index, state, error) = match sys::file_info(&ext) {
            Ok(fi) => (
                fi.attrs,
                fi.mtime,
                fi.serial,
                u128::from(fi.index),
                ScanState::Pending,
                None,
            ),
            Err(e) => (ATTR_DIRECTORY, 0, 0, 0, ScanState::Unreadable, Some(e)),
        };
        let id = store.len() as u32;
        store.push(DirNode {
            parent: u32::MAX,
            root: i as u32,
            name: r.display.clone().into_boxed_slice(),
            attrs,
            reparse: Reparse::None,
            reserved: false,
            mtime,
            id: index,
            serial,
            state,
            error,
            names: Vec::new(),
            files: Vec::new(),
            subdirs: Vec::new(),
            direct: Totals::default(),
            rollup: Totals::default(),
            omitted: Totals::default(),
            latest_mtime: 0,
        });
        infos.push(RootInfo {
            display: r.display.clone(),
            node: id,
        });
        if error.is_none() {
            tasks.push(Task {
                id,
                path: ext,
                vol_root: is_volume_root(&r.display),
                follow_open: false,
                serial,
                chain: if opts.follows_any() {
                    vec![(serial, index)]
                } else {
                    Vec::new()
                },
            });
        }
    }
    let pool = Pool {
        state: Mutex::new(PoolState {
            stack: tasks,
            active: 0,
        }),
        cv: Condvar::new(),
        store: Mutex::new(store),
        opts,
    };
    std::thread::scope(|s| {
        for _ in 0..opts.threads.max(1) {
            s.spawn(|| worker(&pool));
        }
    });
    let mut dirs = pool.store.into_inner().unwrap();
    for d in &mut dirs {
        if d.state == ScanState::Pending {
            d.state = ScanState::Complete;
        }
    }
    ScanData { dirs, roots: infos }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn extended_path_forms() {
        assert_eq!(to_extended(&w(r"C:\a b")), w(r"\\?\C:\a b"));
        assert_eq!(to_extended(&w(r"\\srv\share\x")), w(r"\\?\UNC\srv\share\x"));
        assert_eq!(to_extended(&w(r"\\?\C:\x")), w(r"\\?\C:\x"));
    }

    #[test]
    fn display_path_forms() {
        assert_eq!(to_display(&w(r"\\?\C:\a b")), w(r"C:\a b"));
        assert_eq!(to_display(&w(r"\\?\UNC\srv\share\x")), w(r"\\srv\share\x"));
        assert_eq!(to_display(&w(r"C:\x")), w(r"C:\x"));
    }

    #[test]
    fn volume_root_detection() {
        assert!(is_volume_root(&w(r"C:\")));
        assert!(!is_volume_root(&w(r"C:\Users")));
        assert!(!is_volume_root(&w(r"\\srv\share")));
    }

    #[test]
    fn join_does_not_double_separator() {
        assert_eq!(join(&w(r"\\?\C:\"), &w("a")), w(r"\\?\C:\a"));
        assert_eq!(join(&w(r"\\?\C:\x"), &w("a")), w(r"\\?\C:\x\a"));
    }

    #[test]
    fn exclusion_matches_display_path_case_insensitively() {
        let opts = Options {
            exclude: vec![w(r"*\NODE_MODULES"), w(r"*.tmp")],
            ..Default::default()
        };
        assert!(excluded(&opts, &w(r"\\?\C:\proj\node_modules")));
        assert!(excluded(&opts, &w(r"\\?\C:\proj\a.TMP")));
        assert!(!excluded(&opts, &w(r"\\?\C:\proj\src")));
        assert!(!excluded(&Options::default(), &w(r"\\?\C:\proj\src")));
    }
}
