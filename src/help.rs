//! `--help` 與 `help <子命令>` 的內容：每個選項都寫出預設值與範圍。
//!
//! 這裡的範圍必須與 `cli.rs` 的檢查一致；`tests/m9.rs` 會逐個選項核對輸出裡有沒有它。

const SIZE: &str = "\
  SIZE  is a number of bytes, or a number with a unit: K M G T (powers of 1000)
        or KiB MiB GiB TiB (powers of 1024), for example 500M or 2GiB.";

pub const GENERAL: &str = "voltdir - read-only disk scanner for Windows; results are JSON on stdout

usage:
  voltdir --version
  voltdir help [COMMAND]            (same as COMMAND --help)
  voltdir info [--json]
  voltdir overview <PATH>... | --snapshot <FILE> [options]   (start here)
  voltdir scan <PATH>... [options]
  voltdir report <PATH>... | --snapshot <FILE>  (--top-files|--top-dirs|--by-ext|--many-files <N>)... [options]
  voltdir find <PATH>... | --snapshot <FILE>  [filters] [options]
  voltdir diff <A> <B>  [options]

Run 'voltdir help <command>' for every option with its default and allowed range.
Exit codes: 0 ok, 1 usage error, 2 finished but some directories were not readable,
3 engine unavailable, 4 output failed, 5 cancelled.
";

pub const INFO: &str = "usage: voltdir info [--json]

Prints one JSON object: version, schema versions, whether the process is elevated,
engine availability, the volumes (with free space), and the tool's limits.

options:
  --json   accepted for symmetry; the output is always JSON (default: on, no other mode)
  -h, --help
";

fn scan_common() -> &'static str {
    "\
  -o, --out <FILE|->          where to write the result. default: - (standard output)
                              the parent folder must already exist and be writable;
                              it is checked before scanning and never created
      --engine auto|mft|walk  default: auto (mft when elevated and the root is a whole NTFS volume)
      --threads <N>           integer 1 to 4096. default: number of logical CPUs
                              (report and overview use at most 3 unless you set it: more threads
                              do not make folder listing faster on the machines measured)
  -q, --quiet                 no progress or summary on stderr. default: off
      --log text|json         format of stderr messages and errors. default: text
      --exclude <GLOB>        skip matching paths; may be repeated. default: none
      --follow-links <KIND>   mount, symlink or junction (comma list, may be repeated). default: none
      --hardlinks dedupe|none default: dedupe
      --timeout-sec <N>       integer 1 to 31536000. default: no timeout"
}

