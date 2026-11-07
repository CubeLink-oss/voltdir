//! voltdir：供 AI agent 使用的唯讀 Windows 磁碟掃描工具。
//!
//! 標準輸出只輸出機器可讀的結果；進度與診斷一律寫到標準錯誤。

#![deny(unsafe_code)]

mod cli;
mod diff;
mod dirbuf;
mod fail;
mod find;
mod fxhash;
mod glob;
mod help;
mod index;
mod info;
mod json;
mod jsonin;
mod mft;
mod model;
mod names;
mod nidx;
mod ntfs;
mod overview;
mod regex;
mod report;
mod snapread;
mod snapshot;
mod stream;
#[allow(unsafe_code)]
mod sys;
mod timefmt;
mod walk;

use cli::{
    CliError, Command, Common, DiffArgs, Engine, FindArgs, FollowKind, OverviewArgs, ReportArgs,
    ScanArgs,
};
use model::ScanData;
use report::{Sink, Tallier};
use std::io::Write;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 結束代碼。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Exit {
    Ok,
    Usage,
    Incomplete,
    EngineUnavailable,
    OutputFailed,
    Cancelled,
    #[allow(dead_code)] // 預留給無法歸類的內部錯誤
    Internal,
}

impl Exit {
    fn code(self) -> u8 {
        match self {
            Exit::Ok => 0,
            Exit::Usage => 1,
            Exit::Incomplete => 2,
            Exit::EngineUnavailable => 3,
            Exit::OutputFailed => 4,
            Exit::Cancelled => 5,
            Exit::Internal => 99,
        }
    }

    /// 由掃描結果決定：被取消優先於讀取失敗。
    fn from_scan(incomplete: u64, cancelled: u64) -> Exit {
        if cancelled > 0 {
            Exit::Cancelled
        } else if incomplete > 0 {
            Exit::Incomplete
        } else {
            Exit::Ok
        }
    }
}

fn version_line() -> String {
    format!("voltdir {VERSION}")
}

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    fail::set_json(fail::detect_json(&args));
    let exit = match cli::parse(args) {
        Ok(Command::Version) => {
            println!("{}", version_line());
            Exit::Ok
        }
        Ok(Command::Help(text)) => {
            print!("{text}");
            Exit::Ok
        }
        Ok(Command::Info) => run_info(),
        Ok(Command::Scan(a)) => run_scan(a),
        Ok(Command::Report(a)) => run_report(a),
        Ok(Command::Find(a)) => run_find(a),
        Ok(Command::Diff(a)) => run_diff(a),
        Ok(Command::Overview(a)) => run_overview(a),
        Ok(Command::Index(a)) => run_index(a),
        Err(CliError::Usage(m)) => {
            fail::print(
                cli::usage_code(&m),
                &m,
                "run 'voltdir help <command>' to see every option with its default and allowed range",
            );
            if !fail::is_json() {
                eprint!("{}", help::GENERAL);
            }
            Exit::Usage
        }
        Err(CliError::Unsupported(m)) => {
            fail::print(
                "command_unsupported",
                &m,
                "this command is planned for a later version; use 'voltdir help' to see what is available",
            );
            Exit::Usage
        }
    };
    ExitCode::from(exit.code())
}

fn run_info() -> Exit {
    let out = info::render(VERSION);
    let mut so = std::io::stdout().lock();
    match so.write_all(&out).and_then(|()| so.flush()) {
        Ok(()) => Exit::Ok,
        Err(_) => Exit::OutputFailed,
    }
}

/// 轉成顯示用 Win32 路徑：絕對、去掉 `\\?\` 前綴、去掉結尾分隔符（磁碟機根除外）。
fn display_path(p: &Path) -> Result<Vec<u16>, String> {
    let abs = std::path::absolute(p).map_err(|e| format!("{}: {e}", p.display()))?;
    let mut w: Vec<u16> = abs.as_os_str().encode_wide().collect();
    let unc: Vec<u16> = r"\\?\UNC\".encode_utf16().collect();
    let pre: Vec<u16> = r"\\?\".encode_utf16().collect();
    if w.starts_with(&unc) {
        let mut n: Vec<u16> = r"\\".encode_utf16().collect();
        n.extend_from_slice(&w[unc.len()..]);
        w = n;
    } else if w.starts_with(&pre) {
        w.drain(..pre.len());
    }
    let sep = u16::from(b'\\');
    while w.len() > 3 && w.last() == Some(&sep) {
        w.pop();
    }
    Ok(w)
}

fn upper(s: &[u16]) -> Vec<u16> {
    s.iter().map(|&u| names::fold(u)).collect()
}

/// `inner` 是否等於 `outer` 或位於其底下（不分大小寫）。
fn contains_root(outer: &[u16], inner: &[u16]) -> bool {
    let (o, i) = (upper(outer), upper(inner));
    if i.len() < o.len() || !i.starts_with(&o) {
        return false;
    }
    o.last() == Some(&u16::from(b'\\')) || i.len() == o.len() || i[o.len()] == u16::from(b'\\')
}

/// 去除重複與被其他根包含的根，保留使用者給的順序。
fn effective_roots(displays: Vec<Vec<u16>>) -> (Vec<Vec<u16>>, Vec<Vec<u16>>) {
    let mut keep = Vec::new();
    let mut dropped = Vec::new();
    for (i, d) in displays.iter().enumerate() {
        let redundant = displays
            .iter()
            .enumerate()
            .any(|(j, o)| j != i && contains_root(o, d) && (!contains_root(d, o) || j < i));
        if redundant {
            dropped.push(d.clone());
        } else {
            keep.push(d.clone());
        }
    }
    (keep, dropped)
}

struct Scanned {
    data: ScanData,
    /// `"mft"`、`"walk"` 或 `"mixed"`
    engine: &'static str,
    /// 與 `data.roots` 相同順序的有效掃描根（mixed 時 walk 的根在前）
    meta_roots: Vec<Vec<u16>>,
    used: snapshot::UsedOptions,
    started: u64,
    finished: u64,
    duration_ms: u64,
    threads: usize,
}

/// `mft` 引擎是否適用於這個根：NTFS 磁碟機的根目錄。
fn mft_eligible(display: &[u16]) -> Result<(), &'static str> {
    if !walk::is_volume_root(display) {
        return Err("the mft engine only supports whole-volume roots (for example C:\\)");
    }
    match sys::volume_details(display) {
        Some(v) if v.fs.eq_ignore_ascii_case("NTFS") && !v.is_network => Ok(()),
        Some(_) => Err("the mft engine only supports local NTFS volumes"),
        None => Err("cannot read volume information"),
    }
}

