//! 串流彙總掃描：走訪時不保留逐筆項目。
//!
//! 工作執行緒照 `walk` 的方式列舉目錄，但列舉完的結果不放進整棵樹，而是交給「提交器」：
//! 提交器依**標準順序**（前序；同一目錄下的檔案與子目錄依名稱混合排序，與 `snapshot::traverse` 相同）
//! 逐一取用已完成的目錄，把檔案折進目錄合計、交給接收端，然後丟棄檔案清單。
//! 提交順序固定，所以硬連結的「主要名稱」仍是標準順序中第一個遇到者，數字與先掃描成快照再彙總完全相同。
//!
//! 提交器不佔用工作執行緒的鎖：登記（短）與提交（長）分開，任何一個剛登記完的執行緒用 `try_lock` 當提交器，
//! 其餘執行緒繼續列舉。
//!
//! 記憶體：每個目錄保留一筆小記錄（名稱、序號）；完成但尚未輪到提交的目錄暫存其檔案清單，
//! 總量有上限（`PENDING_BUDGET`）：超過時工作執行緒只領取提交器正在等的目錄；硬連結已見集合見 `SeenSet`。

use crate::fxhash::{FxHashMap, FxHashSet};
use crate::model::*;
use crate::names::cmp_names;
use crate::report::{DirView, FileView, Sink};
use crate::snapshot::Coverage;
use crate::sys::{self, AlignedBuf};
use crate::walk::{
    BATCH_BYTES, EnumOut, Options, RootSpec, Task, enumerate, is_volume_root, join, to_extended,
};
use std::cmp::Ordering;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering as AO};
use std::sync::{Condvar, Mutex};

const SEP: u16 = b'\\' as u16;
/// 位元圖的一頁涵蓋的識別碼數與頁數上限（超過就退回精確的雜湊集合）。
const PAGE_IDS: u64 = 4096;
const MAX_PAGES: usize = 16_384;
/// 已列舉完、還在等待提交的檔案數上限：超過時工作執行緒只領取「提交器正在等的那個目錄」，
/// 讓暫存量有上限（約 65,536 個檔案，數 MB），不會隨執行緒數或樹的形狀無限成長。
const PENDING_BUDGET: usize = 65_536;
/// 框架尚未開始處理的標記。
const NOT_STARTED: usize = usize::MAX;

/// 硬連結「已見過的檔案識別碼」集合（精確，不是近似）。
///
/// 常見情況（識別碼不超過 64 位元）以位元圖記錄：每 4096 個連續識別碼一頁（512 位元組），依磁碟區序號分開。
/// NTFS 的識別碼是連續配置的 MFT 位置，所以 100 萬個檔案只需要幾百頁。頁數超過 `MAX_PAGES`
/// 或識別碼超過 64 位元（ReFS）時，其餘登記改放精確的雜湊集合（每筆約 48 位元組）。
#[derive(Default)]
struct SeenSet {
    index: FxHashMap<(u32, u64), usize>,
    pages: Vec<[u64; 64]>,
    /// 最近使用的頁：同一個目錄的檔案識別碼通常相鄰
    last: Option<((u32, u64), usize)>,
    exact: FxHashSet<(u32, u128)>,
}

impl SeenSet {
    /// 登記；回傳 true 表示先前沒見過（是第一個名稱）。
    fn insert(&mut self, serial: u32, id: u128) -> bool {
        if let Ok(id64) = u64::try_from(id) {
            let key = (serial, id64 / PAGE_IDS);
            let bit = id64 % PAGE_IDS;
            let (word, mask) = ((bit / 64) as usize, 1u64 << (bit % 64));
            let slot = match self.last {
                Some((k, i)) if k == key => Some(i),
                _ => self.index.get(&key).copied(),
            };
            let slot = match slot {
                Some(i) => Some(i),
                None if self.pages.len() < MAX_PAGES => {
                    self.pages.push([0u64; 64]);
                    self.index.insert(key, self.pages.len() - 1);
                    Some(self.pages.len() - 1)
                }
                None => None,
            };
            if let Some(i) = slot {
                self.last = Some((key, i));
                let page = &mut self.pages[i];
                let seen = page[word] & mask != 0;
                page[word] |= mask;
                return !seen;
            }
        }
        self.exact.insert((serial, id))
    }
}

