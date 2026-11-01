//! `diff`：比較兩份快照，回答「從 A 到 B 多了什麼、少了什麼」。
//!
//! 兩份快照都是標準順序（前序、同層依名稱排序），所以用「同步走訪」配對，不需要把整份快照
//! 讀進記憶體：每個根只保留幾個固定大小的前 N 名容器。只讀快照檔，不掃描、不讀被掃描檔案的內容。

use crate::glob;
use crate::json;
use crate::names::{cmp_names, fold};
use crate::report::{TopK, rank};
use crate::snapread::{Parsed, Reader, Rec};
use std::cmp::Ordering;
use std::io::{BufRead, Write};

pub const SCHEMA: &str = "voltdir.diff/1";
const SEP: u16 = b'\\' as u16;

pub struct Options {
    pub top: usize,
    pub min_change: u64,
    pub under: Option<Vec<u16>>,
    pub rank_rollup: bool,
    /// 跨路徑模式的選項；三者都沒給時沿用原本的重疊根比較。
    pub a_root: Option<Vec<u16>>,
    pub b_root: Option<Vec<u16>>,
    pub match_roots: bool,
}

/// 清單中的一個項目；`key` 是排序用的量（變化量的絕對值，或新增／消失的大小）。
struct Ranked {
    key: u64,
    path: Vec<u16>,
    a: u64,
    b: u64,
    /// 目錄：`direct.physical` 的變化量
    ddirect: i128,
    logical: u64,
}

impl PartialEq for Ranked {
    fn eq(&self, o: &Self) -> bool {
        self.cmp(o) == Ordering::Equal
    }
}
impl Eq for Ranked {}
impl PartialOrd for Ranked {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Ranked {
    fn cmp(&self, o: &Self) -> Ordering {
        self.key
            .cmp(&o.key)
            .then_with(|| cmp_names(&o.path, &self.path))
    }
}

struct Lists {
    growth_dirs: TopK<Ranked>,
    shrink_dirs: TopK<Ranked>,
    growth_files: TopK<Ranked>,
    shrink_files: TopK<Ranked>,
    added_dirs: TopK<Ranked>,
    added_files: TopK<Ranked>,
    removed_dirs: TopK<Ranked>,
    removed_files: TopK<Ranked>,
}

#[derive(Default)]
struct Totals {
    a_physical: u128,
    b_physical: u128,
    a_logical: u128,
    b_logical: u128,
}

pub struct Outcome {
    pub a: Parsed,
    pub b: Parsed,
    pub overlap: Vec<Vec<u16>>,
    /// 跨路徑模式的比較根（A、B）；有值時輸出相對路徑
    pub compare_roots: Option<CompareRoots>,
    lists: Lists,
    totals: Totals,
    /// 有重疊根的目錄那一行不在快照裡（被 `--max-depth` 剪掉）而無法計算合計
    totals_incomplete: bool,
    pub limited_reasons: Vec<String>,
}

// ---- 路徑比較（不分大小寫、逐段、前序）----

fn components(p: &[u16]) -> impl Iterator<Item = &[u16]> {
    let mut end = p.len();
    while end > 1 && p[end - 1] == SEP {
        end -= 1;
    }
    p[..end].split(|&u| u == SEP)
}

fn cmp_fold_slice(a: &[u16], b: &[u16]) -> Ordering {
    a.iter().map(|&u| fold(u)).cmp(b.iter().map(|&u| fold(u)))
}

/// 前序順序下的比較：逐段不分大小寫比較，一個路徑是另一個的前綴時，較短者在前。
pub fn cmp_path(a: &[u16], b: &[u16]) -> Ordering {
    let mut ia = components(a);
    let mut ib = components(b);
    loop {
        match (ia.next(), ib.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => match cmp_fold_slice(x, y) {
                Ordering::Equal => {}
                o => return o,
            },
        }
    }
}

/// `path` 是否等於 `root` 或位於其底下（不分大小寫）。
pub fn under(root: &[u16], path: &[u16]) -> bool {
    let mut ir = components(root);
    let mut ip = components(path);
    loop {
        match (ir.next(), ip.next()) {
            (None, _) => return true,
            (Some(_), None) => return false,
            (Some(r), Some(p)) => {
                if cmp_fold_slice(r, p) != Ordering::Equal {
                    return false;
                }
            }
        }
    }
}

/// 兩邊根的重疊範圍：一個根包含另一個時，取較深的那個。
pub fn overlap_roots(a: &[Vec<u16>], b: &[Vec<u16>]) -> Vec<Vec<u16>> {
    let mut out: Vec<Vec<u16>> = Vec::new();
    for ra in a {
        for rb in b {
            let pick = if under(ra, rb) {
                Some(rb)
            } else if under(rb, ra) {
                Some(ra)
            } else {
                None
            };
            if let Some(r) = pick
                && !out.iter().any(|o| cmp_path(o, r) == Ordering::Equal)
            {
                out.push(r.clone());
            }
        }
    }
    out
}

// ---- 同步走訪 ----

struct Scoped<'r, R: BufRead> {
    reader: Reader<'r, R>,
    root: Vec<u16>,
    prev: Vec<u16>,
    seen: bool,
    /// 跨路徑模式：把比較根之前的部分切掉，路徑變成相對路徑（比較根本身為空路徑）
    rebase: bool,
    label: &'static str,
}

