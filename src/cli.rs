//! 命令列解析（不依賴第三方套件）。

use crate::help;
use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Version,
    /// 要印到標準輸出的說明文字
    Help(String),
    Info,
    Scan(ScanArgs),
    Report(ReportArgs),
    Find(FindArgs),
    Diff(DiffArgs),
    Overview(OverviewArgs),
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Engine {
    Auto,
    Mft,
    Walk,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum FollowKind {
    Mount,
    Symlink,
    Junction,
}

impl FollowKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FollowKind::Mount => "mount",
            FollowKind::Symlink => "symlink",
            FollowKind::Junction => "junction",
        }
    }
}

/// `scan` 與 `report` 共用的掃描與輸出選項。
#[derive(Debug, PartialEq, Eq)]
pub struct Common {
    /// `None` 表示標準輸出
    pub out: Option<PathBuf>,
    /// 使用者明確寫了 `--out -`
    pub out_dash: bool,
    pub engine: Engine,
    pub threads: Option<usize>,
    pub quiet: bool,
    pub log_json: bool,
    pub exclude: Vec<OsString>,
    pub follow: Vec<FollowKind>,
    pub hardlinks_dedupe: bool,
    pub timeout_sec: Option<u64>,
    /// 使用者明確給了哪些「只適用於實際掃描」的選項（`--snapshot` 時用來拒絕）
    pub scan_only_given: Vec<String>,
    /// `--no-index`：寫快照時不要同時寫彙總索引
    pub no_index: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ScanArgs {
    pub roots: Vec<PathBuf>,
    pub common: Common,
    pub min_size: Option<u64>,
    pub max_depth: Option<u64>,
}

#[derive(Debug, PartialEq, Eq, Default)]
pub struct Analyses {
    pub top_files: Option<usize>,
    pub top_dirs: Option<usize>,
    pub by_ext: Option<usize>,
    pub many_files: Option<usize>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct OverviewArgs {
    pub roots: Vec<PathBuf>,
    pub snapshot: Option<PathBuf>,
    pub common: Common,
    pub top: usize,
    pub ext_top: usize,
    pub stale_days: u64,
    pub stale_min_size: u64,
    pub rank_logical: bool,
    /// `--dir-rank direct`
    pub dir_direct: bool,
    /// 使用者明確給了 `--top` 或 `--ext-top`（自動收斂輸出大小時要尊重）
    pub top_given: bool,
    pub full: bool,
    pub save_snapshot: Option<PathBuf>,
    /// `--save-index`：串流掃描並只寫索引
    pub save_index: Option<PathBuf>,
    /// `--index`：只讀索引回答
    pub index: Option<PathBuf>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ReportArgs {
    pub roots: Vec<PathBuf>,
    pub snapshot: Option<PathBuf>,
    /// `--index`：只讀索引回答
    pub index: Option<PathBuf>,
    pub common: Common,
    pub analyses: Analyses,
    /// `--top-files` 的篩選
    pub min_size: Option<u64>,
    pub ext: Option<Vec<String>>,
    pub older_than: Option<u64>,
    pub dir_depth: Option<u64>,
}

/// `find` 的篩選與輸出控制（原樣保存，由 `main` 轉成比對條件）。
#[derive(Debug, PartialEq, Eq)]
pub struct FindArgs {
    pub roots: Vec<PathBuf>,
    pub snapshot: Option<PathBuf>,
    pub common: Common,
    pub name: Option<String>,
    pub name_regex: Option<String>,
    pub path: Option<String>,
    pub kind: FindKind,
    pub ext: Option<Vec<String>>,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    pub older_than: Option<u64>,
    pub newer_than: Option<u64>,
    pub empty: bool,
    pub limit: usize,
    pub sort: FindSort,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum FindKind {
    File,
    Dir,
    Any,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum FindSort {
    Path,
    Size,
    Mtime,
}

#[derive(Debug, PartialEq, Eq)]
pub struct DiffArgs {
    pub a: PathBuf,
    pub b: PathBuf,
    pub top: usize,
    pub min_change: u64,
    pub under: Option<String>,
    pub rank_rollup: bool,
    /// 跨路徑比較；成對與衝突的檢查在 `diff::compare`，才能附上正確的 hint
    pub a_root: Option<PathBuf>,
    pub b_root: Option<PathBuf>,
    pub match_roots: bool,
    pub out: Option<PathBuf>,
    pub quiet: bool,
    /// 錯誤格式在 `main` 開頭就由 `fail::detect_json` 決定；這裡只記錄使用者的選擇
    #[allow(dead_code)]
    pub log_json: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CliError {
    /// 用法錯誤
    Usage(String),
    /// 已規劃、但此版本尚未提供的功能
    Unsupported(String),
}

/// 命令列裡有沒有 `--help` 或 `-h`（子命令之後的任何位置）。
fn wants_help(args: &[OsString]) -> bool {
    args.iter().any(|a| a == "--help" || a == "-h")
}

/// 解析大小：純數字（位元組）或帶單位 K/M/G/T（1000 進位）、KiB/MiB/GiB/TiB（1024 進位），不分大小寫。
pub fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    if num.is_empty() {
        return Err(format!("invalid size '{s}'"));
    }
    let n: u64 = num.parse().map_err(|_| format!("invalid size '{s}'"))?;
    let mult: u64 = match unit.to_ascii_lowercase().as_str() {
        "" => 1,
        "k" => 1_000,
        "m" => 1_000_000,
        "g" => 1_000_000_000,
        "t" => 1_000_000_000_000,
        "kib" => 1 << 10,
        "mib" => 1 << 20,
        "gib" => 1 << 30,
        "tib" => 1 << 40,
        _ => return Err(format!("invalid size unit in '{s}'")),
    };
    n.checked_mul(mult)
        .ok_or_else(|| format!("size '{s}' is too large"))
}

struct FindOpts {
    name: Option<String>,
    name_regex: Option<String>,
    path: Option<String>,
    kind: FindKind,
    max_size: Option<u64>,
    newer_than: Option<u64>,
    empty: bool,
    limit: usize,
    sort: FindSort,
}

impl Default for FindOpts {
    fn default() -> Self {
        FindOpts {
            name: None,
            name_regex: None,
            path: None,
            kind: FindKind::File,
            max_size: None,
            newer_than: None,
            empty: false,
            limit: 100,
            sort: FindSort::Path,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Scan,
    Report,
    Find,
    Overview,
}

pub fn parse(args: Vec<OsString>) -> Result<Command, CliError> {
    let usage = |m: &str| CliError::Usage(m.to_string());
    let mut it = args.into_iter();
    let Some(first) = it.next() else {
        return Err(usage("no command given"));
    };
    let first = first.to_string_lossy().into_owned();
    let rest: Vec<OsString> = it.collect();
    let it = rest.clone().into_iter();
    if matches!(
        first.as_str(),
        "info" | "scan" | "report" | "find" | "diff" | "overview"
    ) && wants_help(&rest)
    {
        return Ok(Command::Help(
            help::text(&first).expect("every command has help"),
        ));
    }
    match first.as_str() {
        "--version" | "-V" => Ok(Command::Version),
        "--help" | "-h" => Ok(Command::Help(help::GENERAL.to_string())),
        "help" => match rest.first().map(|a| a.to_string_lossy().into_owned()) {
            None => Ok(Command::Help(help::GENERAL.to_string())),
            Some(name) => match help::text(&name) {
                Some(t) => Ok(Command::Help(t)),
                None => Err(CliError::Usage(format!("no help for '{name}'"))),
            },
        },
        "info" => {
            for a in it {
                if a != "--json" {
                    return Err(CliError::Usage(format!(
                        "unknown option for info: {}",
                        a.to_string_lossy()
                    )));
                }
            }
            Ok(Command::Info)
        }
        "scan" => parse_scan_or_report(it.collect(), Kind::Scan),
        "report" => parse_scan_or_report(it.collect(), Kind::Report),
        "find" => parse_scan_or_report(it.collect(), Kind::Find),
        "overview" => parse_scan_or_report(it.collect(), Kind::Overview),
        "diff" => parse_diff(it.collect()),
        "dupes" => Err(CliError::Unsupported(format!(
            "command '{first}' is not available in this version"
        ))),
        other => Err(CliError::Usage(format!("unknown command '{other}'"))),
    }
}

/// 數值選項超出範圍的訊息片段；`usage_code` 用它辨認這類錯誤。
const RANGE_MSG: &str = "expects an integer between";

/// 用法錯誤的穩定代碼。
pub fn usage_code(message: &str) -> &'static str {
    if message.contains(RANGE_MSG) {
        "value_out_of_range"
    } else if message.starts_with("unknown option") {
        "unknown_option"
    } else if message.starts_with("unknown command") {
        "unknown_command"
    } else {
        "usage"
    }
}

fn int_arg(name: &str, v: &OsString, min: u64, max: u64) -> Result<u64, CliError> {
    let n: u64 = v
        .to_string_lossy()
        .parse()
        .map_err(|_| CliError::Usage(format!("{name} {RANGE_MSG} {min} and {max}")))?;
    if n < min || n > max {
        return Err(CliError::Usage(format!(
            "{name} {RANGE_MSG} {min} and {max}"
        )));
    }
    Ok(n)
}

fn parse_scan_or_report(args: Vec<OsString>, kind: Kind) -> Result<Command, CliError> {
    let mut common = Common {
        out: None,
        out_dash: false,
        engine: Engine::Auto,
        threads: None,
        quiet: false,
        log_json: false,
        exclude: Vec::new(),
        follow: Vec::new(),
        hardlinks_dedupe: true,
        timeout_sec: None,
        scan_only_given: Vec::new(),
        no_index: false,
    };
    let mut roots = Vec::new();
    let mut snapshot: Option<PathBuf> = None;
    let mut analyses = Analyses::default();
    let (mut min_size, mut max_depth) = (None, None);
    let (mut ext, mut older_than, mut dir_depth) = (None, None, None);
    let mut find = FindOpts::default();
    let (mut top, mut ext_top, mut stale_days) = (10usize, 8usize, 365u64);
    let mut stale_min_size = 100u64 * 1024 * 1024;
    let (mut rank_logical, mut full, mut save_snapshot) = (false, false, None);
    let (mut dir_direct, mut top_given) = (false, false);
    let (mut save_index, mut index_in): (Option<PathBuf>, Option<PathBuf>) = (None, None);

    let mut i = 0;
    // 取得選項值：支援 `--opt value` 與 `--opt=value`
    let value = |name: &str, inline: Option<String>, i: &mut usize| -> Result<OsString, CliError> {
        if let Some(v) = inline {
            return Ok(OsString::from(v));
        }
        *i += 1;
        args.get(*i)
            .cloned()
            .ok_or_else(|| CliError::Usage(format!("option {name} requires a value")))
    };
    while i < args.len() {
        let raw = args[i].to_string_lossy().into_owned();
        if !raw.starts_with('-') || raw == "-" {
            roots.push(PathBuf::from(&args[i]));
            i += 1;
            continue;
        }
        let (name, inline) = match raw.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_string(), Some(v.to_string())),
            _ => (raw.clone(), None),
        };
        let only_report = |what: &str| -> Result<(), CliError> {
            if kind == Kind::Report {
                Ok(())
            } else {
                Err(CliError::Usage(format!(
                    "option {what} is only valid for 'report'"
                )))
            }
        };
        let report_or_find = |what: &str| -> Result<(), CliError> {
            if kind != Kind::Scan {
                Ok(())
            } else {
                Err(CliError::Usage(format!(
                    "option {what} is only valid for 'report' and 'find'"
                )))
            }
        };
        let only_overview = |what: &str| -> Result<(), CliError> {
            if kind == Kind::Overview {
                Ok(())
            } else {
                Err(CliError::Usage(format!(
                    "option {what} is only valid for 'overview'"
                )))
            }
        };
        let only_find = |what: &str| -> Result<(), CliError> {
            if kind == Kind::Find {
                Ok(())
            } else {
                Err(CliError::Usage(format!(
                    "option {what} is only valid for 'find'"
                )))
            }
        };
        let only_scan = |what: &str| -> Result<(), CliError> {
            if kind == Kind::Scan {
                Ok(())
            } else {
                Err(CliError::Usage(format!(
                    "option {what} is only valid for 'scan'"
                )))
            }
        };
        match name.as_str() {
            "--out" | "-o" => {
                let v = value(&name, inline, &mut i)?;
                common.out_dash = v == "-";
                common.out = if v == "-" {
                    None
                } else {
                    Some(PathBuf::from(v))
                };
            }
            "--engine" => {
                let v = value(&name, inline, &mut i)?;
                common.engine = match v.to_string_lossy().as_ref() {
                    "auto" => Engine::Auto,
                    "mft" => Engine::Mft,
                    "walk" => Engine::Walk,
                    other => {
                        return Err(CliError::Usage(format!(
                            "invalid --engine '{other}' (expected auto, mft or walk)"
                        )));
                    }
                };
                common.scan_only_given.push(name.clone());
            }
            "--threads" => {
                let v = value(&name, inline, &mut i)?;
                common.threads = Some(int_arg("--threads", &v, 1, 4096)? as usize);
                common.scan_only_given.push(name.clone());
            }
            "--no-index" => {
                if kind != Kind::Scan && kind != Kind::Overview {
                    return Err(CliError::Usage(
                        "option --no-index is only valid for 'scan' and 'overview'".into(),
                    ));
                }
                common.no_index = true;
            }
            "--quiet" | "-q" => common.quiet = true,
            "--log" => {
                let v = value(&name, inline, &mut i)?;
                common.log_json = match v.to_string_lossy().as_ref() {
                    "text" => false,
                    "json" => true,
                    other => {
                        return Err(CliError::Usage(format!(
                            "invalid --log '{other}' (expected text or json)"
                        )));
                    }
                };
            }
            // 輸出本來就一律是 JSON；接受這個選項，是因為 agent 很自然會順手加上
            "--json" => {}
            "--exclude" => {
                let v = value(&name, inline, &mut i)?;
                if v.is_empty() {
                    return Err(CliError::Usage(
                        "--exclude expects a non-empty pattern".into(),
                    ));
                }
                common.exclude.push(v);
                common.scan_only_given.push(name.clone());
            }
            "--follow-links" => {
                let v = value(&name, inline, &mut i)?;
                for part in v.to_string_lossy().split(',') {
                    let k = match part {
                        "mount" => FollowKind::Mount,
                        "symlink" => FollowKind::Symlink,
                        "junction" => FollowKind::Junction,
                        // 預設就是什麼連結都不跟隨；明說 none 的人不該被擋下來
                        "none" => continue,
                        other => {
                            return Err(CliError::Usage(format!(
                                "invalid --follow-links '{other}' (expected none, mount, symlink or junction)"
                            )));
                        }
                    };
                    if !common.follow.contains(&k) {
                        common.follow.push(k);
                    }
                }
                common.scan_only_given.push(name.clone());
            }
            "--hardlinks" => {
                let v = value(&name, inline, &mut i)?;
                common.hardlinks_dedupe = match v.to_string_lossy().as_ref() {
                    "dedupe" => true,
                    "none" => false,
                    other => {
                        return Err(CliError::Usage(format!(
                            "invalid --hardlinks '{other}' (expected dedupe or none)"
                        )));
                    }
                };
                common.scan_only_given.push(name.clone());
            }
            "--timeout-sec" => {
                let v = value(&name, inline, &mut i)?;
                common.timeout_sec = Some(int_arg("--timeout-sec", &v, 1, 31_536_000)?);
                common.scan_only_given.push(name.clone());
            }
            "--min-size" => {
                let v = value(&name, inline, &mut i)?;
                min_size = Some(parse_size(&v.to_string_lossy()).map_err(CliError::Usage)?);
            }
            "--max-depth" => {
                if kind == Kind::Find {
                    return Err(CliError::Usage(
                        "--max-depth does not apply to 'find' (use --path to limit the search)"
                            .into(),
                    ));
                }
                only_scan("--max-depth")?;
                let v = value(&name, inline, &mut i)?;
                max_depth = Some(int_arg("--max-depth", &v, 0, 100_000)?);
            }
            "--snapshot" => {
                report_or_find("--snapshot")?;
                snapshot = Some(PathBuf::from(value(&name, inline, &mut i)?));
            }
            "--top-files" | "--top-dirs" | "--by-ext" | "--many-files" => {
                only_report(&name)?;
                let v = value(&name, inline, &mut i)?;
                let n = int_arg(&name, &v, 1, 1000)? as usize;
                match name.as_str() {
                    "--top-files" => analyses.top_files = Some(n),
                    "--top-dirs" => analyses.top_dirs = Some(n),
                    "--by-ext" => analyses.by_ext = Some(n),
                    _ => analyses.many_files = Some(n),
                }
            }
            "--top" => {
                only_overview("--top")?;
                let v = value(&name, inline, &mut i)?;
                top = int_arg("--top", &v, 1, 100)? as usize;
                top_given = true;
            }
            "--ext-top" => {
                only_overview("--ext-top")?;
                let v = value(&name, inline, &mut i)?;
                ext_top = int_arg("--ext-top", &v, 1, 100)? as usize;
                top_given = true;
            }
            "--stale-days" => {
                only_overview("--stale-days")?;
                let v = value(&name, inline, &mut i)?;
                stale_days = int_arg("--stale-days", &v, 0, 3_650_000)?;
            }
            "--stale-min-size" => {
                only_overview("--stale-min-size")?;
                let v = value(&name, inline, &mut i)?;
                stale_min_size = parse_size(&v.to_string_lossy()).map_err(CliError::Usage)?;
            }
            "--rank" => {
                only_overview("--rank")?;
                let v = value(&name, inline, &mut i)?;
                rank_logical = match v.to_string_lossy().as_ref() {
                    "physical" => false,
                    "logical" => true,
                    other => {
                        return Err(CliError::Usage(format!(
                            "invalid --rank '{other}' (expected physical or logical)"
                        )));
                    }
                };
            }
            "--dir-rank" => {
                only_overview("--dir-rank")?;
                let v = value(&name, inline, &mut i)?;
                dir_direct = match v.to_string_lossy().as_ref() {
                    "rollup" => false,
                    "direct" => true,
                    other => {
                        return Err(CliError::Usage(format!(
                            "invalid --dir-rank '{other}' (expected rollup or direct)"
                        )));
                    }
                };
            }
            "--detail" => {
                only_overview("--detail")?;
                let v = value(&name, inline, &mut i)?;
                full = match v.to_string_lossy().as_ref() {
                    "concise" => false,
                    "full" => true,
                    other => {
                        return Err(CliError::Usage(format!(
                            "invalid --detail '{other}' (expected concise or full)"
                        )));
                    }
                };
            }
            "--save-index" => {
                only_overview("--save-index")?;
                let v = value(&name, inline, &mut i)?;
                if v == "-" {
                    return Err(CliError::Usage(
                        "--save-index needs a file name, not -".into(),
                    ));
                }
                save_index = Some(PathBuf::from(v));
            }
            "--index" => {
                if kind != Kind::Overview && kind != Kind::Report {
                    return Err(CliError::Usage(
                        "option --index is only valid for 'overview' and 'report'".into(),
                    ));
                }
                index_in = Some(PathBuf::from(value(&name, inline, &mut i)?));
            }
            "--save-snapshot" => {
                only_overview("--save-snapshot")?;
                let v = value(&name, inline, &mut i)?;
                if v == "-" {
                    return Err(CliError::Usage(
                        "--save-snapshot needs a file name, not - (a snapshot is never written to standard output here)".into(),
                    ));
                }
                save_snapshot = Some(PathBuf::from(v));
            }
            "--ext" => {
                report_or_find("--ext")?;
                let v = value(&name, inline, &mut i)?;
                let mut list = Vec::new();
                for part in v.to_string_lossy().split(',') {
                    if !part.starts_with('.') || part.len() < 2 {
                        return Err(CliError::Usage(format!(
                            "--ext entries must start with a dot, got '{part}'"
                        )));
                    }
                    list.push(part.to_lowercase());
                }
                ext = Some(list);
            }
            "--older-than" => {
                report_or_find("--older-than")?;
                let v = value(&name, inline, &mut i)?;
                older_than = Some(int_arg("--older-than", &v, 0, 3_650_000)?);
            }
            "--dir-depth" => {
                only_report("--dir-depth")?;
                let v = value(&name, inline, &mut i)?;
                dir_depth = Some(int_arg("--dir-depth", &v, 0, 100_000)?);
            }
            "--name" | "--name-regex" | "--path" => {
                only_find(&name)?;
                let v = value(&name, inline, &mut i)?.to_string_lossy().into_owned();
                if v.is_empty() {
                    return Err(CliError::Usage(format!(
                        "{name} expects a non-empty pattern"
                    )));
                }
                match name.as_str() {
                    "--name" => find.name = Some(v),
                    "--name-regex" => find.name_regex = Some(v),
                    _ => find.path = Some(v),
                }
            }
            "--type" => {
                only_find("--type")?;
                let v = value(&name, inline, &mut i)?;
                find.kind = match v.to_string_lossy().as_ref() {
                    "file" => FindKind::File,
                    "dir" => FindKind::Dir,
                    "any" => FindKind::Any,
                    other => {
                        return Err(CliError::Usage(format!(
                            "invalid --type '{other}' (expected file, dir or any)"
                        )));
                    }
                };
            }
            "--max-size" => {
                only_find("--max-size")?;
                let v = value(&name, inline, &mut i)?;
                find.max_size = Some(parse_size(&v.to_string_lossy()).map_err(CliError::Usage)?);
            }
            "--newer-than" => {
                only_find("--newer-than")?;
                let v = value(&name, inline, &mut i)?;
                find.newer_than = Some(int_arg("--newer-than", &v, 0, 3_650_000)?);
            }
            "--empty" => {
                only_find("--empty")?;
                find.empty = true;
            }
            "--limit" => {
                only_find("--limit")?;
                let v = value(&name, inline, &mut i)?;
                find.limit = int_arg("--limit", &v, 0, 10_000)? as usize;
            }
            "--sort" => {
                only_find("--sort")?;
                let v = value(&name, inline, &mut i)?;
                find.sort = match v.to_string_lossy().as_ref() {
                    "path" => FindSort::Path,
                    "size" => FindSort::Size,
                    "mtime" => FindSort::Mtime,
                    other => {
                        return Err(CliError::Usage(format!(
                            "invalid --sort '{other}' (expected size, mtime or path)"
                        )));
                    }
                };
            }
            _ => return Err(CliError::Usage(format!("unknown option '{raw}'"))),
        }
        i += 1;
    }

    match kind {
        Kind::Scan => {
            if roots.is_empty() {
                return Err(CliError::Usage("scan requires at least one PATH".into()));
            }
            Ok(Command::Scan(ScanArgs {
                roots,
                common,
                min_size,
                max_depth,
            }))
        }
        Kind::Report => {
            if analyses == Analyses::default() {
                return Err(CliError::Usage(
                    "report requires at least one of --top-files, --top-dirs, --by-ext, --many-files"
                        .into(),
                ));
            }
            if snapshot.is_some() && !roots.is_empty() {
                return Err(CliError::Usage(
                    "give either PATH arguments or --snapshot, not both".into(),
                ));
            }
            if index_in.is_some() && (!roots.is_empty() || snapshot.is_some()) {
                return Err(CliError::Usage(
                    "--index replaces PATH and --snapshot: give only one input".into(),
                ));
            }
            if index_in.is_some() && !common.scan_only_given.is_empty() {
                return Err(CliError::Usage(format!(
                    "scan options do not apply when reading an index: {}",
                    common.scan_only_given.join(", ")
                )));
            }
            if snapshot.is_none() && roots.is_empty() && index_in.is_none() {
                return Err(CliError::Usage(
                    "report requires a PATH, --snapshot <FILE> or --index <FILE>".into(),
                ));
            }
            if snapshot.is_some() && !common.scan_only_given.is_empty() {
                return Err(CliError::Usage(format!(
                    "scan options do not apply when reading a snapshot: {}",
                    common.scan_only_given.join(", ")
                )));
            }
            if (min_size.is_some() || ext.is_some() || older_than.is_some())
                && analyses.top_files.is_none()
            {
                return Err(CliError::Usage(
                    "--min-size, --ext and --older-than only apply to --top-files".into(),
                ));
            }
            if dir_depth.is_some() && analyses.top_dirs.is_none() {
                return Err(CliError::Usage(
                    "--dir-depth only applies to --top-dirs".into(),
                ));
            }
            Ok(Command::Report(ReportArgs {
                roots,
                snapshot,
                index: index_in,
                common,
                analyses,
                min_size,
                ext,
                older_than,
                dir_depth,
            }))
        }
        Kind::Overview => {
            if index_in.is_some() {
                if !roots.is_empty() || snapshot.is_some() {
                    return Err(CliError::Usage(
                        "--index replaces PATH and --snapshot: give only one input".into(),
                    ));
                }
                if save_snapshot.is_some() || save_index.is_some() {
                    return Err(CliError::Usage(
                        "--index only reads an index; it cannot be combined with --save-snapshot or --save-index".into(),
                    ));
                }
                if !common.scan_only_given.is_empty() {
                    return Err(CliError::Usage(format!(
                        "scan options do not apply when reading an index: {}",
                        common.scan_only_given.join(", ")
                    )));
                }
            } else if save_index.is_some()
                && (save_snapshot.is_some() || common.no_index || snapshot.is_some())
            {
                return Err(CliError::Usage(
                    "--save-index cannot be combined with --save-snapshot, --no-index or --snapshot (it scans and writes only the index)".into(),
                ));
            }
            if snapshot.is_some() && !roots.is_empty() {
                return Err(CliError::Usage(
                    "give either PATH arguments or --snapshot, not both".into(),
                ));
            }
            if snapshot.is_none() && roots.is_empty() && index_in.is_none() {
                return Err(CliError::Usage(
                    "overview requires a PATH, --snapshot <FILE> or --index <FILE>".into(),
                ));
            }
            if snapshot.is_some() && save_snapshot.is_some() {
                return Err(CliError::Usage(
                    "--save-snapshot cannot be combined with --snapshot (the input is already a snapshot)".into(),
                ));
            }
            if snapshot.is_some() && !common.scan_only_given.is_empty() {
                return Err(CliError::Usage(format!(
                    "scan options do not apply when reading a snapshot: {}",
                    common.scan_only_given.join(", ")
                )));
            }
            if analyses != Analyses::default()
                || dir_depth.is_some()
                || min_size.is_some()
                || ext.is_some()
                || older_than.is_some()
            {
                return Err(CliError::Usage(
                    "overview takes none of --top-files, --top-dirs, --by-ext, --many-files, --dir-depth, --min-size, --ext, --older-than (use --top, --ext-top, --stale-days, --stale-min-size)"
                        .into(),
                ));
            }
            Ok(Command::Overview(OverviewArgs {
                roots,
                snapshot,
                common,
                top,
                ext_top,
                stale_days,
                stale_min_size,
                rank_logical,
                dir_direct,
                top_given,
                full,
                save_snapshot,
                save_index,
                index: index_in,
            }))
        }
        Kind::Find => {
            if analyses != Analyses::default() || dir_depth.is_some() {
                return Err(CliError::Usage(
                    "--top-files, --top-dirs, --by-ext, --many-files and --dir-depth are only valid for 'report'"
                        .into(),
                ));
            }
            if find.name.is_some() && find.name_regex.is_some() {
                return Err(CliError::Usage(
                    "give either --name or --name-regex, not both".into(),
                ));
            }
            if snapshot.is_some() && !roots.is_empty() {
                return Err(CliError::Usage(
                    "give either PATH arguments or --snapshot, not both".into(),
                ));
            }
            if snapshot.is_none() && roots.is_empty() {
                return Err(CliError::Usage(
                    "find requires a PATH or --snapshot <FILE>".into(),
                ));
            }
            if snapshot.is_some() && !common.scan_only_given.is_empty() {
                return Err(CliError::Usage(format!(
                    "scan options do not apply when reading a snapshot: {}",
                    common.scan_only_given.join(", ")
                )));
            }
            if let (Some(lo), Some(hi)) = (min_size, find.max_size)
                && lo > hi
            {
                return Err(CliError::Usage(
                    "--min-size is larger than --max-size".into(),
                ));
            }
            Ok(Command::Find(FindArgs {
                roots,
                snapshot,
                common,
                name: find.name,
                name_regex: find.name_regex,
                path: find.path,
                kind: find.kind,
                ext,
                min_size,
                max_size: find.max_size,
                older_than,
                newer_than: find.newer_than,
                empty: find.empty,
                limit: find.limit,
                sort: find.sort,
            }))
        }
    }
}

fn parse_diff(args: Vec<OsString>) -> Result<Command, CliError> {
    let mut paths: Vec<PathBuf> = Vec::new();
    let (mut top, mut min_change, mut under) = (50usize, 0u64, None);
    let (mut rank_rollup, mut out, mut quiet) = (false, None, false);
    let mut log_json = false;
    let (mut a_root, mut b_root, mut match_roots) = (None, None, false);
    let mut i = 0;
    while i < args.len() {
        let raw = args[i].to_string_lossy().into_owned();
        if !raw.starts_with('-') || raw == "-" {
            paths.push(PathBuf::from(&args[i]));
            i += 1;
            continue;
        }
        let (name, inline) = match raw.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_string(), Some(v.to_string())),
            _ => (raw.clone(), None),
        };
        let value = |i: &mut usize| -> Result<OsString, CliError> {
            if let Some(v) = &inline {
                return Ok(OsString::from(v));
            }
            *i += 1;
            args.get(*i)
                .cloned()
                .ok_or_else(|| CliError::Usage(format!("option {name} requires a value")))
        };
        match name.as_str() {
            "--top" => top = int_arg("--top", &value(&mut i)?, 1, 1000)? as usize,
            "--min-change" => {
                min_change =
                    parse_size(&value(&mut i)?.to_string_lossy()).map_err(CliError::Usage)?;
            }
            "--under" => {
                let v = value(&mut i)?.to_string_lossy().into_owned();
                if v.is_empty() {
                    return Err(CliError::Usage(
                        "--under expects a non-empty pattern".into(),
                    ));
                }
                under = Some(v);
            }
            "--rank" => {
                rank_rollup = match value(&mut i)?.to_string_lossy().as_ref() {
                    "direct" => false,
                    "rollup" => true,
                    other => {
                        return Err(CliError::Usage(format!(
                            "invalid --rank '{other}' (expected direct or rollup)"
                        )));
                    }
                };
            }
            "--a-root" => a_root = Some(PathBuf::from(value(&mut i)?)),
            "--b-root" => b_root = Some(PathBuf::from(value(&mut i)?)),
            "--match-roots" => match_roots = true,
            "--json" => {}
            "--out" | "-o" => {
                let v = value(&mut i)?;
                out = if v == "-" {
                    None
                } else {
                    Some(PathBuf::from(v))
                };
            }
            "--quiet" | "-q" => quiet = true,
            "--log" => {
                log_json = match value(&mut i)?.to_string_lossy().as_ref() {
                    "text" => false,
                    "json" => true,
                    other => {
                        return Err(CliError::Usage(format!(
                            "invalid --log '{other}' (expected text or json)"
                        )));
                    }
                };
            }
            _ => return Err(CliError::Usage(format!("unknown option '{raw}'"))),
        }
        i += 1;
    }
    if paths.len() != 2 {
        return Err(CliError::Usage(
            "diff requires exactly two snapshot files: diff <A> <B>".into(),
        ));
    }
    let b = paths.pop().expect("two paths");
    let a = paths.pop().expect("two paths");
    Ok(Command::Diff(DiffArgs {
        a,
        b,
        top,
        min_change,
        under,
        rank_rollup,
        a_root,
        b_root,
        match_roots,
        out,
        quiet,
        log_json,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Command, CliError> {
        parse(args.iter().map(OsString::from).collect())
    }

    fn scan(args: &[&str]) -> ScanArgs {
        match p(args) {
            Ok(Command::Scan(a)) => a,
            other => panic!("expected scan, got {other:?}"),
        }
    }

    fn report(args: &[&str]) -> ReportArgs {
        match p(args) {
            Ok(Command::Report(a)) => a,
            other => panic!("expected report, got {other:?}"),
        }
    }

    #[test]
    fn size_units() {
        assert_eq!(parse_size("0"), Ok(0));
        assert_eq!(parse_size("123"), Ok(123));
        assert_eq!(parse_size("1K"), Ok(1_000));
        assert_eq!(parse_size("2m"), Ok(2_000_000));
        assert_eq!(parse_size("3G"), Ok(3_000_000_000));
        assert_eq!(parse_size("1T"), Ok(1_000_000_000_000));
        assert_eq!(parse_size("1KiB"), Ok(1024));
        assert_eq!(parse_size("1mib"), Ok(1 << 20));
        assert_eq!(parse_size("5GiB"), Ok(5 << 30));
        assert_eq!(parse_size("2TiB"), Ok(2 << 40));
    }

    #[test]
    fn size_errors() {
        assert!(parse_size("").is_err());
        assert!(parse_size("abc").is_err());
        assert!(parse_size("10X").is_err());
        assert!(parse_size("-5").is_err());
        assert!(parse_size("1.5M").is_err());
        assert!(parse_size("99999999999999999999").is_err());
        assert!(parse_size("18446744073709551615T").is_err());
    }

    #[test]
    fn basic_commands() {
        assert_eq!(p(&["--version"]), Ok(Command::Version));
        assert_eq!(p(&["info"]), Ok(Command::Info));
        assert_eq!(p(&["info", "--json"]), Ok(Command::Info));
        assert!(matches!(p(&[]), Err(CliError::Usage(_))));
        assert!(matches!(p(&["frobnicate"]), Err(CliError::Usage(_))));
        assert!(matches!(
            p(&["dupes", "C:\\"]),
            Err(CliError::Unsupported(_))
        ));
    }

    #[test]
    fn scan_options() {
        let a = scan(&[
            "scan",
            "C:\\a",
            "D:\\b",
            "--out",
            "x.ndjson",
            "--threads=4",
            "-q",
            "--engine",
            "walk",
            "--min-size",
            "1MiB",
            "--max-depth=3",
            "--exclude",
            "*.tmp",
            "--exclude",
            "*\\cache",
            "--follow-links",
            "junction,mount",
            "--follow-links",
            "symlink",
            "--hardlinks",
            "none",
            "--timeout-sec",
            "30",
        ]);
        assert_eq!(a.roots.len(), 2);
        assert_eq!(a.common.out, Some(PathBuf::from("x.ndjson")));
        assert_eq!(a.common.threads, Some(4));
        assert!(a.common.quiet);
        assert_eq!(a.common.engine, Engine::Walk);
        assert_eq!(a.min_size, Some(1 << 20));
        assert_eq!(a.max_depth, Some(3));
        assert_eq!(a.common.exclude.len(), 2);
        assert_eq!(
            a.common.follow,
            [FollowKind::Junction, FollowKind::Mount, FollowKind::Symlink]
        );
        assert!(!a.common.hardlinks_dedupe);
        assert_eq!(a.common.timeout_sec, Some(30));
        assert_eq!(scan(&["scan", "C:\\a", "--out", "-"]).common.out, None);
    }

    #[test]
    fn invalid_scan_values_are_usage_errors() {
        for bad in [
            &["scan"][..],
            &["scan", "C:\\", "--threads", "0"],
            &["scan", "C:\\", "--engine", "gpu"],
            &["scan", "C:\\", "--bogus"],
            &["scan", "C:\\", "--out"],
            &["scan", "C:\\", "--min-size", "zz"],
            &["scan", "C:\\", "--max-depth", "-1"],
            &["scan", "C:\\", "--follow-links", "hardlink"],
            &["scan", "C:\\", "--hardlinks", "maybe"],
            &["scan", "C:\\", "--timeout-sec", "0"],
            &["scan", "C:\\", "--top-files", "5"],
        ] {
            assert!(matches!(p(bad), Err(CliError::Usage(_))), "{bad:?}");
        }
    }

    #[test]
    fn json_and_follow_links_none_are_accepted() {
        // agent 很自然會寫這兩個：--json（輸出本來就一律是 JSON）與 --follow-links none（那就是預設）
        for cmd in [
            &["scan", "C:\\a", "--out", "-", "--json"][..],
            &["report", "C:\\a", "--top-files", "3", "--json"],
            &["find", "C:\\a", "--name", "x", "--json"],
            &["overview", "C:\\a", "--json"],
            &["diff", "a.ndjson", "b.ndjson", "--json"],
        ] {
            assert!(p(cmd).is_ok(), "{cmd:?}: {:?}", p(cmd));
        }
        let none = scan(&["scan", "C:\\a", "--out", "-", "--follow-links", "none"]);
        assert!(none.common.follow.is_empty());
        let mixed = scan(&[
            "scan",
            "C:\\a",
            "--out",
            "-",
            "--follow-links",
            "none,junction",
        ]);
        assert_eq!(mixed.common.follow, [FollowKind::Junction]);
    }

    #[test]
    fn report_requires_an_analysis_and_a_source() {
        assert!(matches!(p(&["report", "C:\\"]), Err(CliError::Usage(_))));
        assert!(matches!(
            p(&["report", "--top-files", "5"]),
            Err(CliError::Usage(_))
        ));
        assert!(matches!(
            p(&[
                "report",
                "C:\\",
                "--snapshot",
                "s.ndjson",
                "--top-files",
                "5"
            ]),
            Err(CliError::Usage(_))
        ));
        let r = report(&[
            "report",
            "C:\\",
            "--top-files",
            "10",
            "--top-dirs=20",
            "--by-ext",
            "5",
            "--many-files",
            "7",
        ]);
        assert_eq!(r.analyses.top_files, Some(10));
        assert_eq!(r.analyses.top_dirs, Some(20));
        assert_eq!(r.analyses.by_ext, Some(5));
        assert_eq!(r.analyses.many_files, Some(7));
        assert_eq!(r.snapshot, None);
    }

    #[test]
    fn report_n_must_be_between_1_and_1000() {
        assert!(matches!(
            p(&["report", "C:\\", "--top-files", "0"]),
            Err(CliError::Usage(_))
        ));
        assert!(matches!(
            p(&["report", "C:\\", "--top-files", "1001"]),
            Err(CliError::Usage(_))
        ));
        assert!(p(&["report", "C:\\", "--top-files", "1000"]).is_ok());
        assert!(p(&["report", "C:\\", "--top-files", "1"]).is_ok());
    }

    #[test]
    fn report_filters() {
        let r = report(&[
            "report",
            "--snapshot",
            "s.ndjson",
            "--top-files",
            "5",
            "--min-size",
            "10M",
            "--ext",
            ".ISO,.Zip",
            "--older-than",
            "90",
            "--top-dirs",
            "3",
            "--dir-depth",
            "2",
        ]);
        assert_eq!(r.snapshot, Some(PathBuf::from("s.ndjson")));
        assert_eq!(r.min_size, Some(10_000_000));
        assert_eq!(r.ext, Some(vec![".iso".to_string(), ".zip".to_string()]));
        assert_eq!(r.older_than, Some(90));
        assert_eq!(r.dir_depth, Some(2));
        // 篩選只適用於對應的分析
        assert!(matches!(
            p(&["report", "C:\\", "--top-dirs", "5", "--ext", ".a"]),
            Err(CliError::Usage(_))
        ));
        assert!(matches!(
            p(&["report", "C:\\", "--top-files", "5", "--dir-depth", "2"]),
            Err(CliError::Usage(_))
        ));
        assert!(matches!(
            p(&["report", "C:\\", "--top-files", "5", "--ext", "iso"]),
            Err(CliError::Usage(_))
        ));
    }

    #[test]
    fn scan_options_are_rejected_with_a_snapshot() {
        for opt in [
            &["--engine", "walk"][..],
            &["--threads", "2"],
            &["--exclude", "*"],
            &["--follow-links", "junction"],
            &["--hardlinks", "none"],
            &["--timeout-sec", "5"],
        ] {
            let mut args = vec!["report", "--snapshot", "s.ndjson", "--top-files", "5"];
            args.extend_from_slice(opt);
            assert!(matches!(p(&args), Err(CliError::Usage(_))), "{opt:?}");
        }
        // --out 與 -q 在使用快照時仍可用
        assert!(
            p(&[
                "report",
                "--snapshot",
                "s.ndjson",
                "--top-files",
                "5",
                "--out",
                "r.json",
                "-q"
            ])
            .is_ok()
        );
    }

    #[test]
    fn max_depth_is_not_a_report_option() {
        assert!(matches!(
            p(&["report", "C:\\", "--top-files", "5", "--max-depth", "2"]),
            Err(CliError::Usage(_))
        ));
    }
}
