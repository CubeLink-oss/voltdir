//! 讀取並驗證 `voltdir.snapshot/1` 快照。
//!
//! `Reader` 一次讀一行並交出一筆 `Rec`（`find`、`diff` 用）；`read` 把每筆交給 `report` 的分析器。
//! 先完整驗證才輸出：任何錯誤（截斷、格式不符、版本不符）都回傳 `Err`，呼叫端不得輸出部分結果。

use crate::jsonin::{self, Val};
use crate::model::Totals;
use crate::report::{DirView, FileView, Sink};
use crate::snapshot::{Coverage, SCHEMA};
use crate::timefmt;
use std::io::BufRead;

#[derive(Debug, Default, Clone)]
pub struct Parsed {
    /// `meta` 物件去掉 `type`、`schema` 後的 JSON（報表直接帶入）
    pub meta_body: Vec<u8>,
    pub min_size: Option<u64>,
    pub max_depth: Option<u64>,
    pub totals: Totals,
    pub coverage: Coverage,
    /// `meta.roots`（沒有這個欄位時為空）
    pub roots: Vec<Vec<u16>>,
    pub started: String,
    pub engine: String,
}

/// 一個 `dir` 或 `file` 行的內容（欄位在 `Reader::next_into` 中就地覆寫，避免逐行配置）。
#[derive(Default)]
pub struct Rec {
    pub is_dir: bool,
    pub path: Vec<u16>,
    /// 名稱；快照沒有 `name` 時取路徑最後一段
    pub name: Vec<u16>,
    pub ext: Vec<u16>,
    pub depth: u64,
    pub logical: u64,
    pub physical: u64,
    /// 檔案為 `mtime`，目錄為 `latest_mtime`
    pub mtime: u64,
    pub secondary: bool,
    pub rollup: Totals,
    pub direct: Totals,
}

fn bad(line_no: usize, msg: impl std::fmt::Display) -> String {
    format!("line {line_no}: {msg}")
}

fn req<'v, 'a>(v: &'v Val<'a>, key: &str, line_no: usize) -> Result<&'v Val<'a>, String> {
    v.get(key)
        .ok_or_else(|| bad(line_no, format!("missing field '{key}'")))
}

fn req_u64(v: &Val<'_>, key: &str, line_no: usize) -> Result<u64, String> {
    req(v, key, line_no)?.as_u64().ok_or_else(|| {
        bad(
            line_no,
            format!("field '{key}' must be a non-negative integer"),
        )
    })
}

fn req_units<'v>(v: &'v Val<'_>, key: &str, line_no: usize) -> Result<&'v [u16], String> {
    req(v, key, line_no)?
        .as_units()
        .ok_or_else(|| bad(line_no, format!("field '{key}' must be a string")))
}

fn req_time(v: &Val<'_>, key: &str, line_no: usize) -> Result<u64, String> {
    let s = req(v, key, line_no)?
        .as_string()
        .ok_or_else(|| bad(line_no, format!("field '{key}' must be a string")))?;
    timefmt::parse_filetime(&s)
        .ok_or_else(|| bad(line_no, format!("field '{key}' is not a valid time: {s}")))
}

fn totals_of(v: &Val<'_>, with_dirs: bool, line_no: usize) -> Result<Totals, String> {
    Ok(Totals {
        files: req_u64(v, "files", line_no)?,
        dirs: if with_dirs {
            req_u64(v, "dirs", line_no)?
        } else {
            0
        },
        logical: req_u64(v, "logical", line_no)?,
        physical: req_u64(v, "physical", line_no)?,
    })
}