/// 去掉 `root` 這個前綴與緊接的分隔符。`under()` 已確認 `path` 在 `root` 之下，
/// 而摺疊大小寫是逐 UTF-16 單元，所以前綴長度相同。
fn relative_to(root: &[u16], path: &mut Vec<u16>) {
    let mut n = root.len();
    while n > 0 && root[n - 1] == SEP {
        n -= 1;
    }
    let mut cut = n.min(path.len());
    while cut < path.len() && path[cut] == SEP {
        cut += 1;
    }
    path.drain(..cut);
}

impl<R: BufRead> Scoped<'_, R> {
    /// 下一個在範圍內的項目；順序不是標準順序時回報錯誤（此時配對不可靠）。
    fn next(&mut self, rec: &mut Rec) -> Result<bool, String> {
        loop {
            if !self.reader.next_into(rec)? {
                return Ok(false);
            }
            if !under(&self.root, &rec.path) {
                continue;
            }
            if self.seen && cmp_path(&self.prev, &rec.path) == Ordering::Greater {
                return Err(format!(
                    "snapshot {} {MSG_NOT_CANONICAL} (entry '{}' comes after '{}'); it was not produced by voltdir scan",
                    self.label,
                    String::from_utf16_lossy(&rec.path),
                    String::from_utf16_lossy(&self.prev)
                ));
            }
            self.seen = true;
            self.prev.clear();
            self.prev.extend_from_slice(&rec.path);
            if self.rebase {
                relative_to(&self.root, &mut rec.path);
            }
            return Ok(true);
        }
    }
}

fn contribution(r: &Rec) -> u64 {
    if r.secondary { 0 } else { r.physical }
}

fn mag(d: i128) -> u64 {
    u64::try_from(d.unsigned_abs()).unwrap_or(u64::MAX)
}

struct Ctx<'o> {
    opt: &'o Options,
    lists: Lists,
    totals: Totals,
    totals_incomplete: bool,
}