/// 依 `--engine` 把各根分給 `mft` 或 `walk` 並掃描。回傳資料、實際的根順序與引擎名稱。
fn scan_roots(
    displays: Vec<Vec<u16>>,
    c: &Common,
    opts: &walk::Options,
) -> Result<(ScanData, Vec<Vec<u16>>, &'static str), Exit> {
    let forced = c.engine == Engine::Mft;
    let mut walk_roots: Vec<Vec<u16>> = Vec::new();
    let mut mft_roots: Vec<(Vec<u16>, ScanData, mft::Stats)> = Vec::new();
    // `auto`：已提權、沒有 --follow-links、根是 NTFS 磁碟機根目錄時用 mft
    let auto_allowed = sys::is_elevated() && c.follow.is_empty();
    if forced && !c.follow.is_empty() {
        fail::print(
            "engine_option_conflict",
            "--follow-links cannot be used with --engine mft",
            "use --engine walk (or --engine auto) when following links, or drop --follow-links",
        );
        return Err(Exit::EngineUnavailable);
    }
    for d in displays {
        let name = String::from_utf16_lossy(&d);
        let try_mft = match c.engine {
            Engine::Walk => false,
            Engine::Mft => true,
            Engine::Auto => auto_allowed && mft_eligible(&d).is_ok(),
        };
        if !try_mft {
            walk_roots.push(d);
            continue;
        }
        let eligibility = mft_eligible(&d);
        let not_eligible = eligibility.is_err();
        let result = eligibility
            .map_err(|m| mft::MftError::Unavailable(m.to_string()))
            .and_then(|()| mft::scan_volume(&d, opts, 0));
        match result {
            Ok((data, stats)) => {
                if !c.quiet {
                    if stats.bad_records > 0 {
                        eprintln!(
                            "warning: {} master file table record(s) of {name} were damaged and skipped",
                            stats.bad_records
                        );
                    }
                    if stats.orphan_names > 0 {
                        eprintln!(
                            "note: {} name(s) on {name} could not be reached from the root directory",
                            stats.orphan_names
                        );
                    }
                }
                mft_roots.push((d, data, stats));
            }
            Err(e) if forced => {
                fail::print(
                    "engine_mft_unavailable",
                    &format!("the mft engine cannot scan {name}: {}", e.message()),
                    if not_eligible {
                        "the mft engine only reads a whole local NTFS drive (for example C:\\); elevation does not help here. Use --engine auto or --engine walk for folders and other file systems"
                    } else {
                        "run from an elevated (administrator) prompt, or use --engine auto to fall back to the walk engine (--engine walk always works without elevation)"
                    },
                );
                return Err(Exit::EngineUnavailable);
            }
            Err(e) => {
                if !c.quiet {
                    eprintln!(
                        "note: the mft engine is not used for {name} ({}); using the walk engine",
                        e.message()
                    );
                }
                walk_roots.push(d);
            }
        }
    }
    let engine = match (walk_roots.is_empty(), mft_roots.is_empty()) {
        (_, true) => "walk",
        (true, false) => "mft",
        (false, false) => "mixed",
    };
    let mut data = if walk_roots.is_empty() {
        ScanData {
            dirs: Vec::new(),
            roots: Vec::new(),
        }
    } else {
        let specs: Vec<walk::RootSpec> = walk_roots
            .iter()
            .map(|d| walk::RootSpec { display: d.clone() })
            .collect();
        walk::scan(&specs, opts)
    };
    let mut order = walk_roots;
    for (d, part, _) in mft_roots {
        mft::append(&mut data, part);
        order.push(d);
    }
    Ok((data, order, engine))
}

/// 驗證根、套用選項、掃描並完成後處理。失敗時回傳結束代碼。
fn prepare_scan(
    roots: &[PathBuf],
    c: &Common,
    min_size: Option<u64>,
    max_depth: Option<u64>,
) -> Result<Scanned, Exit> {
    let displays = resolve_roots(roots, c)?;
    scan_resolved(displays, c, min_size, max_depth)
}

/// 驗證每個掃描根（存在、是資料夾）並去掉已被其他根涵蓋的根。
fn resolve_roots(roots: &[PathBuf], c: &Common) -> Result<Vec<Vec<u16>>, Exit> {
    let mut displays = Vec::new();
    for r in roots {
        let d = display_path(r).map_err(|m| {
            fail::print(
                "path_invalid",
                &m,
                "give an existing folder or drive, for example C:\\ or C:\\Users",
            );
            Exit::Usage
        })?;
        let os = <std::ffi::OsString as std::os::windows::ffi::OsStringExt>::from_wide(&d);
        match std::fs::metadata(PathBuf::from(os)) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                fail::print(
                    "path_not_directory",
                    &format!("not a directory: {}", r.display()),
                    "scan needs a folder or a drive root, not a file; pass the file's parent folder",
                );
                return Err(Exit::Usage);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                fail::print(
                    "path_not_found",
                    &format!("cannot access {}: {e}", r.display()),
                    "check the spelling and that the drive is present; relative paths start from the current folder",
                );
                return Err(Exit::Usage);
            }
            Err(e) => {
                fail::print(
                    "path_inaccessible",
                    &format!("cannot access {}: {e}", r.display()),
                    "run from an elevated (administrator) prompt or choose a folder you are allowed to read",
                );
                return Err(Exit::Usage);
            }
        }
        displays.push(d);
    }
    let (displays, dropped) = effective_roots(displays);
    for d in &dropped {
        if !c.quiet {
            eprintln!(
                "note: root {} is already covered by another root and is not scanned twice",
                String::from_utf16_lossy(d)
            );
        }
    }
    Ok(displays)
}

/// 走訪選項（執行緒數、排除、跟隨連結、期限）。
fn walk_options(c: &Common) -> (usize, Vec<Vec<u16>>, walk::Options) {
    let threads = c
        .threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));
    let (exclude, exclude_set) = walk::Options::with_exclude(
        c.exclude
            .iter()
            .map(|p| p.encode_wide().collect())
            .collect(),
    );
    let opts = walk::Options {
        threads,
        exclude: exclude.clone(),
        exclude_set,
        follow_mount: c.follow.contains(&FollowKind::Mount),
        follow_symlink: c.follow.contains(&FollowKind::Symlink),
        follow_junction: c.follow.contains(&FollowKind::Junction),
        deadline: c
            .timeout_sec
            .map(|s| std::time::Instant::now() + std::time::Duration::from_secs(s)),
    };
    (threads, exclude, opts)
}

fn scan_resolved(
    displays: Vec<Vec<u16>>,
    c: &Common,
    min_size: Option<u64>,
    max_depth: Option<u64>,
) -> Result<Scanned, Exit> {
    let (threads, exclude, opts) = walk_options(c);
    sys::install_interrupt_handler();

    let started = timefmt::now_filetime();
    let clock = std::time::Instant::now();
    let (mut data, displays, engine) = scan_roots(displays, c, &opts)?;
    warn_unmatched_excludes(c, &opts);
    snapshot::finalize(&mut data, c.hardlinks_dedupe, min_size);
    let finished = timefmt::now_filetime();
    let duration_ms = clock.elapsed().as_millis() as u64;

    let used = snapshot::UsedOptions {
        threads,
        hardlinks_dedupe: c.hardlinks_dedupe,
        min_size,
        max_depth,
        exclude,
        follow_links: c.follow.iter().map(|k| k.as_str()).collect(),
        timeout_sec: c.timeout_sec,
    };
    Ok(Scanned {
        data,
        engine,
        meta_roots: displays,
        used,
        started,
        finished,
        duration_ms,
        threads,
    })
}

/// `--exclude` 樣式從頭到尾沒命中任何路徑時提醒一聲（結束代碼不變）。
/// 這種情形多半是樣式寫錯；以前寫了 `*/node_modules/*` 這類樣式會悄悄什麼都不排除。
fn warn_unmatched_excludes(c: &Common, opts: &walk::Options) {
    if c.quiet {
        return;
    }
    for pattern in opts.unmatched_excludes() {
        eprintln!(
            "warning: --exclude '{}' matched no path; patterns match the whole path, for example *\\node_modules or *.tmp",
            String::from_utf16_lossy(pattern)
        );
    }
}

fn volumes_of(displays: &[Vec<u16>]) -> Vec<sys::VolumeDetails> {
    let mut volumes: Vec<sys::VolumeDetails> = Vec::new();
    for d in displays {
        if let Some(v) = sys::volume_path_name(d).and_then(|r| sys::volume_details(&r))
            && !volumes.iter().any(|x| x.root == v.root)
        {
            volumes.push(v);
        }
    }
    volumes
}

fn log_done(c: &Common, s: &Scanned, incomplete: u64, cancelled: u64) {
    let dirs = s.data.dirs.len();
    let files: usize = s.data.dirs.iter().map(|d| d.files.len()).sum();
    log_done_parts(
        c,
        dirs,
        files,
        s.duration_ms,
        s.threads,
        incomplete,
        cancelled,
    );
}

fn log_done_parts(
    c: &Common,
    dirs: usize,
    files: usize,
    duration_ms: u64,
    threads: usize,
    incomplete: u64,
    cancelled: u64,
) {
    if c.quiet {
        return;
    }
    if c.log_json {
        eprintln!(
            "{{\"event\":\"done\",\"dirs\":{dirs},\"files\":{files},\"duration_ms\":{duration_ms},\"incomplete_dirs\":{incomplete},\"cancelled_dirs\":{cancelled}}}"
        );
    } else {
        eprintln!(
            "scanned {dirs} dirs, {files} files in {duration_ms} ms ({threads} threads, {incomplete} incomplete dirs, {cancelled} cancelled dirs)"
        );
    }
    if cancelled > 0 {
        eprintln!(
            "warning: the scan was stopped early; directories marked 'cancelled' were not read"
        );
    }
}