/// 一個目錄的結構記錄（不含檔案、不含合計）。工作執行緒登記、提交器讀取。
struct Skel {
    first_child: u32,
    n_children: u32,
    serial: u32,
    /// 目錄自己的修改時間（滾算最新修改時間的起點）
    mtime: u64,
    name: Box<[u16]>,
    state: ScanState,
    error: Option<u32>,
    /// 列舉結果已登記（或根本不需要列舉）
    ready: bool,
}

struct Batch {
    names: Vec<u16>,
    files: Vec<FileEntry>,
}

/// 工作執行緒與提交器共用的部分：登記列舉結果用，持鎖時間很短。
struct Reg {
    skel: Vec<Skel>,
    batches: FxHashMap<u32, Batch>,
}

/// 提交器正在處理的一個目錄。堆疊上只有從根到目前目錄的這一條路徑，所以合計不必每個目錄各留一份。
struct Frame {
    dir: u32,
    /// 下一個檔案的序號；`NOT_STARTED` 表示還沒取過列舉結果
    fi: usize,
    /// 下一個子目錄的序號
    si: u32,
    depth: u32,
    serial: u32,
    /// 第一個子目錄的編號（子目錄編號連續，依名稱排序）
    first_child: u32,
    /// 此目錄的檔案清單與子目錄名稱（開始提交時從共用區取出）
    batch: Option<Batch>,
    kids: Vec<Box<[u16]>>,
    direct: Totals,
    rollup: Totals,
    latest: u64,
}

/// 掃描結果的摘要（取代 `ScanData` 上的各種彙總）。
pub struct Summary {
    /// 對應 `snapshot::scan_totals`
    pub totals: Totals,
    pub coverage: Coverage,
    pub cancelled: u64,
    pub dirs: u64,
    pub files: u64,
}

/// 提交器的私有狀態；同一時間只有一個執行緒在提交（`try_lock`）。
struct Commit<'s> {
    stack: Vec<Frame>,
    roots: Vec<u32>,
    next_root: usize,
    /// 目前堆疊頂端目錄的路徑（Win32 顯示形式）與各層長度
    path: Vec<u16>,
    lens: Vec<usize>,
    scratch: Vec<u16>,
    ext: Vec<u16>,
    seen: SeenSet,
    dedupe: bool,
    coverage: Coverage,
    cancelled: u64,
    files: u64,
    dirs: u64,
    root_totals: Totals,
    displays: Vec<Vec<u16>>,
    /// 提交器正在等的目錄（沒有則 u32::MAX），與暫存中的檔案數；工作執行緒據此決定要不要領取新任務
    need: &'s AtomicU32,
    pending: &'s AtomicUsize,
    sink: &'s mut (dyn Sink + Send),
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

fn push_sep_name(path: &mut Vec<u16>, name: &[u16]) {
    if path.last() != Some(&SEP) {
        path.push(SEP);
    }
    path.extend_from_slice(name);
}

fn new_frame(dir: u32, depth: u32) -> Frame {
    Frame {
        dir,
        fi: NOT_STARTED,
        si: 0,
        depth,
        serial: 0,
        first_child: 0,
        batch: None,
        kids: Vec::new(),
        direct: Totals::default(),
        rollup: Totals::default(),
        latest: 0,
    }
}