impl Ctx<'_> {
    fn in_scope(&self, path: &[u16]) -> bool {
        self.opt
            .under
            .as_ref()
            .is_none_or(|g| glob::matches(g, path))
    }

    fn offer(list: &mut TopK<Ranked>, item: Ranked) {
        let (key, path) = (item.key, item.path.clone());
        list.offer(|w| rank(key, &path)(w.key, &w.path), || item);
    }

    fn only_in(&mut self, r: &Rec, added: bool) {
        let key = if r.is_dir {
            r.rollup.physical
        } else {
            contribution(r)
        };
        if r.path.is_empty() || key < self.opt.min_change || !self.in_scope(&r.path) {
            return;
        }
        let item = Ranked {
            key,
            path: r.path.clone(),
            a: 0,
            b: 0,
            ddirect: 0,
            logical: if r.is_dir {
                r.rollup.logical
            } else {
                r.logical
            },
        };
        let list = match (r.is_dir, added) {
            (true, true) => &mut self.lists.added_dirs,
            (false, true) => &mut self.lists.added_files,
            (true, false) => &mut self.lists.removed_dirs,
            (false, false) => &mut self.lists.removed_files,
        };
        Self::offer(list, item);
    }

    fn pair(&mut self, a: &Rec, b: &Rec) {
        // 跨路徑模式的比較根本身（空路徑）不列為項目
        if b.path.is_empty() || !self.in_scope(&b.path) {
            return;
        }
        if a.is_dir {
            let d_rollup = i128::from(b.rollup.physical) - i128::from(a.rollup.physical);
            let d_direct = i128::from(b.direct.physical) - i128::from(a.direct.physical);
            let d = if self.opt.rank_rollup {
                d_rollup
            } else {
                d_direct
            };
            if d == 0 || mag(d) < self.opt.min_change {
                return;
            }
            let item = Ranked {
                key: mag(d),
                path: b.path.clone(),
                a: a.rollup.physical,
                b: b.rollup.physical,
                ddirect: d_direct,
                logical: 0,
            };
            let list = if d > 0 {
                &mut self.lists.growth_dirs
            } else {
                &mut self.lists.shrink_dirs
            };
            Self::offer(list, item);
        } else {
            let (pa, pb) = (contribution(a), contribution(b));
            let d = i128::from(pb) - i128::from(pa);
            if d == 0 || mag(d) < self.opt.min_change {
                return;
            }
            let item = Ranked {
                key: mag(d),
                path: b.path.clone(),
                a: pa,
                b: pb,
                ddirect: 0,
                logical: 0,
            };
            let list = if d > 0 {
                &mut self.lists.growth_files
            } else {
                &mut self.lists.shrink_files
            };
            Self::offer(list, item);
        }
    }
}

const MSG_DISJOINT: &str = "the two snapshots have no root in common";
const MSG_NOT_CANONICAL: &str = "is not in the canonical order";
const MSG_NEED_PAIR: &str = "--a-root and --b-root must be given together";
const MSG_ROOTS_CONFLICT: &str = "--match-roots cannot be combined with --a-root/--b-root";
const MSG_MANY_ROOTS: &str = "--match-roots needs each snapshot to have exactly one root";
const MSG_ROOT_OUTSIDE: &str = "is not under any root of snapshot";
pub const MSG_OPEN: &str = "cannot open snapshot";

/// 把比較失敗的訊息對應到穩定的錯誤代碼與建議。
/// 訊息文字由本檔產生，所以用同一組常數比對。
pub fn classify(message: &str) -> (&'static str, &'static str) {
    if message.contains(MSG_DISJOINT) {
        (
            "diff_roots_disjoint",
            "to compare snapshots of different paths add --match-roots (each snapshot has one root) or --a-root <PATH> --b-root <PATH> (the folders to line up); to compare one path over time, scan the same PATH twice",
        )
    } else if message.contains(MSG_NEED_PAIR) {
        (
            "usage",
            "give --a-root and --b-root together, or use --match-roots when each snapshot has exactly one root",
        )
    } else if message.contains(MSG_ROOTS_CONFLICT) {
        (
            "usage",
            "use either --match-roots or the --a-root/--b-root pair, not both",
        )
    } else if message.contains(MSG_MANY_ROOTS) {
        (
            "usage",
            "pick the folders explicitly: --a-root <PATH inside A> --b-root <PATH inside B>; 'voltdir report <snapshot>' lists a snapshot's roots",
        )
    } else if message.contains(MSG_ROOT_OUTSIDE) {
        (
            "usage",
            "the path must equal or lie under one of that snapshot's roots; 'voltdir report <snapshot>' shows them",
        )
    } else if message.contains(MSG_NOT_CANONICAL) {
        (
            "snapshot_not_canonical",
            "only files written by 'voltdir scan' (not edited or re-sorted) can be compared; scan again",
        )
    } else if message.contains(MSG_OPEN) {
        (
            "snapshot_unreadable",
            "check both file paths; make snapshots with 'voltdir scan <PATH> --out <FILE>'",
        )
    } else {
        (
            "snapshot_invalid",
            "both files must be complete snapshots written by 'voltdir scan'; if a scan was interrupted, scan again",
        )
    }
}