/// `report`／`overview` 的串流掃描在使用者沒有指定 `--threads` 時使用的執行緒數上限。
const STREAM_DEFAULT_THREADS: usize = 3;

/// 即時掃描給 `report`／`overview` 的結果。逐筆項目只有在需要寫快照時才保留（`data`）。
struct Live {
    engine: &'static str,
    meta_roots: Vec<Vec<u16>>,
    used: snapshot::UsedOptions,
    started: u64,
    finished: u64,
    duration_ms: u64,
    threads: usize,
    totals: model::Totals,
    coverage: snapshot::Coverage,
    cancelled: u64,
    dirs: usize,
    files: usize,
    data: Option<ScanData>,
}

/// 這些根會不會用 `mft` 引擎？會的話走原本的整棵樹路徑（串流只做 `walk`）。
fn streaming_ok(displays: &[Vec<u16>], c: &Common) -> bool {
    match c.engine {
        Engine::Walk => true,
        Engine::Mft => false,
        Engine::Auto => {
            !(sys::is_elevated()
                && c.follow.is_empty()
                && displays.iter().any(|d| mft_eligible(d).is_ok()))
        }
    }
}

/// 把整棵樹依標準順序餵給接收端（`mft` 引擎或需要保留快照時使用）。
fn feed_sink(data: &ScanData, sink: &mut dyn Sink) {
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
            let ext = names::ext_of(dir.file_name(file));
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

/// 掃描並把結果餵給接收端。`keep_data` 為 false 且是 `walk` 引擎時用串流掃描，不保留逐筆項目。
/// `live_scan` 的接收端。能「先計入、後放入」的接收端（`report`、`overview`）可以走 `mft` 的緊湊路徑。
trait LiveSink: Sink + Send {
    fn tallier(&mut self) -> Option<&mut dyn Tallier> {
        None
    }
}

impl LiveSink for report::Analyzer {
    fn tallier(&mut self) -> Option<&mut dyn Tallier> {
        Some(self)
    }
}

impl LiveSink for overview::Collector {
    fn tallier(&mut self) -> Option<&mut dyn Tallier> {
        Some(self)
    }
}

impl<A: Sink + Send, B: Sink + Send> LiveSink for Tee<'_, A, B> {}

/// 這個根要用 `mft` 引擎嗎？（和 `scan_roots` 的規則相同，但這裡只看單一根）
fn wants_mft(display: &[u16], c: &Common) -> bool {
    // VOLTDIR_MFT_STANDARD：關閉緊湊路徑，給差異測試與「改動前」的量測用（不是公開的選項）
    if std::env::var_os("VOLTDIR_MFT_STANDARD").is_some() {
        return false;
    }
    match c.engine {
        Engine::Walk => false,
        Engine::Mft => true,
        Engine::Auto => sys::is_elevated() && c.follow.is_empty() && mft_eligible(display).is_ok(),
    }
}

fn live_scan(
    roots: &[PathBuf],
    c: &Common,
    keep_data: bool,
    sink: &mut dyn LiveSink,
) -> Result<Live, Exit> {
    let displays = resolve_roots(roots, c)?;
    // mft 的緊湊路徑：單一根、不存快照、不排除、要硬連結去重；任何不能保證結果相同的情況都回退
    if !keep_data
        && displays.len() == 1
        && c.exclude.is_empty()
        && c.hardlinks_dedupe
        && wants_mft(&displays[0], c)
        && mft_eligible(&displays[0]).is_ok()
        && let Some(t) = sink.tallier()
    {
        let (threads, exclude, opts) = walk_options(c);
        sys::install_interrupt_handler();
        let started = timefmt::now_filetime();
        let clock = std::time::Instant::now();
        match mft::scan_volume_compact(&displays[0], &opts, t) {
            Ok(out) => {
                let finished = timefmt::now_filetime();
                if !c.quiet && out.stats.orphan_names > 0 {
                    eprintln!(
                        "note: {} name(s) on {} could not be reached from the root directory",
                        out.stats.orphan_names,
                        String::from_utf16_lossy(&displays[0])
                    );
                }
                return Ok(Live {
                    engine: "mft",
                    meta_roots: displays,
                    used: snapshot::UsedOptions {
                        threads,
                        hardlinks_dedupe: c.hardlinks_dedupe,
                        min_size: None,
                        max_depth: None,
                        exclude,
                        follow_links: c.follow.iter().map(|k| k.as_str()).collect(),
                        timeout_sec: c.timeout_sec,
                    },
                    started,
                    finished,
                    duration_ms: clock.elapsed().as_millis() as u64,
                    threads,
                    totals: out.totals,
                    coverage: out.coverage,
                    cancelled: out.cancelled,
                    dirs: out.dirs as usize,
                    files: out.files as usize,
                    data: None,
                });
            }
            Err(mft::CompactError::Fallback(why)) => {
                if !c.quiet {
                    eprintln!(
                        "note: the compact mft path was not used ({why}); using the standard path"
                    );
                }
                t.reset();
            }
            // 讀不到磁碟區或沒有權限等：重設後交給原本的路徑，由它回報（錯誤訊息與結束代碼不變）
            Err(mft::CompactError::Failed(_)) => t.reset(),
        }
    }
    if !keep_data && streaming_ok(&displays, c) {
        let (_, exclude, mut opts) = walk_options(c);
        // 目錄列舉在這台機器上超過 2 到 3 個執行緒就不再變快（反而因為開啟目錄的鎖競爭變慢，
        // ；使用者沒指定 --threads 時串流掃描用較少的執行緒
        let threads = c
            .threads
            .unwrap_or_else(|| opts.threads.min(STREAM_DEFAULT_THREADS));
        opts.threads = threads;
        sys::install_interrupt_handler();
        let specs: Vec<walk::RootSpec> = displays
            .iter()
            .map(|d| walk::RootSpec { display: d.clone() })
            .collect();
        let started = timefmt::now_filetime();
        let clock = std::time::Instant::now();
        let sum = stream::scan(&specs, &opts, c.hardlinks_dedupe, sink);
        warn_unmatched_excludes(c, &opts);
        let finished = timefmt::now_filetime();
        return Ok(Live {
            engine: "walk",
            meta_roots: displays,
            used: snapshot::UsedOptions {
                threads,
                hardlinks_dedupe: c.hardlinks_dedupe,
                min_size: None,
                max_depth: None,
                exclude,
                follow_links: c.follow.iter().map(|k| k.as_str()).collect(),
                timeout_sec: c.timeout_sec,
            },
            started,
            finished,
            duration_ms: clock.elapsed().as_millis() as u64,
            threads,
            totals: sum.totals,
            coverage: sum.coverage,
            cancelled: sum.cancelled,
            dirs: sum.dirs as usize,
            files: sum.files as usize,
            data: None,
        });
    }
    let s = scan_resolved(displays, c, None, None)?;
    feed_sink(&s.data, sink);
    let (coverage, cancelled) = snapshot::count_coverage(&s.data);
    let dirs = s.data.dirs.len();
    let files: usize = s.data.dirs.iter().map(|d| d.files.len()).sum();
    let totals = snapshot::scan_totals(&s.data);
    Ok(Live {
        engine: s.engine,
        meta_roots: s.meta_roots,
        used: s.used,
        started: s.started,
        finished: s.finished,
        duration_ms: s.duration_ms,
        threads: s.threads,
        totals,
        coverage,
        cancelled,
        dirs,
        files,
        data: keep_data.then_some(s.data),
    })
}

fn write_output(
    out: &Option<PathBuf>,
    body: impl FnOnce(&mut dyn Write) -> std::io::Result<()>,
) -> Result<(), Exit> {
    let r = match out {
        None => {
            let mut so = std::io::stdout().lock();
            body(&mut so)
        }
        Some(path) => write_atomic(path, |w| body(w)),
    };
    r.map_err(|e| {
        fail::print(
            "output_write_failed",
            &format!("cannot write output: {e}"),
            "check free space and permissions of the destination, or write to standard output with --out -",
        );
        Exit::OutputFailed
    })
}

/// `--out` 的父資料夾必須在掃描開始前就存在且可寫入；不自動建立資料夾。
fn check_out(out: &Option<PathBuf>) -> Result<(), Exit> {
    let Some(path) = out else {
        return Ok(());
    };
    let shown = path.display();
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let refuse = |code: &str, why: String, hint: &str| {
        fail::print(code, &format!("cannot write output {shown}: {why}"), hint);
        Err(Exit::OutputFailed)
    };
    match std::fs::metadata(parent) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => {
            return refuse(
                "output_dir_missing",
                format!("{} is not a folder", parent.display()),
                "give an --out path whose parent folder exists; voltdir does not create folders",
            );
        }
        Err(e) => {
            return refuse(
                "output_dir_missing",
                format!("folder {}: {e}", parent.display()),
                "create the folder first (voltdir does not create folders) or choose another --out path",
            );
        }
    }
    if path.is_dir() {
        return refuse(
            "output_path_is_directory",
            "the path is an existing folder".into(),
            "give a file name, for example --out snapshot.ndjson",
        );
    }
    let probe = parent.join(format!(".voltdir-probe-{}", std::process::id()));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(f) => {
            drop(f);
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(e) => refuse(
            "output_dir_not_writable",
            format!("folder {} is not writable: {e}", parent.display()),
            "choose a folder you can write to, or fix its permissions",
        ),
    }
}