pub fn text(name: &str) -> Option<String> {
    let text = match name {
        "info" => INFO.to_string(),
        "scan" => format!(
            "usage: voltdir scan <PATH>... [options]

Scans one or more folders or drives and writes a snapshot (NDJSON) to stdout or --out.

options:
{}
      --no-index              do not write the summary index <FILE>.idx next to the snapshot. default: write it
      --min-size <SIZE>       leave out files smaller than this from the listing. default: 0 (all)
      --max-depth <N>         integer 0 to 100000; list only this deep. default: unlimited
  -h, --help

{SIZE}
",
            scan_common()
        ),
        "overview" => format!(
            "usage: voltdir overview <PATH>... | --snapshot <FILE> [options]

One call answers the usual questions: totals, largest folders, largest files, extensions,
old large files, empty files and folders, unread places, and runnable next commands.
Scans once (or reads a snapshot). With one root and --detail concise, paths are relative to it.

options:
      --top <N>               folders and files per list. integer 1 to 100. default: 10
                              (when neither --top nor --ext-top is given and the output would exceed 4096 bytes,
                              lists are cut to 5, then 3 items and 'trimmed' says so)
      --ext-top <N>           extensions listed. integer 1 to 100. default: 8
      --stale-days <D>        old-file threshold in days. integer 0 to 3650000. default: 365
      --stale-min-size <SIZE> old-file size threshold. default: 100MiB
      --rank physical|logical sort folders, files and extensions by allocated or content size. default: physical
      --dir-rank rollup|direct rank top_dirs by everything below (rollup) or by the folder's own files (direct, skips folders with no files of their own). default: rollup
      --detail concise|full   full adds meta and uses absolute paths. default: concise
      --save-snapshot <FILE>  also save the scan as a snapshot for later --snapshot questions
                              (not with --snapshot; the parent folder must already exist);
                              a summary index <FILE>.idx is written next to it
      --no-index              with --save-snapshot: do not write the index. default: write it
      --save-index <FILE>     scan (streaming, small memory) and write only a summary index, no snapshot;
                              later questions use --index <FILE>. find and diff need --save-snapshot instead
      --index <FILE>          answer from an index written by --save-index (no scan); not with PATH or --snapshot
      --snapshot <FILE>       read a snapshot instead of scanning (scan options then do not apply)
{}
  -h, --help

{SIZE}
",
            scan_common()
        ),
        "report" => format!(
            "usage: voltdir report <PATH>... | --snapshot <FILE>  (--top-files|--top-dirs|--by-ext|--many-files <N>)... [options]

Answers a question in one small JSON object. At least one analysis is required.

analyses (each N is an integer 1 to 1000, no default):
      --top-files <N>         the N largest files
      --top-dirs <N>          the N largest directories
      --by-ext <N>            the N extensions that use the most space
      --many-files <N>        the N directories with the most files

filters:
      --min-size <SIZE>       only for --top-files. default: 0
      --ext <.a,.b>           only for --top-files; each entry starts with a dot. default: all
      --older-than <DAYS>     only for --top-files; integer 0 to 3650000. default: any age
      --dir-depth <D>         only for --top-dirs; integer 0 to 100000. default: any depth

source:
      --snapshot <FILE>       read a snapshot instead of scanning (scan options then do not apply)
      --index <FILE>          read an index written by 'overview --save-index' (top-files, top-dirs, by-ext,
                              many-files without filters only)

options:
{}
  -h, --help

{SIZE}
",
            scan_common()
        ),
        "find" => format!(
            "usage: voltdir find <PATH>... | --snapshot <FILE>  [filters] [options]

Lists the entries that match every filter given.

filters:
      --name <GLOB>           * and ? only, case-insensitive. default: any name
      --name-regex <RE>       regular expression on the name (not with --name). default: none
      --path <GLOB>           match the whole path. default: any path
      --type file|dir|any     default: file
      --ext <.a,.b>           each entry starts with a dot. default: any (excludes directories)
      --min-size <SIZE>       default: 0
      --max-size <SIZE>       default: unlimited
      --older-than <DAYS>     integer 0 to 3650000. default: any age
      --newer-than <DAYS>     integer 0 to 3650000. default: any age
      --empty                 only empty files or directories. default: off

result:
      --limit <N>             integer 0 to 10000. default: 100
      --sort size|mtime|path  default: path

source:
      --snapshot <FILE>       read a snapshot instead of scanning (scan options then do not apply)

options:
{}
  -h, --help

{SIZE}
",
            scan_common()
        ),
        "diff" => format!(
            "usage: voltdir diff <A> <B>  [options]

Compares two snapshot files (A is older, B is newer). Snapshots of the same path pair up directly.
For different paths (for example monday vs friday copies of one folder) use --match-roots.

options:
      --top <N>               integer 1 to 1000. default: 50
      --min-change <SIZE>     ignore changes smaller than this. default: 0
      --under <GLOB>          only paths matching this. default: everything
      --rank direct|rollup    rank directories by their own files or by everything below. default: direct
      --match-roots           treat the single root of A and the single root of B as the same folder; output paths become relative to it (rebased: true)
      --a-root <PATH>         with --b-root: the folder inside A to line up with --b-root inside B (use when a snapshot has several roots)
      --b-root <PATH>         see --a-root
  -o, --out <FILE|->          default: - (standard output); the parent folder must already exist
  -q, --quiet                 no warning on stderr. default: off
      --log text|json        format of stderr errors. default: text
  -h, --help

{SIZE}
"
        ),
        _ => return None,
    };
    // scan 沒有 --out 時不輸出到標準輸出，所以 --out 對 scan 是必填
    Some(if name == "scan" {
        text.replace(
            "where to write the result. default: - (standard output)",
            "REQUIRED: a file, or - for standard output. There is no default,
                              because a snapshot can be hundreds of MB",
        )
    } else {
        text
    })
}