/// 跨路徑模式下 A、B 各自的比較根。
type CompareRoots = (Vec<u16>, Vec<u16>);

/// 決定跨路徑模式的比較根；沒給任何選項時回傳 None。
fn resolve_compare_roots(
    opt: &Options,
    roots_a: &[Vec<u16>],
    roots_b: &[Vec<u16>],
) -> Result<Option<CompareRoots>, String> {
    if opt.match_roots && (opt.a_root.is_some() || opt.b_root.is_some()) {
        return Err(MSG_ROOTS_CONFLICT.into());
    }
    if opt.match_roots {
        if roots_a.len() != 1 || roots_b.len() != 1 {
            return Err(format!(
                "{MSG_MANY_ROOTS} (snapshot A has {}, snapshot B has {})",
                roots_a.len(),
                roots_b.len()
            ));
        }
        return Ok(Some((roots_a[0].clone(), roots_b[0].clone())));
    }
    match (&opt.a_root, &opt.b_root) {
        (None, None) => Ok(None),
        (Some(a), Some(b)) => {
            for (label, root, roots) in [("A", a, roots_a), ("B", b, roots_b)] {
                if !roots.iter().any(|r| under(r, root)) {
                    return Err(format!(
                        "path '{}' {MSG_ROOT_OUTSIDE} {label} (its roots: {})",
                        String::from_utf16_lossy(root),
                        roots
                            .iter()
                            .map(|r| String::from_utf16_lossy(r))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
            Ok(Some((a.clone(), b.clone())))
        }
        _ => Err(MSG_NEED_PAIR.into()),
    }
}

/// 比較兩份快照（以 `open_a`、`open_b` 開啟，重疊的每個根各開一次）。
pub fn compare<RA: BufRead, RB: BufRead>(
    mut open_a: impl FnMut() -> Result<RA, String>,
    mut open_b: impl FnMut() -> Result<RB, String>,
    opt: &Options,
) -> Result<Outcome, String> {
    // 先讀兩份的 meta 以決定重疊的根
    let (roots_a, roots_b) = {
        let mut ia = open_a()?;
        let mut ib = open_b()?;
        let ra = Reader::open(&mut ia).map_err(|e| format!("snapshot A: {e}"))?;
        let rb = Reader::open(&mut ib).map_err(|e| format!("snapshot B: {e}"))?;
        (ra.meta().roots.clone(), rb.meta().roots.clone())
    };
    if roots_a.is_empty() || roots_b.is_empty() {
        return Err("a snapshot has no 'roots' in its meta line".into());
    }
    let compare_roots = resolve_compare_roots(opt, &roots_a, &roots_b)?;
    let overlap = match &compare_roots {
        // 跨路徑模式只有一組比較根；A 的根與 B 的根各自帶入
        Some((ra, _)) => vec![ra.clone()],
        None => {
            let o = overlap_roots(&roots_a, &roots_b);
            if o.is_empty() {
                return Err(MSG_DISJOINT.into());
            }
            o
        }
    };

    let mk = || TopK::new(opt.top);
    let mut cx = Ctx {
        opt,
        lists: Lists {
            growth_dirs: mk(),
            shrink_dirs: mk(),
            growth_files: mk(),
            shrink_files: mk(),
            added_dirs: mk(),
            added_files: mk(),
            removed_dirs: mk(),
            removed_files: mk(),
        },
        totals: Totals::default(),
        totals_incomplete: false,
    };
    let mut parsed_a = None;
    let mut parsed_b = None;
    for root in &overlap {
        let mut ia = open_a()?;
        let mut ib = open_b()?;
        let mut sa = Scoped {
            reader: Reader::open(&mut ia).map_err(|e| format!("snapshot A: {e}"))?,
            root: root.clone(),
            prev: Vec::new(),
            seen: false,
            rebase: compare_roots.is_some(),
            label: "A",
        };
        let mut sb = Scoped {
            reader: Reader::open(&mut ib).map_err(|e| format!("snapshot B: {e}"))?,
            root: compare_roots
                .as_ref()
                .map_or_else(|| root.clone(), |(_, rb)| rb.clone()),
            prev: Vec::new(),
            seen: false,
            rebase: compare_roots.is_some(),
            label: "B",
        };
        let (mut ea, mut eb) = (Rec::default(), Rec::default());
        let mut have_a = sa.next(&mut ea).map_err(|e| format!("snapshot A: {e}"))?;
        let mut have_b = sb.next(&mut eb).map_err(|e| format!("snapshot B: {e}"))?;
        let (mut root_a, mut root_b) = (false, false);
        while have_a || have_b {
            let order = match (have_a, have_b) {
                (true, true) => cmp_path(&ea.path, &eb.path),
                (true, false) => Ordering::Less,
                _ => Ordering::Greater,
            };
            match order {
                Ordering::Less => {
                    cx.only_in(&ea, false);
                    have_a = sa.next(&mut ea).map_err(|e| format!("snapshot A: {e}"))?;
                }
                Ordering::Greater => {
                    cx.only_in(&eb, true);
                    have_b = sb.next(&mut eb).map_err(|e| format!("snapshot B: {e}"))?;
                }
                Ordering::Equal => {
                    if ea.is_dir == eb.is_dir {
                        cx.pair(&ea, &eb);
                        let is_root = if compare_roots.is_some() {
                            ea.path.is_empty()
                        } else {
                            cmp_path(&ea.path, root) == Ordering::Equal
                        };
                        if ea.is_dir && is_root {
                            cx.totals.a_physical += u128::from(ea.rollup.physical);
                            cx.totals.a_logical += u128::from(ea.rollup.logical);
                            cx.totals.b_physical += u128::from(eb.rollup.physical);
                            cx.totals.b_logical += u128::from(eb.rollup.logical);
                            root_a = true;
                            root_b = true;
                        }
                    } else {
                        // 同一路徑一邊是目錄、另一邊是檔案：視為舊的消失、新的出現
                        cx.only_in(&ea, false);
                        cx.only_in(&eb, true);
                    }
                    have_a = sa.next(&mut ea).map_err(|e| format!("snapshot A: {e}"))?;
                    have_b = sb.next(&mut eb).map_err(|e| format!("snapshot B: {e}"))?;
                }
            }
        }
        if !(root_a && root_b) {
            cx.totals_incomplete = true;
        }
        parsed_a = Some(sa.reader.finish().map_err(|e| format!("snapshot A: {e}"))?);
        parsed_b = Some(sb.reader.finish().map_err(|e| format!("snapshot B: {e}"))?);
    }
    let a = parsed_a.expect("at least one overlap root");
    let b = parsed_b.expect("at least one overlap root");
    let mut reasons = Vec::new();
    for (label, p) in [("A", &a), ("B", &b)] {
        if let Some(m) = p.min_size {
            reasons.push(format!(
                "snapshot {label} was made with --min-size {m}: files smaller than that are not in it, so file-level results cover only the listed files"
            ));
        }
        if let Some(d) = p.max_depth {
            reasons.push(format!(
                "snapshot {label} was made with --max-depth {d}: entries deeper than that are not in it, so results cover only the listed entries"
            ));
        }
    }
    Ok(Outcome {
        a,
        b,
        overlap,
        compare_roots,
        lists: cx.lists,
        totals: cx.totals,
        totals_incomplete: cx.totals_incomplete,
        limited_reasons: reasons,
    })
}

fn push_side(out: &mut Vec<u8>, p: &Parsed, compare_root: Option<&[u16]>) {
    out.extend_from_slice(b"{\"started\":");
    json::push_str(out, &p.started);
    out.extend_from_slice(b",\"roots\":[");
    for (i, r) in p.roots.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        json::push_str_u16(out, r);
    }
    out.extend_from_slice(b"],");
    if let Some(c) = compare_root {
        out.extend_from_slice(b"\"compare_root\":");
        json::push_str_u16(out, c);
        out.push(b',');
    }
    out.extend_from_slice(b"\"engine\":");
    json::push_str(out, &p.engine);
    let _ = write!(out, ",\"incomplete_dirs\":{}}}", p.coverage.incomplete_dirs);
}

fn push_list(out: &mut Vec<u8>, list: TopK<Ranked>, item: impl Fn(&mut Vec<u8>, &Ranked)) {
    let truncated = list.truncated();
    out.extend_from_slice(b"{\"items\":[");
    for (i, x) in list.into_sorted().iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        item(out, x);
    }
    let _ = write!(out, "],\"truncated\":{truncated}}}");
}

fn dir_delta_item(out: &mut Vec<u8>, x: &Ranked) {
    out.extend_from_slice(b"{\"path\":");
    json::push_str_u16(out, &x.path);
    let _ = write!(
        out,
        ",\"a_rollup\":{},\"b_rollup\":{},\"delta_rollup\":{},\"delta_direct\":{}}}",
        x.a,
        x.b,
        i128::from(x.b) - i128::from(x.a),
        x.ddirect
    );
}

fn file_delta_item(out: &mut Vec<u8>, x: &Ranked) {
    out.extend_from_slice(b"{\"path\":");
    json::push_str_u16(out, &x.path);
    let _ = write!(
        out,
        ",\"a\":{},\"b\":{},\"delta\":{}}}",
        x.a,
        x.b,
        i128::from(x.b) - i128::from(x.a)
    );
}

fn presence_item(out: &mut Vec<u8>, x: &Ranked) {
    out.extend_from_slice(b"{\"path\":");
    json::push_str_u16(out, &x.path);
    let _ = write!(out, ",\"physical\":{},\"logical\":{}}}", x.key, x.logical);
}

pub fn render(o: Outcome) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 * 1024);
    out.extend_from_slice(b"{\"schema\":");
    json::push_str(&mut out, SCHEMA);
    out.extend_from_slice(b",\"a\":");
    let cr = o.compare_roots.as_ref();
    push_side(&mut out, &o.a, cr.map(|c| c.0.as_slice()));
    out.extend_from_slice(b",\"b\":");
    push_side(&mut out, &o.b, cr.map(|c| c.1.as_slice()));
    if cr.is_some() {
        // 跨路徑模式：沒有共同的根，路徑都是相對於各自的比較根
        out.extend_from_slice(b",\"rebased\":true");
    } else {
        out.extend_from_slice(b",\"overlap\":[");
        for (i, r) in o.overlap.iter().enumerate() {
            if i > 0 {
                out.push(b',');
            }
            json::push_str_u16(&mut out, r);
        }
        out.push(b']');
    }
    out.extend_from_slice(b",\"notes\":[");
    json::push_str(
        &mut out,
        "sizes are physical (allocated) bytes after hard-link de-duplication; a name that moves between directories can change which name carries the size",
    );
    out.extend_from_slice(b",");
    json::push_str(
        &mut out,
        "entries are paired by case-insensitive path; a rename or move shows as removed plus added",
    );
    out.extend_from_slice(b"],\"totals\":");
    if o.totals_incomplete {
        out.extend_from_slice(b"null");
    } else {
        let t = &o.totals;
        let _ = write!(
            out,
            "{{\"a_physical\":{},\"b_physical\":{},\"delta_physical\":{},\"a_logical\":{},\"b_logical\":{},\"delta_logical\":{}}}",
            t.a_physical,
            t.b_physical,
            t.b_physical as i128 - t.a_physical as i128,
            t.a_logical,
            t.b_logical,
            t.b_logical as i128 - t.a_logical as i128
        );
    }
    let l = o.lists;
    out.extend_from_slice(b",\"growth\":{\"dirs\":");
    push_list(&mut out, l.growth_dirs, dir_delta_item);
    out.extend_from_slice(b",\"files\":");
    push_list(&mut out, l.growth_files, file_delta_item);
    out.extend_from_slice(b"},\"shrink\":{\"dirs\":");
    push_list(&mut out, l.shrink_dirs, dir_delta_item);
    out.extend_from_slice(b",\"files\":");
    push_list(&mut out, l.shrink_files, file_delta_item);
    out.extend_from_slice(b"},\"added\":{\"dirs\":");
    push_list(&mut out, l.added_dirs, presence_item);
    out.extend_from_slice(b",\"files\":");
    push_list(&mut out, l.added_files, presence_item);
    out.extend_from_slice(b"},\"removed\":{\"dirs\":");
    push_list(&mut out, l.removed_dirs, presence_item);
    out.extend_from_slice(b",\"files\":");
    push_list(&mut out, l.removed_files, presence_item);
    let _ = write!(
        out,
        "}},\"limited_by_snapshot\":{},\"limited_reasons\":[",
        !o.limited_reasons.is_empty()
    );
    for (i, r) in o.limited_reasons.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        json::push_str(&mut out, r);
    }
    out.extend_from_slice(b"]}\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn path_order_is_preorder_and_case_insensitive() {
        let o = |a: &str, b: &str| cmp_path(&w(a), &w(b));
        assert_eq!(o(r"C:\a", r"c:\A"), Ordering::Equal);
        assert_eq!(o(r"C:\a", r"C:\a\b"), Ordering::Less);
        assert_eq!(o(r"C:\a\z", r"C:\b"), Ordering::Less);
        // 前綴不是「同一段」：a.txt 與 a 是不同的段，而 'a' < 'a.txt'
        assert_eq!(o(r"C:\a", r"C:\a.txt"), Ordering::Less);
        assert_eq!(o(r"C:\a\z", r"C:\a.txt"), Ordering::Less);
        // 根目錄的結尾分隔符不影響
        assert_eq!(o(r"C:\", r"C:"), Ordering::Equal);
        assert_eq!(o(r"C:\", r"C:\x"), Ordering::Less);
    }

    #[test]
    fn under_and_overlap() {
        assert!(under(&w(r"C:\a"), &w(r"c:\A\b")));
        assert!(under(&w(r"C:\"), &w(r"C:\x\y")));
        assert!(!under(&w(r"C:\a"), &w(r"C:\ab")));
        assert!(!under(&w(r"C:\a\b"), &w(r"C:\a")));
        let o = overlap_roots(&[w(r"C:\")], &[w(r"C:\Users"), w(r"D:\x")]);
        assert_eq!(o, vec![w(r"C:\Users")]);
        let o2 = overlap_roots(&[w(r"C:\a\b")], &[w(r"C:\a")]);
        assert_eq!(o2, vec![w(r"C:\a\b")]);
        assert!(overlap_roots(&[w(r"C:\a")], &[w(r"C:\b")]).is_empty());
    }
}