fn run_scan(args: ScanArgs) -> Exit {
    // 快照可能有數百 MB，忘了 --out 會灌爆呼叫者的上下文：必須明確選擇
    if args.common.out.is_none() && !args.common.out_dash {
        fail::print(
            "output_required",
            "scan needs --out: a snapshot can be hundreds of megabytes and is not written to standard output by default",
            "use --out <FILE> to save it, or --out - to write it to standard output explicitly",
        );
        return Exit::Usage;
    }
    if let Err(e) = check_out(&args.common.out) {
        return e;
    }
    let s = match prepare_scan(&args.roots, &args.common, args.min_size, args.max_depth) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let meta = snapshot::Meta {
        tool_version: VERSION,
        started: s.started,
        finished: s.finished,
        engine: s.engine,
        elevated: sys::is_elevated(),
        options: s.used.clone(),
        roots: &s.meta_roots,
        volumes: volumes_of(&s.meta_roots),
    };
    let emit = snapshot::Emit {
        min_size: args.min_size,
        max_depth: args.max_depth,
    };
    let mut outcome = None;
    if let Err(e) = write_output(&args.common.out, |w| {
        outcome = Some(snapshot::write_snapshot(
            &s.data,
            &meta,
            emit,
            s.duration_ms,
            &mut Wrap(w),
        )?);
        Ok(())
    }) {
        return e;
    }
    let o = outcome.expect("written");
    if let Some(path) = &args.common.out
        && !args.common.no_index
    {
        write_index(path, &s.data, emit, &o.layout, args.common.quiet);
    }
    log_done(&args.common, &s, o.incomplete_dirs, o.cancelled_dirs);
    Exit::from_scan(o.incomplete_dirs, o.cancelled_dirs)
}

/// 在剛寫好的快照旁寫彙總索引。任何失敗都只是警告，不影響快照與結束代碼。
fn write_index(
    snapshot_path: &Path,
    data: &ScanData,
    emit: snapshot::Emit,
    layout: &snapshot::Layout,
    quiet: bool,
) {
    let result = index::build(snapshot_path, data, emit).and_then(|bytes| {
        write_atomic(&index::path_of(snapshot_path), |w| w.write_all(&bytes)).map_err(|e| {
            format!(
                "cannot write {}: {e}",
                index::path_of(snapshot_path).display()
            )
        })
    });
    if let Err(why) = result
        && !quiet
    {
        eprintln!("warning: index not written ({why}); the snapshot itself is complete");
    }
    // 名稱索引（find --snapshot 用）；失敗同樣只是警告
    let result = nidx::build_from_scan(snapshot_path, data, layout).and_then(|bytes| {
        write_atomic(&nidx::path_of(snapshot_path), |w| w.write_all(&bytes)).map_err(|e| {
            format!(
                "cannot write {}: {e}",
                nidx::path_of(snapshot_path).display()
            )
        })
    });
    if let Err(why) = result
        && !quiet
    {
        eprintln!("warning: name index not written ({why}); the snapshot itself is complete");
    }
}

/// 把同一串項目交給兩個接收端（`overview` 的彙總與索引建構器）。
struct Tee<'a, A: Sink, B: Sink>(&'a mut A, &'a mut B);

impl<A: Sink, B: Sink> Sink for Tee<'_, A, B> {
    fn dir(&mut self, d: &report::DirView<'_>) {
        self.0.dir(d);
        self.1.dir(d);
    }
    fn file(&mut self, f: &report::FileView<'_>) {
        self.0.file(f);
        self.1.file(f);
    }
}

/// 讀不到或不是索引：沒有快照可以回退，直接說明。
fn index_unreadable(why: &str) -> Exit {
    fail::print(
        "index_unreadable",
        why,
        "give a file written by 'voltdir overview <PATH> --save-index <FILE>'",
    );
    Exit::Usage
}

fn index_cannot_answer(why: &str) -> Exit {
    fail::print(
        "index_cannot_answer",
        &format!("the index cannot answer this question: {why}"),
        "scan again with 'voltdir overview <PATH> --save-snapshot <FILE>' (it also keeps find and diff working), or drop the extra filter",
    );
    Exit::Usage
}

fn overview_from_index(args: &OverviewArgs, path: &Path) -> Exit {
    let ix = match index::Index::load_standalone(path) {
        Ok(i) => i,
        Err(why) => return index_unreadable(&why),
    };
    let mut col = overview::Collector::new(overview_options(args));
    if let Err(why) = ix.replay_overview(&mut col, &overview_options(args)) {
        return index_cannot_answer(&why);
    }
    let parsed = ix.parsed;
    let meta_body = args.full.then(|| {
        let mut b = parsed.meta_body.clone();
        if b.last() == Some(&b'}') {
            b.pop();
            b.extend_from_slice(b",\"source\":\"index\"}");
        }
        b
    });
    let index_arg = quoted(path);
    let roots_quoted = parsed
        .roots
        .iter()
        .map(|r| format!("\"{}\"", String::from_utf16_lossy(r)))
        .collect::<Vec<_>>()
        .join(" ");
    let next = |_: Option<&str>| overview::next_commands_index(&index_arg, &roots_quoted);
    let frame = overview::Frame {
        roots: &parsed.roots,
        engine: &parsed.engine,
        totals: parsed.totals,
        coverage: &parsed.coverage,
        limited_reasons: &[],
        meta_body: meta_body.as_deref(),
        next: &next,
    };
    let out = col.render(&frame, relative_base(args, &parsed.roots));
    if let Err(e) = write_output(&args.common.out, |w| {
        w.write_all(&out).and_then(|()| w.flush())
    }) {
        return e;
    }
    if parsed.coverage.incomplete_dirs > 0 {
        if !args.common.quiet {
            eprintln!(
                "warning: the index reports {} directories that were not fully read; see 'coverage'",
                parsed.coverage.incomplete_dirs
            );
        }
        return Exit::Incomplete;
    }
    Exit::Ok
}