impl Commit<'_> {
    /// 依標準順序盡量往前提交；遇到尚未列舉完的目錄就停下。
    fn run(&mut self, reg: &Mutex<Reg>) {
        loop {
            if self.stack.is_empty() {
                if self.next_root >= self.roots.len() {
                    self.need.store(u32::MAX, AO::Relaxed);
                    return;
                }
                let r = self.roots[self.next_root];
                self.path.clear();
                self.path.extend_from_slice(&self.displays[self.next_root]);
                self.next_root += 1;
                self.lens.clear();
                self.lens.push(self.path.len());
                self.stack.push(new_frame(r, 0));
            }
            let mut fr = self.stack.pop().expect("non-empty");
            if fr.fi == NOT_STARTED {
                // 第一次處理這個目錄：需要它的列舉結果
                let mut g = reg.lock().unwrap();
                let n = &mut g.skel[fr.dir as usize];
                if !n.ready {
                    drop(g);
                    self.need.store(fr.dir, AO::Relaxed);
                    self.stack.push(fr);
                    return;
                }
                let state = if n.state == ScanState::Pending {
                    ScanState::Complete
                } else {
                    n.state
                };
                let (first, count, error) = (n.first_child, n.n_children, n.error);
                fr.serial = n.serial;
                fr.latest = n.mtime;
                fr.first_child = first;
                fr.kids = (first..first + count)
                    .map(|s| std::mem::take(&mut g.skel[s as usize].name))
                    .collect();
                fr.batch = g.batches.remove(&fr.dir);
                drop(g);
                fr.fi = 0;
                self.dirs += 1;
                if state == ScanState::Cancelled {
                    self.cancelled += 1;
                }
                self.coverage.note(state, error, &self.path);
            }
            // 這個目錄裡盡量往前：檔案與子目錄依名稱混合，名稱相同時檔案在前
            loop {
                let file = fr.batch.as_ref().and_then(|b| b.files.get(fr.fi).copied());
                let sub = fr.kids.get(fr.si as usize);
                let take_file = match (file, sub) {
                    (None, None) => {
                        self.finish_dir(fr);
                        break;
                    }
                    (Some(_), None) => true,
                    (None, Some(_)) => false,
                    (Some(fe), Some(sub_name)) => {
                        let b = fr.batch.as_ref().expect("file implies batch");
                        let name = &b.names
                            [fe.name_off as usize..fe.name_off as usize + fe.name_len as usize];
                        cmp_names(name, sub_name) != Ordering::Greater
                    }
                };
                if take_file {
                    let fe = file.expect("checked");
                    {
                        let b = fr.batch.as_ref().expect("file implies batch");
                        let name = &b.names
                            [fe.name_off as usize..fe.name_off as usize + fe.name_len as usize];
                        self.scratch.clear();
                        self.scratch.extend_from_slice(&self.path);
                        push_sep_name(&mut self.scratch, name);
                        lower_ext_into(name, &mut self.ext);
                    }
                    self.fold_file(&mut fr, fe);
                    fr.fi += 1;
                } else {
                    let idx = fr.si;
                    fr.si += 1;
                    let child_id = fr.first_child + idx;
                    let name = std::mem::take(&mut fr.kids[idx as usize]);
                    let depth = fr.depth + 1;
                    self.stack.push(fr);
                    push_sep_name(&mut self.path, &name);
                    self.lens.push(self.path.len());
                    self.stack.push(new_frame(child_id, depth));
                    break;
                }
            }
        }
    }

    fn fold_file(&mut self, fr: &mut Frame, fe: FileEntry) {
        let secondary = self.dedupe && fe.id != 0 && !self.seen.insert(fr.serial, fe.id);
        let contribution = if secondary { 0 } else { fe.physical };
        fr.direct.files += 1;
        fr.direct.logical += fe.logical;
        fr.direct.physical += contribution;
        fr.latest = fr.latest.max(fe.mtime);
        self.files += 1;
        self.sink.file(&FileView {
            path: &self.scratch,
            ext: &self.ext,
            logical: fe.logical,
            physical: fe.physical,
            mtime: fe.mtime,
            secondary,
        });
    }

    /// 目錄的所有項目都提交完了：算出滾算值、交給接收端、折進父層。
    fn finish_dir(&mut self, mut fr: Frame) {
        if let Some(b) = &fr.batch {
            self.pending.fetch_sub(b.files.len(), AO::Relaxed);
        }
        fr.rollup.files += fr.direct.files;
        fr.rollup.logical += fr.direct.logical;
        fr.rollup.physical += fr.direct.physical;
        self.sink.dir(&DirView {
            path: &self.path,
            depth: u64::from(fr.depth),
            rollup: fr.rollup,
            direct: fr.direct,
            latest_mtime: fr.latest,
        });
        let r = fr.rollup;
        if let Some(p) = self.stack.last_mut() {
            p.rollup.files += r.files;
            p.rollup.dirs += 1 + r.dirs;
            p.rollup.logical += r.logical;
            p.rollup.physical += r.physical;
            p.latest = p.latest.max(fr.latest);
        } else {
            self.root_totals.files += r.files;
            self.root_totals.logical += r.logical;
            self.root_totals.physical += r.physical;
        }
        self.lens.pop();
        if let Some(&l) = self.lens.last() {
            self.path.truncate(l);
        }
    }
}

struct Pool<'a, 's> {
    pending: &'s AtomicUsize,
    need: &'s AtomicU32,
    /// 有新的列舉結果登記、提交器可能能往前走
    dirty: AtomicBool,
    state: Mutex<PoolState>,
    cv: Condvar,
    reg: Mutex<Reg>,
    com: Mutex<Commit<'s>>,
    opts: &'a Options,
}