pub(crate) fn coverage_of(v: &Val<'_>, line_no: usize) -> Result<Coverage, String> {
    let mut c = Coverage {
        complete_dirs: req_u64(v, "complete_dirs", line_no)?,
        incomplete_dirs: req_u64(v, "incomplete_dirs", line_no)?,
        truncated: req(v, "truncated", line_no)?
            .as_bool()
            .ok_or_else(|| bad(line_no, "field 'truncated' must be a boolean"))?,
        ..Default::default()
    };
    if let Val::Obj(errs) = req(v, "errors", line_no)? {
        for (k, n) in errs {
            let code: u32 = k
                .parse()
                .map_err(|_| bad(line_no, "error code must be an integer"))?;
            c.errors.insert(
                code,
                n.as_u64()
                    .ok_or_else(|| bad(line_no, "error count must be an integer"))?,
            );
        }
    } else {
        return Err(bad(line_no, "field 'errors' must be an object"));
    }
    if let Val::Arr(paths) = req(v, "incomplete_paths", line_no)? {
        for p in paths {
            c.incomplete_paths.push(
                p.as_units()
                    .ok_or_else(|| bad(line_no, "incomplete_paths entries must be strings"))?
                    .to_vec(),
            );
        }
    } else {
        return Err(bad(line_no, "field 'incomplete_paths' must be an array"));
    }
    Ok(c)
}

/// 解析結尾的 `summary` 行，取得總計與涵蓋範圍（建立索引時用）。
pub fn parse_summary(line: &[u8]) -> Result<(Totals, Coverage), String> {
    let v = jsonin::parse(line).map_err(|e| format!("summary line: {e}"))?;
    if v.get("type").and_then(Val::as_string).as_deref() != Some("summary") {
        return Err("the last line of the snapshot is not the summary line".into());
    }
    Ok((
        totals_of(req(&v, "totals", 0)?, true, 0)?,
        coverage_of(req(&v, "coverage", 0)?, 0)?,
    ))
}

fn parse_meta(v: &Val<'_>, line_no: usize) -> Result<Parsed, String> {
    let schema = v.get("schema").and_then(Val::as_string).unwrap_or_default();
    if schema != SCHEMA {
        return Err(bad(
            line_no,
            format!("unsupported schema '{schema}' (expected {SCHEMA})"),
        ));
    }
    let opts = v.get("options");
    let opt_num = |k: &str| opts.and_then(|o| o.get(k)).and_then(Val::as_u64);
    let body = match v {
        Val::Obj(fields) => {
            let kept: Vec<(&str, Val<'_>)> = fields
                .iter()
                .filter(|(k, _)| *k != "type" && *k != "schema")
                .map(|(k, x)| (*k, x.clone()))
                .collect();
            let mut out = Vec::new();
            jsonin::write(&mut out, &Val::Obj(kept));
            out
        }
        _ => unreachable!("type was read from an object"),
    };
    let roots = match v.get("roots") {
        Some(Val::Arr(a)) => a
            .iter()
            .filter_map(|r| r.as_units().map(<[u16]>::to_vec))
            .collect(),
        _ => Vec::new(),
    };
    Ok(Parsed {
        meta_body: body,
        min_size: opt_num("min_size"),
        max_depth: opt_num("max_depth"),
        roots,
        started: v
            .get("started")
            .and_then(Val::as_string)
            .unwrap_or_default(),
        engine: v.get("engine").and_then(Val::as_string).unwrap_or_default(),
        ..Default::default()
    })
}

/// 一行一行讀取快照。`open` 讀到 `meta` 行為止；之後反覆呼叫 `next_into`，回傳 `false` 時
/// 整份快照（含結尾的 `summary`）都已驗證，這時才可以呼叫 `finish`。
pub struct Reader<'r, R: BufRead> {
    input: &'r mut R,
    line: Vec<u8>,
    line_no: usize,
    /// 已讀過的位元組數，以及最近一個非空行的起點（名稱索引記錄原始行的位置用）
    pos: Pos,
    meta: Parsed,
    summary: Option<(Totals, Coverage)>,
}

#[derive(Default, Clone, Copy)]
struct Pos {
    read: u64,
    start: u64,
}