fn report_from_index(args: &ReportArgs, path: &Path, req: report::Request) -> Exit {
    let ix = match index::Index::load_standalone(path) {
        Ok(i) => i,
        Err(why) => return index_unreadable(&why),
    };
    let mut an = report::Analyzer::new(req.clone());
    if let Err(why) = ix.replay_report(&mut an, &req) {
        return index_cannot_answer(&why);
    }
    let parsed = ix.parsed;
    let mut reports = Vec::new();
    an.write_reports(&mut reports);
    let mut meta_body = parsed.meta_body.clone();
    if meta_body.last() == Some(&b'}') {
        meta_body.pop();
        meta_body.extend_from_slice(b",\"source\":\"index\"}");
    }
    let frame = report::Frame {
        meta_body: &meta_body,
        totals: parsed.totals,
        coverage: &parsed.coverage,
        limited_reasons: &[],
    };
    let out = report::render(&frame, &reports);
    if let Err(e) = write_output(&args.common.out, |w| {
        w.write_all(&out).and_then(|()| w.flush())
    }) {
        return e;
    }
    if parsed.coverage.incomplete_dirs > 0 {
        if !args.common.quiet {
            eprintln!(
                "warning: the index reports {} directories that were not fully read; see 'coverage'",
                parsed.coverage.incomplete_dirs
            );
        }
        return Exit::Incomplete;
    }
    Exit::Ok
}

/// 讓 `&mut dyn Write` 可以傳給需要具體 `Write` 型別的函式。
struct Wrap<'a>(&'a mut dyn Write);

impl Write for Wrap<'_> {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

fn request_of(args: &ReportArgs) -> report::Request {
    report::Request {
        top_files: args.analyses.top_files,
        top_dirs: args.analyses.top_dirs,
        by_ext: args.analyses.by_ext,
        many_files: args.analyses.many_files,
        min_size: args.min_size,
        exts: args.ext.as_ref().map(|v| {
            v.iter()
                .map(|e| e.encode_utf16().collect::<Vec<u16>>())
                .collect()
        }),
        older_than_days: args.older_than,
        dir_depth: args.dir_depth,
        now: timefmt::now_filetime(),
    }
}

fn run_report(args: ReportArgs) -> Exit {
    if let Err(e) = check_out(&args.common.out) {
        return e;
    }
    let req = request_of(&args);
    if let Some(path) = &args.index {
        return report_from_index(&args, path, req);
    }
    match &args.snapshot {
        Some(path) => report_from_snapshot(&args, path, req),
        None => report_live(&args, req),
    }
}

fn report_live(args: &ReportArgs, req: report::Request) -> Exit {
    let mut an = report::Analyzer::new(req);
    let live = match live_scan(&args.roots, &args.common, false, &mut an) {
        Ok(l) => l,
        Err(e) => return e,
    };
    let mut reports = Vec::new();
    an.write_reports(&mut reports);

    let incomplete = live.coverage.incomplete_dirs - live.cancelled;
    let meta = snapshot::Meta {
        tool_version: VERSION,
        started: live.started,
        finished: live.finished,
        engine: live.engine,
        elevated: sys::is_elevated(),
        options: live.used.clone(),
        roots: &live.meta_roots,
        volumes: volumes_of(&live.meta_roots),
    };
    let mut meta_body = Vec::new();
    snapshot::write_meta_object(&mut meta_body, &meta, false, Some("scan"));
    let frame = report::Frame {
        meta_body: &meta_body,
        totals: live.totals,
        coverage: &live.coverage,
        limited_reasons: &[],
    };
    let out = report::render(&frame, &reports);
    if let Err(e) = write_output(&args.common.out, |w| {
        w.write_all(&out).and_then(|()| w.flush())
    }) {
        return e;
    }
    log_done_parts(
        &args.common,
        live.dirs,
        live.files,
        live.duration_ms,
        live.threads,
        incomplete,
        live.cancelled,
    );
    Exit::from_scan(incomplete, live.cancelled)
}

fn report_from_snapshot(args: &ReportArgs, path: &Path, req: report::Request) -> Exit {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            snapshot_unreadable(path, &e);
            return Exit::Usage;
        }
    };
    let mut input = std::io::BufReader::with_capacity(1 << 20, file);
    let reasons_req = req.clone();
    let mut an = report::Analyzer::new(req.clone());
    // 旁邊有有效的索引、且請求它答得出來：只讀索引（輸出與讀快照逐位元組相同）
    let mut from_index = None;
    if let Some(ix) = load_index(path, args.common.quiet) {
        match ix.replay_report(&mut an, &req) {
            Ok(()) => from_index = Some(ix.parsed),
            Err(why) => {
                index_not_used(&why, args.common.quiet);
                an = report::Analyzer::new(req.clone());
            }
        }
    }
    let parsed = match from_index {
        Some(p) => p,
        None => match snapread::read(&mut input, &mut an) {
            Ok(p) => p,
            Err(m) => {
                snapshot_invalid(path, &m);
                return Exit::Usage;
            }
        },
    };
    let mut reports = Vec::new();
    an.write_reports(&mut reports);

    let mut meta_body = parsed.meta_body.clone();
    if meta_body.last() == Some(&b'}') {
        meta_body.pop();
        meta_body.extend_from_slice(b",\"source\":\"snapshot\"}");
    }
    let reasons = report::limited_reasons(parsed.min_size, parsed.max_depth, &reasons_req);
    let frame = report::Frame {
        meta_body: &meta_body,
        totals: parsed.totals,
        coverage: &parsed.coverage,
        limited_reasons: &reasons,
    };
    let out = report::render(&frame, &reports);
    if let Err(e) = write_output(&args.common.out, |w| {
        w.write_all(&out).and_then(|()| w.flush())
    }) {
        return e;
    }
    // 快照本身若有沒讀到的目錄，結果同樣要提醒呼叫者（結束代碼 2）
    if parsed.coverage.incomplete_dirs > 0 {
        if !args.common.quiet {
            eprintln!(
                "warning: the snapshot reports {} directories that were not fully read; see 'coverage'",
                parsed.coverage.incomplete_dirs
            );
        }
        return Exit::Incomplete;
    }
    Exit::Ok
}

/// 載入快照旁的索引；不能用時印一行說明（`-q` 不印），回傳 None 讓呼叫端讀整份快照。
fn load_index(snapshot_path: &Path, quiet: bool) -> Option<index::Index> {
    match index::Index::load(snapshot_path) {
        Ok(ix) => Some(ix),
        Err(why) => {
            index_not_used(&why, quiet);
            None
        }
    }
}

fn index_not_used(why: &str, quiet: bool) {
    if !quiet {
        eprintln!("index: not used ({why})");
    }
}

fn snapshot_unreadable(path: &Path, e: &std::io::Error) {
    fail::print(
        "snapshot_unreadable",
        &format!("cannot open snapshot {}: {e}", path.display()),
        "check the path; make a snapshot first with 'voltdir scan <PATH> --out <FILE>'",
    );
}

fn snapshot_invalid(path: &Path, why: &str) {
    fail::print(
        "snapshot_invalid",
        &format!("invalid snapshot {}: {why}", path.display()),
        "the file must be a complete snapshot written by 'voltdir scan'; if a scan was interrupted, scan again",
    );
}

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// 把命令列的篩選條件轉成比對條件；正規表示式不合法時回報用法錯誤。
fn filter_of(args: &FindArgs) -> Result<find::Filter, String> {
    let now = timefmt::now_filetime();
    let name_regex = match &args.name_regex {
        Some(p) => {
            Some(regex::Regex::new(&utf16(p)).map_err(|e| format!("invalid --name-regex: {e}"))?)
        }
        None => None,
    };
    Ok(find::Filter {
        name_glob: args.name.as_deref().map(utf16),
        name_regex,
        path_glob: args.path.as_deref().map(utf16),
        kind: match args.kind {
            cli::FindKind::File => find::Kind::File,
            cli::FindKind::Dir => find::Kind::Dir,
            cli::FindKind::Any => find::Kind::Any,
        },
        exts: args
            .ext
            .as_ref()
            .map(|v| v.iter().map(|e| utf16(e)).collect()),
        min_size: args.min_size,
        max_size: args.max_size,
        older_than: args.older_than.map(|d| find::Filter::cutoff(now, d)),
        newer_than: args.newer_than.map(|d| find::Filter::cutoff(now, d)),
        empty: args.empty,
    })
}