struct PoolState {
    stack: Vec<Task>,
    active: usize,
    /// 正在 `cv.wait` 的工作執行緒數：只叫醒需要的數量，避免每次登記都驚動所有等待者
    waiting: usize,
}

impl Pool<'_, '_> {
    /// 一個任務結束：有新任務時只叫醒對應數量的等待者；全部做完時叫醒所有人離開。
    fn finish_task(&self, n_new: usize) {
        let mut st = self.state.lock().unwrap();
        st.active -= 1;
        let done = st.active == 0 && st.stack.is_empty();
        let wake = n_new.min(st.waiting);
        drop(st);
        if done {
            self.cv.notify_all();
        } else {
            for _ in 0..wake {
                self.cv.notify_one();
            }
        }
    }

    /// 試著由目前的執行緒當提交器；別的執行緒正在提交時直接返回（它結束前會再檢查 `dirty`）。
    fn try_commit(&self) {
        loop {
            let Ok(mut c) = self.com.try_lock() else {
                return;
            };
            loop {
                self.dirty.store(false, AO::SeqCst);
                c.run(&self.reg);
                if !self.dirty.load(AO::SeqCst) {
                    break;
                }
            }
            drop(c);
            // 暫存量或等待的目錄改變了：叫醒可能在等的工作執行緒（先過一次狀態鎖，避免漏掉喚醒）
            let waiting = self.state.lock().unwrap().waiting;
            if waiting > 0 {
                self.cv.notify_one();
            }
            if !self.dirty.load(AO::SeqCst) {
                return;
            }
        }
    }
}

fn worker(pool: &Pool<'_, '_>) {
    let mut buf = AlignedBuf::new(BATCH_BYTES);
    loop {
        let task = {
            let mut st = pool.state.lock().unwrap();
            loop {
                let over = pool.pending.load(AO::Relaxed) >= PENDING_BUDGET;
                let pick = if over {
                    // 暫存太多：只領取提交器正在等的目錄，其餘等提交器消化
                    let need = pool.need.load(AO::Relaxed);
                    st.stack
                        .iter()
                        .rposition(|t| t.id == need)
                        .map(|i| st.stack.remove(i))
                } else {
                    st.stack.pop()
                };
                if let Some(t) = pick {
                    st.active += 1;
                    break Some(t);
                }
                if st.active == 0 && st.stack.is_empty() {
                    break None;
                }
                st.waiting += 1;
                st = pool.cv.wait(st).unwrap();
                st.waiting -= 1;
            }
        };
        let Some(task) = task else {
            pool.cv.notify_all();
            return;
        };

        // 逾時或收到中斷：不再列舉，該目錄標為 cancelled
        if pool.opts.cancelled() {
            {
                let mut g = pool.reg.lock().unwrap();
                let n = &mut g.skel[task.id as usize];
                n.state = ScanState::Cancelled;
                n.ready = true;
            }
            pool.dirty.store(true, AO::SeqCst);
            pool.finish_task(0);
            pool.try_commit();
            continue;
        }

        let out: EnumOut = enumerate(&task, pool.opts, &mut buf);

        let follow_any = pool.opts.follows_any();
        let mut new_tasks = Vec::new();
        {
            let mut g = pool.reg.lock().unwrap();
            let first_child = g.skel.len() as u32;
            let n_children = out.subs.len() as u32;
            for s in out.subs {
                let id = g.skel.len() as u32;
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
                g.skel.push(Skel {
                    first_child: 0,
                    n_children: 0,
                    serial: s.serial,
                    mtime: s.mtime,
                    name: s.name,
                    state: s.state,
                    error: s.error,
                    ready: !s.enter,
                });
            }
            {
                let n = &mut g.skel[task.id as usize];
                n.first_child = first_child;
                n.n_children = n_children;
                n.state = out.state;
                n.error = out.error;
                n.ready = true;
            }
            if !out.files.is_empty() {
                pool.pending.fetch_add(out.files.len(), AO::Relaxed);
                g.batches.insert(
                    task.id,
                    Batch {
                        names: out.names,
                        files: out.files,
                    },
                );
            }
        }
        pool.dirty.store(true, AO::SeqCst);
        // 反向推入：堆疊從尾端取，所以名稱最小的子目錄最先被走訪，與提交順序（前序）一致
        let n_new = new_tasks.len();
        pool.state
            .lock()
            .unwrap()
            .stack
            .extend(new_tasks.into_iter().rev());
        pool.finish_task(n_new);
        pool.try_commit();
    }
}