/// 讀下一個非空行（去掉換行）。回傳 `false` 表示檔案結束。
fn next_line(
    input: &mut impl BufRead,
    line: &mut Vec<u8>,
    line_no: &mut usize,
    pos: &mut Pos,
) -> Result<bool, String> {
    loop {
        line.clear();
        let n = input
            .read_until(b'\n', line)
            .map_err(|e| format!("read error: {e}"))?;
        if n == 0 {
            return Ok(false);
        }
        pos.start = pos.read;
        pos.read += n as u64;
        *line_no += 1;
        while matches!(line.last(), Some(b'\n' | b'\r')) {
            line.pop();
        }
        if !line.is_empty() {
            return Ok(true);
        }
    }
}

impl<'r, R: BufRead> Reader<'r, R> {
    pub fn open(input: &'r mut R) -> Result<Self, String> {
        let mut line = Vec::with_capacity(4096);
        let mut line_no = 0usize;
        let mut pos = Pos::default();
        if !next_line(input, &mut line, &mut line_no, &mut pos)? {
            return Err("snapshot is empty: no meta line".into());
        }
        let v = jsonin::parse(&line).map_err(|e| bad(line_no, e))?;
        let ty = v
            .get("type")
            .and_then(Val::as_string)
            .ok_or_else(|| bad(line_no, "missing 'type'"))?;
        if ty != "meta" {
            return Err(bad(line_no, "first line must be the meta line"));
        }
        let meta = parse_meta(&v, line_no)?;
        drop(v);
        Ok(Reader {
            input,
            line,
            line_no,
            pos,
            meta,
            summary: None,
        })
    }

    /// 最近一次讀到的那一行在檔案中的（起點位元組，長度）；長度含換行。
    pub fn line_span(&self) -> (u64, u32) {
        (self.pos.start, (self.pos.read - self.pos.start) as u32)
    }

    pub fn meta(&self) -> &Parsed {
        &self.meta
    }

    /// 最近一次 `next_into` 讀到的那一行（原始 JSON，不含換行）。
    pub fn raw(&self) -> &[u8] {
        &self.line
    }

    /// 讀到下一個 `dir` 或 `file` 行並填入 `rec`；沒有更多資料（且已驗證到結尾）時回傳 `false`。
    pub fn next_into(&mut self, rec: &mut Rec) -> Result<bool, String> {
        loop {
            if !next_line(self.input, &mut self.line, &mut self.line_no, &mut self.pos)? {
                return if self.summary.is_some() {
                    Ok(false)
                } else {
                    Err("snapshot is truncated: the summary line is missing".into())
                };
            }
            let line_no = self.line_no;
            if self.summary.is_some() {
                return Err(bad(line_no, "data after the summary line"));
            }
            let v = jsonin::parse(&self.line).map_err(|e| bad(line_no, e))?;
            let ty = v
                .get("type")
                .and_then(Val::as_string)
                .ok_or_else(|| bad(line_no, "missing 'type'"))?;
            match ty.as_str() {
                "meta" => return Err(bad(line_no, "more than one meta line")),
                "dir" => {
                    rec.is_dir = true;
                    rec.rollup = totals_of(req(&v, "rollup", line_no)?, true, line_no)?;
                    rec.direct = totals_of(req(&v, "direct", line_no)?, false, line_no)?;
                    fill_path(rec, &v, line_no)?;
                    rec.depth = req_u64(&v, "depth", line_no)?;
                    rec.mtime = req_time(&v, "latest_mtime", line_no)?;
                    rec.ext.clear();
                    rec.logical = rec.rollup.logical;
                    rec.physical = rec.rollup.physical;
                    rec.secondary = false;
                    return Ok(true);
                }
                "file" => {
                    rec.is_dir = false;
                    fill_path(rec, &v, line_no)?;
                    let ext = req_units(&v, "ext", line_no)?;
                    rec.ext.clear();
                    rec.ext.extend_from_slice(ext);
                    rec.depth = v.get("depth").and_then(Val::as_u64).unwrap_or(0);
                    rec.logical = req_u64(&v, "size_logical", line_no)?;
                    rec.physical = req_u64(&v, "size_physical", line_no)?;
                    rec.mtime = req_time(&v, "mtime", line_no)?;
                    rec.secondary = req(&v, "hardlink_secondary", line_no)?
                        .as_bool()
                        .ok_or_else(|| {
                            bad(line_no, "field 'hardlink_secondary' must be a boolean")
                        })?;
                    rec.rollup = Totals::default();
                    rec.direct = Totals::default();
                    return Ok(true);
                }
                "summary" => {
                    let totals = totals_of(req(&v, "totals", line_no)?, true, line_no)?;
                    let coverage = coverage_of(req(&v, "coverage", line_no)?, line_no)?;
                    self.summary = Some((totals, coverage));
                }
                other => return Err(bad(line_no, format!("unknown line type '{other}'"))),
            }
        }
    }