fn run_find(args: FindArgs) -> Exit {
    if let Err(e) = check_out(&args.common.out) {
        return e;
    }
    let filter = match filter_of(&args) {
        Ok(f) => f,
        Err(m) => {
            fail::print(
                "invalid_pattern",
                &m,
                "supported regex syntax: literals, . [] [^] ? * + {n,m} | () ^ $ and \\d \\w \\s; run 'voltdir help find'",
            );
            return Exit::Usage;
        }
    };
    let kind = filter.kind;
    let sort = match args.sort {
        cli::FindSort::Path => find::Sort::Path,
        cli::FindSort::Mtime => find::Sort::Mtime,
        cli::FindSort::Size => find::Sort::Size,
    };
    let needs_ext = filter.exts.is_some();
    let matcher = find::Matcher::new(filter, sort, args.limit);
    match &args.snapshot {
        Some(path) => {
            // 名稱索引只是加速器：任何一步失敗，都用全新的篩選器改走串流讀快照
            let fresh = || {
                filter_of(&args)
                    .map(|f| find::Matcher::new(f, sort, args.limit))
                    .expect("the pattern was already validated")
            };
            if let Some(exit) = find_with_name_index(&args, path, kind, &fresh) {
                return exit;
            }
            find_from_snapshot(&args, path, fresh(), kind)
        }
        None => find_live(&args, matcher, needs_ext),
    }
}

/// 用 `<快照>.nidx` 回答 `find --snapshot`。回傳 `None` 表示索引不能用（原因已印在 stderr），
/// 呼叫端改走串流路徑；結果與串流路徑逐位元組相同。
fn find_with_name_index(
    args: &FindArgs,
    path: &Path,
    kind: find::Kind,
    fresh: &dyn Fn() -> find::Matcher,
) -> Option<Exit> {
    let quiet = args.common.quiet;
    let not_used = |why: &str| {
        if !quiet {
            eprintln!("index: not used ({why})");
        }
    };
    let ix = match nidx::NameIndex::load(path) {
        Ok(ix) => ix,
        Err(why) => {
            not_used(&why);
            return None;
        }
    };
    // 外框（meta、涵蓋範圍）用串流路徑現有的程式碼讀：只讀第一行與結尾的 summary 行
    let open = || -> Result<snapread::Parsed, String> {
        let file =
            std::fs::File::open(path).map_err(|e| format!("cannot open the snapshot: {e}"))?;
        let len = file.metadata().map_err(|e| e.to_string())?.len();
        let mut input = std::io::BufReader::with_capacity(1 << 16, file);
        let mut parsed = snapread::Reader::open(&mut input)?.meta().clone();
        let (totals, coverage) = snapread::parse_summary(&index::summary_line(path, len)?)?;
        parsed.totals = totals;
        parsed.coverage = coverage;
        Ok(parsed)
    };
    let parsed = match open() {
        Ok(p) => p,
        Err(why) => {
            not_used(&why);
            return None;
        }
    };
    let mut matcher = fresh();
    if let Err(why) = ix.query(path, &mut matcher) {
        not_used(&why);
        return None;
    }
    if !quiet {
        let age = nidx::snapshot_age_seconds(path)
            .map_or_else(String::new, |s| format!("built {} ago", nidx::age_text(s)));
        eprintln!("index: used ({age})");
    }
    Some(emit_snapshot_find(args, matcher, &parsed, kind))
}

fn find_live(args: &FindArgs, mut matcher: find::Matcher, needs_ext: bool) -> Exit {
    let s = match prepare_scan(&args.roots, &args.common, None, None) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let strip = |mut b: Vec<u8>| {
        while b.last() == Some(&b'\n') {
            b.pop();
        }
        b
    };
    snapshot::for_each_entry(&s.data, &mut |e| match e {
        snapshot::Entry::Dir { path, node, depth } => {
            let name = &node.name;
            matcher.offer(
                &find::Cand {
                    is_dir: true,
                    path,
                    name,
                    ext: &[],
                    logical: node.rollup.logical,
                    physical: node.rollup.physical,
                    mtime: node.latest_mtime,
                    dir_empty: node.rollup.files == 0 && node.rollup.dirs == 0,
                },
                || {
                    let mut b = Vec::new();
                    snapshot::write_dir(&mut b, path, node, depth);
                    strip(b)
                },
            );
        }
        snapshot::Entry::File {
            path,
            dir,
            file,
            depth,
        } => {
            let name = dir.file_name(file);
            let ext = if needs_ext {
                names::ext_of(name)
            } else {
                Vec::new()
            };
            matcher.offer(
                &find::Cand {
                    is_dir: false,
                    path,
                    name,
                    ext: &ext,
                    logical: file.logical,
                    physical: file.physical,
                    mtime: file.mtime,
                    dir_empty: false,
                },
                || {
                    let mut b = Vec::new();
                    snapshot::write_file(&mut b, path, name, file, depth);
                    strip(b)
                },
            );
        }
    });
    let (matched, items, truncated) = matcher.finish();

    let (coverage, cancelled) = snapshot::count_coverage(&s.data);
    let incomplete = coverage.incomplete_dirs - cancelled;
    let meta = snapshot::Meta {
        tool_version: VERSION,
        started: s.started,
        finished: s.finished,
        engine: s.engine,
        elevated: sys::is_elevated(),
        options: s.used.clone(),
        roots: &s.meta_roots,
        volumes: volumes_of(&s.meta_roots),
    };
    let mut meta_body = Vec::new();
    snapshot::write_meta_object(&mut meta_body, &meta, false, Some("scan"));
    let out = find::render(&find::Frame {
        meta_body: &meta_body,
        matched_total: matched,
        items: &items,
        truncated,
        coverage: &coverage,
        limited_reasons: &[],
    });
    if let Err(e) = write_output(&args.common.out, |w| {
        w.write_all(&out).and_then(|()| w.flush())
    }) {
        return e;
    }
    log_done(&args.common, &s, incomplete, cancelled);
    Exit::from_scan(incomplete, cancelled)
}

fn find_from_snapshot(
    args: &FindArgs,
    path: &Path,
    mut matcher: find::Matcher,
    kind: find::Kind,
) -> Exit {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            snapshot_unreadable(path, &e);
            return Exit::Usage;
        }
    };
    let mut input = std::io::BufReader::with_capacity(1 << 20, file);
    let invalid = |m: String| {
        snapshot_invalid(path, &m);
        Exit::Usage
    };
    let mut reader = match snapread::Reader::open(&mut input) {
        Ok(r) => r,
        Err(m) => return invalid(m),
    };
    let mut rec = snapread::Rec::default();
    loop {
        match reader.next_into(&mut rec) {
            Ok(true) => {
                let raw = reader.raw();
                matcher.offer(
                    &find::Cand {
                        is_dir: rec.is_dir,
                        path: &rec.path,
                        name: &rec.name,
                        ext: &rec.ext,
                        logical: rec.logical,
                        physical: rec.physical,
                        mtime: rec.mtime,
                        dir_empty: rec.rollup.files == 0 && rec.rollup.dirs == 0,
                    },
                    || raw.to_vec(),
                );
            }
            Ok(false) => break,
            Err(m) => return invalid(m),
        }
    }
    let parsed = match reader.finish() {
        Ok(p) => p,
        Err(m) => return invalid(m),
    };
    emit_snapshot_find(args, matcher, &parsed, kind)
}

/// 讀快照（串流或名稱索引）之後共用的輸出：外框、`limited_*`、涵蓋範圍與結束代碼。
fn emit_snapshot_find(
    args: &FindArgs,
    matcher: find::Matcher,
    parsed: &snapread::Parsed,
    kind: find::Kind,
) -> Exit {
    let (matched, items, truncated) = matcher.finish();
    let mut meta_body = parsed.meta_body.clone();
    if meta_body.last() == Some(&b'}') {
        meta_body.pop();
        meta_body.extend_from_slice(b",\"source\":\"snapshot\"}");
    }
    let reasons = find::limited_reasons(parsed.min_size, parsed.max_depth, kind);
    let out = find::render(&find::Frame {
        meta_body: &meta_body,
        matched_total: matched,
        items: &items,
        truncated,
        coverage: &parsed.coverage,
        limited_reasons: &reasons,
    });
    if let Err(e) = write_output(&args.common.out, |w| {
        w.write_all(&out).and_then(|()| w.flush())
    }) {
        return e;
    }
    if parsed.coverage.incomplete_dirs > 0 {
        if !args.common.quiet {
            eprintln!(
                "warning: the snapshot reports {} directories that were not fully read; see 'coverage'",
                parsed.coverage.incomplete_dirs
            );
        }
        return Exit::Incomplete;
    }
    Exit::Ok
}