/// 串流掃描所有根；每個項目依標準順序交給 `sink`。
pub fn scan(
    roots: &[RootSpec],
    opts: &Options,
    dedupe_hardlinks: bool,
    sink: &mut (dyn Sink + Send),
) -> Summary {
    let mut skel = Vec::new();
    let mut root_ids = Vec::new();
    let mut displays = Vec::new();
    let mut tasks = Vec::new();
    for r in roots {
        let ext = to_extended(&r.display);
        let (mtime, serial, index, state, error) = match sys::file_info(&ext) {
            Ok(fi) => (
                fi.mtime,
                fi.serial,
                u128::from(fi.index),
                ScanState::Pending,
                None,
            ),
            Err(e) => (0, 0, 0, ScanState::Unreadable, Some(e)),
        };
        let id = skel.len() as u32;
        skel.push(Skel {
            first_child: 0,
            n_children: 0,
            serial,
            mtime,
            name: r.display.clone().into_boxed_slice(),
            state,
            error,
            ready: error.is_some(),
        });
        root_ids.push(id);
        displays.push(r.display.clone());
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
    let need = AtomicU32::new(u32::MAX);
    let pending = AtomicUsize::new(0);
    let pool = Pool {
        pending: &pending,
        need: &need,
        dirty: AtomicBool::new(true),
        state: Mutex::new(PoolState {
            stack: tasks.into_iter().rev().collect(),
            active: 0,
            waiting: 0,
        }),
        cv: Condvar::new(),
        reg: Mutex::new(Reg {
            skel,
            batches: FxHashMap::default(),
        }),
        com: Mutex::new(Commit {
            stack: Vec::new(),
            roots: root_ids,
            next_root: 0,
            path: Vec::new(),
            lens: Vec::new(),
            scratch: Vec::new(),
            ext: Vec::new(),
            seen: SeenSet::default(),
            dedupe: dedupe_hardlinks,
            coverage: Coverage::default(),
            cancelled: 0,
            files: 0,
            dirs: 0,
            root_totals: Totals::default(),
            displays,
            need: &need,
            pending: &pending,
            sink,
        }),
        opts,
    };
    // 開頭就失敗的根（沒有任務）也要能被提交
    pool.try_commit();
    std::thread::scope(|s| {
        for _ in 0..opts.threads.max(1) {
            s.spawn(|| worker(&pool));
        }
    });
    let mut c = pool.com.into_inner().unwrap();
    c.run(&pool.reg);
    let mut totals = c.root_totals;
    totals.dirs = c.dirs;
    Summary {
        totals,
        coverage: c.coverage,
        cancelled: c.cancelled,
        dirs: c.dirs,
        files: c.files,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seen_set_is_exact_across_pages_serials_and_wide_ids() {
        let mut s = SeenSet::default();
        assert!(s.insert(1, 100));
        assert!(!s.insert(1, 100), "second sighting of the same id");
        assert!(
            s.insert(2, 100),
            "same id on another volume is a different file"
        );
        assert!(s.insert(1, 100 + PAGE_IDS as u128), "next page");
        assert!(s.insert(1, 101));
        assert!(!s.insert(1, 101));
        // 超過 64 位元的識別碼（ReFS）走精確集合
        let wide = (1u128 << 100) | 7;
        assert!(s.insert(1, wide));
        assert!(!s.insert(1, wide));
        assert!(s.insert(1, wide + 1));
    }

    #[test]
    fn seen_set_falls_back_to_exact_when_pages_run_out() {
        let mut s = SeenSet::default();
        for p in 0..MAX_PAGES as u64 {
            assert!(s.insert(1, u128::from(p * PAGE_IDS)));
        }
        assert_eq!(s.pages.len(), MAX_PAGES);
        let beyond = u128::from(MAX_PAGES as u64 * PAGE_IDS + 5);
        assert!(s.insert(1, beyond));
        assert!(!s.insert(1, beyond), "exact fallback still detects repeats");
        // 已有頁的識別碼仍走位元圖
        assert!(!s.insert(1, 0));
    }
}
