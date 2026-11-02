#![allow(dead_code)]
//! 整合測試共用工具：執行 voltdir、解析輸出、建立夾具。

use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq)]
pub enum J {
    Null,
    Bool(bool),
    Num(i128),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

impl J {
    pub fn get(&self, k: &str) -> &J {
        match self {
            J::Obj(v) => v
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v)
                .unwrap_or(&J::Null),
            _ => &J::Null,
        }
    }
    pub fn get_idx(&self, i: usize) -> &J {
        match self {
            J::Arr(v) => v.get(i).unwrap_or(&J::Null),
            _ => &J::Null,
        }
    }
    pub fn str(&self) -> &str {
        match self {
            J::Str(s) => s,
            other => panic!("not a string: {other:?}"),
        }
    }
    pub fn num(&self) -> i128 {
        match self {
            J::Num(n) => *n,
            other => panic!("not a number: {other:?}"),
        }
    }
    pub fn bool(&self) -> bool {
        match self {
            J::Bool(b) => *b,
            other => panic!("not a bool: {other:?}"),
        }
    }
    pub fn is_null(&self) -> bool {
        *self == J::Null
    }
}

pub fn parse_json(s: &str) -> J {
    let b = s.as_bytes();
    let mut i = 0;
    let v = value(b, &mut i);
    skip_ws(b, &mut i);
    assert_eq!(i, b.len(), "trailing data in JSON line");
    v
}

fn skip_ws(b: &[u8], i: &mut usize) {
    while *i < b.len() && b[*i].is_ascii_whitespace() {
        *i += 1;
    }
}

fn value(b: &[u8], i: &mut usize) -> J {
    skip_ws(b, i);
    match b[*i] {
        b'{' => {
            *i += 1;
            let mut out = Vec::new();
            skip_ws(b, i);
            if b[*i] == b'}' {
                *i += 1;
                return J::Obj(out);
            }
            loop {
                skip_ws(b, i);
                let k = string(b, i);
                skip_ws(b, i);
                assert_eq!(b[*i], b':');
                *i += 1;
                out.push((k, value(b, i)));
                skip_ws(b, i);
                match b[*i] {
                    b',' => *i += 1,
                    b'}' => {
                        *i += 1;
                        return J::Obj(out);
                    }
                    c => panic!("unexpected {}", c as char),
                }
            }
        }
        b'[' => {
            *i += 1;
            let mut out = Vec::new();
            skip_ws(b, i);
            if b[*i] == b']' {
                *i += 1;
                return J::Arr(out);
            }
            loop {
                out.push(value(b, i));
                skip_ws(b, i);
                match b[*i] {
                    b',' => *i += 1,
                    b']' => {
                        *i += 1;
                        return J::Arr(out);
                    }
                    c => panic!("unexpected {}", c as char),
                }
            }
        }
        b'"' => J::Str(string(b, i)),
        b't' => {
            *i += 4;
            J::Bool(true)
        }
        b'f' => {
            *i += 5;
            J::Bool(false)
        }
        b'n' => {
            *i += 4;
            J::Null
        }
        _ => {
            let st = *i;
            while *i < b.len() && (b[*i] == b'-' || b[*i].is_ascii_digit()) {
                *i += 1;
            }
            J::Num(
                std::str::from_utf8(&b[st..*i])
                    .unwrap()
                    .parse()
                    .expect("number"),
            )
        }
    }
}

fn string(b: &[u8], i: &mut usize) -> String {
    assert_eq!(b[*i], b'"');
    *i += 1;
    let mut units: Vec<u16> = Vec::new();
    while b[*i] != b'"' {
        if b[*i] == b'\\' {
            *i += 1;
            match b[*i] {
                b'n' => units.push(10),
                b'r' => units.push(13),
                b't' => units.push(9),
                b'u' => {
                    let h = std::str::from_utf8(&b[*i + 1..*i + 5]).unwrap();
                    units.push(u16::from_str_radix(h, 16).unwrap());
                    *i += 4;
                }
                c => units.push(u16::from(c)),
            }
            *i += 1;
        } else {
            // 原樣的 UTF-8 位元組：解出完整字元
            let len = match b[*i] {
                0..=0x7F => 1,
                0xC0..=0xDF => 2,
                0xE0..=0xEF => 3,
                _ => 4,
            };
            let ch = std::str::from_utf8(&b[*i..*i + len])
                .unwrap()
                .chars()
                .next()
                .unwrap();
            units.extend_from_slice(ch.encode_utf16(&mut [0u16; 2]));
            *i += len;
        }
    }
    *i += 1;
    String::from_utf16_lossy(&units)
}

pub struct Run {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl Run {
    pub fn text(&self) -> String {
        String::from_utf8(self.stdout.clone()).expect("stdout must be valid UTF-8")
    }
    pub fn lines(&self) -> Vec<J> {
        self.text().lines().map(parse_json).collect()
    }
}

/// 執行 voltdir。`scan` 沒有 `--out` 時不輸出到標準輸出，所以對 `scan`
/// 沒寫 `--out` 的呼叫，這裡替測試明確加上 `--out -`；要測「真的沒有 --out」請用 `voltdir_raw`。
pub fn voltdir(args: &[&std::ffi::OsStr]) -> Run {
    let is_scan = args.first().is_some_and(|a| *a == "scan");
    let has_out = args.iter().any(|a| {
        let s = a.to_string_lossy();
        s == "--out" || s == "-o" || s.starts_with("--out=")
    });
    if is_scan && !has_out {
        let mut v: Vec<&std::ffi::OsStr> = args.to_vec();
        v.extend_from_slice(&["--out".as_ref(), "-".as_ref()]);
        return voltdir_raw(&v);
    }
    voltdir_raw(args)
}

/// 原樣執行 voltdir，不替測試補任何參數。
pub fn voltdir_raw(args: &[&std::ffi::OsStr]) -> Run {
    let out = Command::new(env!("CARGO_BIN_EXE_voltdir"))
        .args(args)
        .output()
        .expect("run voltdir");
    Run {
        code: out.status.code().unwrap_or(-1),
        stdout: out.stdout,
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

pub fn scan(path: &Path) -> Run {
    voltdir(&["scan".as_ref(), path.as_os_str(), "-q".as_ref()])
}

/// 夾具資料夾（位於 target\fixtures，已被 git 忽略）。每次重新建立。
pub fn fixture(name: &str) -> PathBuf {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("fixtures")
        .join(name);
    // Windows 刪掉資料夾之後，路徑有時還會短暫處於「待刪除」狀態（防毒或索引服務還握著它），
    // 馬上重建偶爾會失敗；等一下重試就好，不要讓整個測試因此亂掉
    let mut last_error = None;
    for _ in 0..40 {
        let _ = std::fs::remove_dir_all(&p);
        match std::fs::create_dir_all(&p) {
            Ok(()) => return p,
            Err(e) => {
                last_error = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
    panic!("create fixture {}: {:?}", p.display(), last_error);
}

/// 取得 type 與 path 相符的那一行。
pub fn find<'a>(lines: &'a [J], ty: &str, path: &Path) -> &'a J {
    let want = path.to_string_lossy().to_string();
    lines
        .iter()
        .find(|l| l.get("type").str() == ty && l.get("path").str().eq_ignore_ascii_case(&want))
        .unwrap_or_else(|| panic!("no {ty} line for {want}"))
}

pub fn summary(lines: &[J]) -> &J {
    let s = lines.last().expect("lines");
    assert_eq!(s.get("type").str(), "summary", "last line must be summary");
    s
}