/// `voltdir index <SNAPSHOT>`：串流讀一份既有的快照，替它補寫名稱索引。
fn run_index(args: cli::IndexArgs) -> Exit {
    let path = &args.snapshot;
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            snapshot_unreadable(path, &e);
            return Exit::Usage;
        }
    };
    let mut input = std::io::BufReader::with_capacity(1 << 20, file);
    let invalid = |m: String| {
        snapshot_invalid(path, &m);
        Exit::Usage
    };
    let mut reader = match snapread::Reader::open(&mut input) {
        Ok(r) => r,
        Err(m) => return invalid(m),
    };
    let mut rec = snapread::Rec::default();
    let mut b = nidx::Builder::new();
    loop {
        match reader.next_into(&mut rec) {
            Ok(true) => {
                // 本工具寫的快照，根的 name 就是它的顯示路徑；不是這樣的快照不建索引
                if rec.is_dir && rec.depth == 0 && rec.path != rec.name {
                    fail::print(
                        "name_index_unsupported",
                        "a root folder's name differs from its path, so this snapshot was not written by this tool",
                        "use 'find --snapshot' without the index, or scan again",
                    );
                    return Exit::Usage;
                }
                let (off, len) = reader.line_span();
                b.push(&nidx::Row {
                    is_dir: rec.is_dir,
                    depth: rec.depth,
                    name: &rec.name,
                    ext: &rec.ext,
                    logical: rec.logical,
                    physical: rec.physical,
                    mtime: rec.mtime,
                    dir_empty: rec.is_dir && rec.rollup.files == 0 && rec.rollup.dirs == 0,
                    line_off: off,
                    line_len: len,
                });
            }
            Ok(false) => break,
            Err(m) => return invalid(m),
        }
    }
    if let Err(m) = reader.finish() {
        return invalid(m);
    }
    let entries = b.len();
    let bytes = match b.finish(path) {
        Ok(b) => b,
        Err(why) => {
            fail::print(
                "name_index_unsupported",
                &format!("cannot build the name index: {why}"),
                "use 'find --snapshot' without the index, or scan again",
            );
            return Exit::Usage;
        }
    };
    let target = nidx::path_of(path);
    if let Err(e) = write_atomic(&target, |w| w.write_all(&bytes)) {
        fail::print(
            "output_failed",
            &format!("cannot write {}: {e}", target.display()),
            "choose a snapshot in a folder you can write to",
        );
        return Exit::OutputFailed;
    }
    if !args.quiet {
        eprintln!(
            "name index written: {} ({entries} entries, {} bytes)",
            target.display(),
            bytes.len()
        );
    }
    Exit::Ok
}

/// `--a-root`／`--b-root` 的值：斜線統一成反斜線（快照裡的路徑都是反斜線）。
fn root_arg(p: &Path) -> Vec<u16> {
    utf16(&p.to_string_lossy().replace('/', "\\"))
}

fn run_diff(args: DiffArgs) -> Exit {
    if let Err(e) = check_out(&args.out) {
        return e;
    }
    let opt = diff::Options {
        top: args.top,
        min_change: args.min_change,
        under: args.under.as_deref().map(utf16),
        rank_rollup: args.rank_rollup,
        a_root: args.a_root.as_deref().map(root_arg),
        b_root: args.b_root.as_deref().map(root_arg),
        match_roots: args.match_roots,
    };
    let open = |p: &Path| {
        let p = p.to_path_buf();
        move || -> Result<std::io::BufReader<std::fs::File>, String> {
            std::fs::File::open(&p)
                .map(|f| std::io::BufReader::with_capacity(1 << 20, f))
                .map_err(|e| format!("{} {}: {e}", diff::MSG_OPEN, p.display()))
        }
    };
    let outcome = match diff::compare(open(&args.a), open(&args.b), &opt) {
        Ok(o) => o,
        Err(m) => {
            let (code, hint) = diff::classify(&m);
            fail::print(code, &m, hint);
            return Exit::Usage;
        }
    };
    let incomplete = outcome.a.coverage.incomplete_dirs + outcome.b.coverage.incomplete_dirs;
    let out = diff::render(outcome);
    if let Err(e) = write_output(&args.out, |w| w.write_all(&out).and_then(|()| w.flush())) {
        return e;
    }
    if incomplete > 0 {
        if !args.quiet {
            eprintln!(
                "warning: the snapshots report {incomplete} directories that were not fully read; the comparison may miss changes there"
            );
        }
        return Exit::Incomplete;
    }
    Exit::Ok
}

fn overview_options(args: &OverviewArgs) -> overview::Options {
    overview::Options {
        top: args.top,
        ext_top: args.ext_top,
        stale_days: args.stale_days,
        stale_min_size: args.stale_min_size,
        rank_logical: args.rank_logical,
        dir_direct: args.dir_direct,
        auto_trim: !args.top_given && !args.full,
        now: timefmt::now_filetime(),
    }
}

/// 單一掃描根且 concise 時，清單路徑相對於該根。
fn relative_base<'a>(args: &OverviewArgs, roots: &'a [Vec<u16>]) -> Option<&'a [u16]> {
    (!args.full && roots.len() == 1).then(|| roots[0].as_slice())
}

fn quoted(p: &Path) -> String {
    format!("\"{}\"", p.display())
}

fn run_overview(args: OverviewArgs) -> Exit {
    if let Err(e) = check_out(&args.common.out) {
        return e;
    }
    if let Err(e) = check_out(&args.save_snapshot) {
        return e;
    }
    if let Err(e) = check_out(&args.save_index) {
        return e;
    }
    if let Some(path) = &args.index {
        return overview_from_index(&args, path);
    }
    match &args.snapshot {
        Some(path) => overview_from_snapshot(&args, path),
        None => overview_live(&args),
    }
}