    /// 在 `next_into` 回傳 `false` 之後取回完整的中繼資料、總計與涵蓋範圍。
    pub fn finish(mut self) -> Result<Parsed, String> {
        let (totals, coverage) = self
            .summary
            .take()
            .ok_or("snapshot is truncated: the summary line is missing")?;
        self.meta.totals = totals;
        self.meta.coverage = coverage;
        Ok(self.meta)
    }
}

fn fill_path(rec: &mut Rec, v: &Val<'_>, line_no: usize) -> Result<(), String> {
    let path = req_units(v, "path", line_no)?;
    rec.path.clear();
    rec.path.extend_from_slice(path);
    rec.name.clear();
    match v.get("name").and_then(Val::as_units) {
        Some(n) => rec.name.extend_from_slice(n),
        None => {
            let sep = u16::from(b'\\');
            let trimmed = match path.iter().rposition(|&u| u != sep) {
                Some(last) => &path[..=last],
                None => path,
            };
            let start = trimmed.iter().rposition(|&u| u == sep).map_or(0, |i| i + 1);
            rec.name.extend_from_slice(&trimmed[start..]);
        }
    }
    Ok(())
}

/// 讀取整份快照並把每個項目交給接收端（`report` 的分析器或 `overview`）。
pub fn read(input: &mut impl BufRead, an: &mut impl Sink) -> Result<Parsed, String> {
    let mut r = Reader::open(input)?;
    let mut rec = Rec::default();
    while r.next_into(&mut rec)? {
        if rec.is_dir {
            an.dir(&DirView {
                path: &rec.path,
                depth: rec.depth,
                rollup: rec.rollup,
                direct: rec.direct,
                latest_mtime: rec.mtime,
            });
        } else {
            an.file(&FileView {
                path: &rec.path,
                ext: &rec.ext,
                logical: rec.logical,
                physical: rec.physical,
                mtime: rec.mtime,
                secondary: rec.secondary,
            });
        }
    }
    r.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{Analyzer, Request};

    const META: &str = r#"{"type":"meta","schema":"voltdir.snapshot/1","tool_version":"0.0.1","options":{"min_size":null,"max_depth":3}}"#;
    const DIR: &str = r#"{"type":"dir","path":"C:\\r","depth":0,"latest_mtime":"2020-01-01T00:00:00Z","rollup":{"files":1,"dirs":0,"logical":5,"physical":8},"direct":{"files":1,"logical":5,"physical":8}}"#;
    const FILE: &str = r#"{"type":"file","path":"C:\\r\\a.txt","ext":".txt","size_logical":5,"size_physical":8,"mtime":"2020-01-01T00:00:00Z","hardlink_secondary":false}"#;
    const SUMMARY: &str = r#"{"type":"summary","totals":{"files":1,"dirs":1,"logical":5,"physical":8},"coverage":{"complete_dirs":1,"incomplete_dirs":0,"errors":{},"incomplete_paths":[],"truncated":false}}"#;

    fn run(text: &str) -> Result<Parsed, String> {
        let mut an = Analyzer::new(Request {
            top_files: Some(5),
            ..Default::default()
        });
        read(&mut text.as_bytes(), &mut an)
    }

    #[test]
    fn valid_snapshot_is_accepted_and_options_are_read() {
        let text = format!("{META}\n{DIR}\n{FILE}\n{SUMMARY}\n");
        let p = run(&text).unwrap();
        assert_eq!(p.max_depth, Some(3));
        assert_eq!(p.min_size, None);
        assert_eq!(p.totals.files, 1);
        assert_eq!(p.coverage.complete_dirs, 1);
        let meta = String::from_utf8(p.meta_body).unwrap();
        assert!(
            !meta.contains("\"type\"")
                && !meta.contains("\"schema\"")
                && meta.contains("tool_version")
        );
    }

    #[test]
    fn line_spans_cover_every_byte() {
        // 空行與 \r\n 照舊被跳過，但位元組位置要把它們算進去
        let text = format!("{META}\r\n\n{DIR}\n\n\n{FILE}\r\n{SUMMARY}\n");
        let bytes = text.as_bytes();
        let mut input = bytes;
        let mut r = Reader::open(&mut input).unwrap();
        let (off, len) = r.line_span();
        assert_eq!(
            &bytes[off as usize..(off as usize + len as usize)],
            format!("{META}\r\n").as_bytes()
        );
        let mut rec = Rec::default();
        let mut seen = Vec::new();
        while r.next_into(&mut rec).unwrap() {
            let (off, len) = r.line_span();
            let slice = &bytes[off as usize..off as usize + len as usize];
            let trimmed = slice.strip_suffix(b"\n").unwrap();
            let trimmed = trimmed.strip_suffix(b"\r").unwrap_or(trimmed);
            assert_eq!(trimmed, r.raw());
            seen.push((off, len));
        }
        assert_eq!(seen.len(), 2);
        assert!(
            seen[0].0 + u64::from(seen[0].1) < seen[1].0,
            "blank lines sit between"
        );
    }

    #[test]
    fn missing_summary_is_reported_as_truncated() {
        let text = format!("{META}\n{DIR}\n{FILE}\n");
        assert!(run(&text).unwrap_err().contains("truncated"));
        // 最後一行被截斷到一半
        let cut = format!("{META}\n{DIR}\n{}", &FILE[..FILE.len() / 2]);
        assert!(run(&cut).is_err());
        assert!(run("").unwrap_err().contains("empty"));
    }

    #[test]
    fn structural_problems_are_rejected_with_a_line_number() {
        let wrong_schema = META.replace("snapshot/1", "snapshot/9");
        assert!(
            run(&format!("{wrong_schema}\n{SUMMARY}\n"))
                .unwrap_err()
                .contains("unsupported schema")
        );
        assert!(
            run(&format!("{DIR}\n{SUMMARY}\n"))
                .unwrap_err()
                .contains("first line")
        );
        assert!(
            run(&format!("{META}\n{META}\n{SUMMARY}\n"))
                .unwrap_err()
                .contains("more than one meta")
        );
        assert!(
            run(&format!("{META}\n{SUMMARY}\n{FILE}\n"))
                .unwrap_err()
                .contains("after the summary")
        );
        let no_size = FILE.replace("\"size_physical\":8,", "");
        let e = run(&format!("{META}\n{DIR}\n{no_size}\n{SUMMARY}\n")).unwrap_err();
        assert!(
            e.starts_with("line 3:") && e.contains("size_physical"),
            "{e}"
        );
        assert!(
            run(&format!("{META}\nnot json\n{SUMMARY}\n"))
                .unwrap_err()
                .starts_with("line 2:")
        );
        let bad_time = FILE.replace("2020-01-01T00:00:00Z", "yesterday");
        assert!(
            run(&format!("{META}\n{bad_time}\n{SUMMARY}\n"))
                .unwrap_err()
                .contains("valid time")
        );
        let unknown = r#"{"type":"mystery"}"#;
        assert!(
            run(&format!("{META}\n{unknown}\n{SUMMARY}\n"))
                .unwrap_err()
                .contains("unknown line type")
        );
    }

    #[test]
    fn crlf_and_blank_lines_are_tolerated() {
        let text = format!("{META}\r\n\r\n{DIR}\r\n{FILE}\r\n{SUMMARY}\r\n");
        assert!(run(&text).is_ok());
    }
}