fn overview_live(args: &OverviewArgs) -> Exit {
    let mut col = overview::Collector::new(overview_options(args));
    // --save-index：串流掃描同時把彙總索引建起來，不保留逐筆項目、不寫快照
    let mut builder = index::Builder::new();
    let scanned = if args.save_index.is_some() {
        live_scan(
            &args.roots,
            &args.common,
            false,
            &mut Tee(&mut col, &mut builder),
        )
    } else {
        live_scan(
            &args.roots,
            &args.common,
            args.save_snapshot.is_some(),
            &mut col,
        )
    };
    let s = match scanned {
        Ok(s) => s,
        Err(e) => return e,
    };
    let (coverage, cancelled) = (&s.coverage, s.cancelled);
    let incomplete = coverage.incomplete_dirs - cancelled;
    let meta = snapshot::Meta {
        tool_version: VERSION,
        started: s.started,
        finished: s.finished,
        engine: s.engine,
        elevated: sys::is_elevated(),
        options: s.used.clone(),
        roots: &s.meta_roots,
        volumes: volumes_of(&s.meta_roots),
    };
    if let (Some(save), Some(data)) = (&args.save_snapshot, &s.data) {
        let emit = snapshot::Emit {
            min_size: None,
            max_depth: None,
        };
        let mut written = None;
        if let Err(e) = write_output(&Some(save.clone()), |w| {
            written = Some(snapshot::write_snapshot(
                data,
                &meta,
                emit,
                s.duration_ms,
                &mut Wrap(w),
            )?);
            Ok(())
        }) {
            return e;
        }
        if !args.common.no_index
            && let Some(o) = &written
        {
            let emit = snapshot::Emit {
                min_size: None,
                max_depth: None,
            };
            write_index(save, data, emit, &o.layout, args.common.quiet);
        }
    }
    if let Some(path) = &args.save_index {
        let mut mb = Vec::new();
        snapshot::write_meta_object(&mut mb, &meta, false, None);
        let mut started = Vec::new();
        timefmt::push_filetime(&mut started, s.started);
        let hdr = index::Header {
            snapshot: None,
            meta_body: String::from_utf8_lossy(&mb).into_owned(),
            min_size: None,
            max_depth: None,
            engine: s.engine,
            started: &String::from_utf8_lossy(&started),
            roots: &s.meta_roots,
            totals: s.totals,
            coverage: &s.coverage,
        };
        let bytes = builder.finish(&hdr);
        if let Err(e) = write_output(&Some(path.clone()), |w| {
            w.write_all(&bytes).and_then(|()| w.flush())
        }) {
            return e;
        }
    }
    let meta_body = args.full.then(|| {
        let mut b = Vec::new();
        snapshot::write_meta_object(&mut b, &meta, false, Some("scan"));
        b
    });
    let input: String = match &args.save_snapshot {
        Some(save) => format!("--snapshot {}", quoted(save)),
        None => s
            .meta_roots
            .iter()
            .map(|r| format!("\"{}\"", String::from_utf16_lossy(r)))
            .collect::<Vec<_>>()
            .join(" "),
    };
    let from_snapshot = args.save_snapshot.is_some();
    let roots_quoted = input.clone();
    let index_arg = args.save_index.as_ref().map(|p| quoted(p));
    let next = |first: Option<&str>| match &index_arg {
        Some(ix) => overview::next_commands_index(ix, &roots_quoted),
        None => overview::next_commands(&input, from_snapshot, first),
    };
    let frame = overview::Frame {
        roots: &s.meta_roots,
        engine: s.engine,
        totals: s.totals,
        coverage,
        limited_reasons: &[],
        meta_body: meta_body.as_deref(),
        next: &next,
    };
    let out = col.render(&frame, relative_base(args, &s.meta_roots));
    if let Err(e) = write_output(&args.common.out, |w| {
        w.write_all(&out).and_then(|()| w.flush())
    }) {
        return e;
    }
    log_done_parts(
        &args.common,
        s.dirs,
        s.files,
        s.duration_ms,
        s.threads,
        incomplete,
        cancelled,
    );
    Exit::from_scan(incomplete, cancelled)
}

fn overview_from_snapshot(args: &OverviewArgs, path: &Path) -> Exit {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            snapshot_unreadable(path, &e);
            return Exit::Usage;
        }
    };
    let mut input = std::io::BufReader::with_capacity(1 << 20, file);
    let mut col = overview::Collector::new(overview_options(args));
    // 旁邊有有效的索引、且請求它答得出來：只讀索引（輸出與讀快照逐位元組相同）
    let mut from_index = None;
    if let Some(ix) = load_index(path, args.common.quiet) {
        match ix.replay_overview(&mut col, &overview_options(args)) {
            Ok(()) => from_index = Some(ix.parsed),
            Err(why) => {
                index_not_used(&why, args.common.quiet);
                col = overview::Collector::new(overview_options(args));
            }
        }
    }
    let source = if from_index.is_some() {
        "index"
    } else {
        "snapshot"
    };
    let parsed = match from_index {
        Some(p) => p,
        None => match snapread::read(&mut input, &mut col) {
            Ok(p) => p,
            Err(m) => {
                snapshot_invalid(path, &m);
                return Exit::Usage;
            }
        },
    };
    let all = report::Request {
        top_files: Some(1),
        top_dirs: Some(1),
        by_ext: Some(1),
        ..Default::default()
    };
    let reasons = report::limited_reasons(parsed.min_size, parsed.max_depth, &all);
    let meta_body = args.full.then(|| {
        let mut b = parsed.meta_body.clone();
        if b.last() == Some(&b'}') {
            b.pop();
            b.extend_from_slice(format!(",\"source\":\"{source}\"}}").as_bytes());
        }
        b
    });
    let input_arg = format!("--snapshot {}", quoted(path));
    let next = |first: Option<&str>| overview::next_commands(&input_arg, true, first);
    let frame = overview::Frame {
        roots: &parsed.roots,
        engine: &parsed.engine,
        totals: parsed.totals,
        coverage: &parsed.coverage,
        limited_reasons: &reasons,
        meta_body: meta_body.as_deref(),
        next: &next,
    };
    let out = col.render(&frame, relative_base(args, &parsed.roots));
    if let Err(e) = write_output(&args.common.out, |w| {
        w.write_all(&out).and_then(|()| w.flush())
    }) {
        return e;
    }
    if parsed.coverage.incomplete_dirs > 0 {
        if !args.common.quiet {
            eprintln!(
                "warning: the snapshot reports {} directories that were not fully read; see 'coverage'",
                parsed.coverage.incomplete_dirs
            );
        }
        return Exit::Incomplete;
    }
    Exit::Ok
}

/// 先寫同資料夾的暫存檔，成功後才改名；失敗不留下半成品。
fn write_atomic(
    path: &Path,
    body: impl FnOnce(&mut std::io::BufWriter<std::fs::File>) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no file name"))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(".voltdir-tmp-{}", std::process::id()));
    let tmp = path.with_file_name(tmp_name);
    let result = (|| {
        let file = std::fs::File::create(&tmp)?;
        let mut w = std::io::BufWriter::with_capacity(1 << 20, file);
        body(&mut w)?;
        w.flush()?;
        drop(w);
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn version_line_has_expected_format() {
        let line = version_line();
        let version = line.strip_prefix("voltdir ").expect("prefix");
        let parts: Vec<&str> = version.split('.').collect();
        assert_eq!(parts.len(), 3, "semver x.y.z: {version}");
        assert!(parts.iter().all(|p| p.parse::<u32>().is_ok()));
    }

    #[test]
    fn exit_codes_match_spec_4_2() {
        assert_eq!(Exit::Ok.code(), 0);
        assert_eq!(Exit::Usage.code(), 1);
        assert_eq!(Exit::Incomplete.code(), 2);
        assert_eq!(Exit::EngineUnavailable.code(), 3);
        assert_eq!(Exit::OutputFailed.code(), 4);
        assert_eq!(Exit::Cancelled.code(), 5);
        assert_eq!(Exit::Internal.code(), 99);
    }

    #[test]
    fn scan_outcome_maps_to_exit_code() {
        assert_eq!(Exit::from_scan(0, 0), Exit::Ok);
        assert_eq!(Exit::from_scan(3, 0), Exit::Incomplete);
        assert_eq!(Exit::from_scan(0, 2), Exit::Cancelled);
        assert_eq!(Exit::from_scan(3, 2), Exit::Cancelled);
    }

    #[test]
    fn root_containment() {
        assert!(contains_root(&w(r"C:\a"), &w(r"c:\A\b")));
        assert!(!contains_root(&w(r"C:\a"), &w(r"C:\ab")));
        assert!(contains_root(&w(r"C:\"), &w(r"C:\x")));
        assert!(!contains_root(&w(r"C:\a"), &w(r"D:\a")));
    }

    #[test]
    fn nested_and_duplicate_roots_are_dropped_once() {
        let (keep, dropped) =
            effective_roots(vec![w(r"C:\a\b"), w(r"C:\a"), w(r"D:\x"), w(r"c:\A")]);
        assert_eq!(keep, vec![w(r"C:\a"), w(r"D:\x")]);
        assert_eq!(dropped, vec![w(r"C:\a\b"), w(r"c:\A")]);
        // 完全相同的兩個根只保留第一個
        let (keep, dropped) = effective_roots(vec![w(r"C:\a"), w(r"C:\a")]);
        assert_eq!(keep, vec![w(r"C:\a")]);
        assert_eq!(dropped.len(), 1);
    }

    #[test]
    fn display_path_trims_trailing_separator_but_keeps_drive_root() {
        let d = |s: &str| String::from_utf16_lossy(&display_path(Path::new(s)).unwrap());
        assert_eq!(d(r"C:\"), r"C:\");
        assert_eq!(d(r"C:\Windows\"), r"C:\Windows");
        assert_eq!(d(r"\\?\C:\x"), r"C:\x");
    }
}
