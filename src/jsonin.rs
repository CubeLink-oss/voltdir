//! 讀取快照用的最小 JSON 解析器（一行一個物件）。
//!
//! 只支援本工具輸出的子集：物件、陣列、字串、整數、布林、null。字串解碼為 UTF-16，
//! 因此 `\ud800` 這類不成對代理字元可以原樣保留。對任何輸入都不得 panic。

const MAX_DEPTH: usize = 32;

#[derive(Debug, Clone, PartialEq)]
pub enum Val<'a> {
    Null,
    Bool(bool),
    Num(i64),
    Str(Vec<u16>),
    Arr(Vec<Val<'a>>),
    Obj(Vec<(&'a str, Val<'a>)>),
}

impl<'a> Val<'a> {
    pub fn get(&self, key: &str) -> Option<&Val<'a>> {
        match self {
            Val::Obj(v) => v.iter().find(|(k, _)| *k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Val::Num(n) if *n >= 0 => Some(*n as u64),
            _ => None,
        }
    }

    pub fn as_units(&self) -> Option<&[u16]> {
        match self {
            Val::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_string(&self) -> Option<String> {
        self.as_units().map(String::from_utf16_lossy)
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Val::Bool(b) => Some(*b),
            _ => None,
        }
    }
}

pub fn parse(line: &[u8]) -> Result<Val<'_>, String> {
    let mut p = Parser { b: line, i: 0 };
    let v = p.value(0)?;
    p.ws();
    if p.i != line.len() {
        return Err("trailing characters after JSON value".into());
    }
    Ok(v)
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\r' | b'\n') {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn expect(&mut self, c: u8) -> Result<(), String> {
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        } else {
            Err(format!("expected '{}' at offset {}", c as char, self.i))
        }
    }

    fn literal(&mut self, word: &str, v: Val<'a>) -> Result<Val<'a>, String> {
        if self.b[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            Ok(v)
        } else {
            Err(format!("invalid literal at offset {}", self.i))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Val<'a>, String> {
        if depth > MAX_DEPTH {
            return Err("JSON nested too deeply".into());
        }
        self.ws();
        match self.peek().ok_or("unexpected end of input")? {
            b'{' => {
                self.i += 1;
                let mut out = Vec::new();
                self.ws();
                if self.peek() == Some(b'}') {
                    self.i += 1;
                    return Ok(Val::Obj(out));
                }
                loop {
                    self.ws();
                    let key = self.key()?;
                    self.ws();
                    self.expect(b':')?;
                    let v = self.value(depth + 1)?;
                    out.push((key, v));
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(Val::Obj(out));
                        }
                        _ => return Err(format!("expected ',' or '}}' at offset {}", self.i)),
                    }
                }
            }
            b'[' => {
                self.i += 1;
                let mut out = Vec::new();
                self.ws();
                if self.peek() == Some(b']') {
                    self.i += 1;
                    return Ok(Val::Arr(out));
                }
                loop {
                    out.push(self.value(depth + 1)?);
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(Val::Arr(out));
                        }
                        _ => return Err(format!("expected ',' or ']' at offset {}", self.i)),
                    }
                }
            }
            b'"' => Ok(Val::Str(self.string()?)),
            b't' => self.literal("true", Val::Bool(true)),
            b'f' => self.literal("false", Val::Bool(false)),
            b'n' => self.literal("null", Val::Null),
            b'-' | b'0'..=b'9' => self.number(),
            c => Err(format!(
                "unexpected character '{}' at offset {}",
                c as char, self.i
            )),
        }
    }

    fn number(&mut self) -> Result<Val<'a>, String> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        let digits = self.i;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.i += 1;
        }
        if self.i == digits {
            return Err(format!("invalid number at offset {start}"));
        }
        if matches!(self.peek(), Some(b'.' | b'e' | b'E')) {
            return Err("non-integer numbers are not supported".into());
        }
        std::str::from_utf8(&self.b[start..self.i])
            .ok()
            .and_then(|s| s.parse::<i64>().ok())
            .map(Val::Num)
            .ok_or_else(|| format!("number out of range at offset {start}"))
    }

    /// 物件鍵：只接受不含跳脫的字串（本工具的欄位名稱皆為 ASCII）。
    fn key(&mut self) -> Result<&'a str, String> {
        self.expect(b'"')?;
        let start = self.i;
        while let Some(c) = self.peek() {
            match c {
                b'"' => {
                    let s = std::str::from_utf8(&self.b[start..self.i])
                        .map_err(|_| "invalid UTF-8 in key".to_string())?;
                    self.i += 1;
                    return Ok(s);
                }
                b'\\' => return Err("escapes in object keys are not supported".into()),
                _ => self.i += 1,
            }
        }
        Err("unterminated string".into())
    }

    fn string(&mut self) -> Result<Vec<u16>, String> {
        self.expect(b'"')?;
        let mut out: Vec<u16> = Vec::new();
        loop {
            let start = self.i;
            while let Some(c) = self.peek() {
                if c == b'"' || c == b'\\' || c < 0x20 {
                    break;
                }
                self.i += 1;
            }
            if self.i > start {
                let s = std::str::from_utf8(&self.b[start..self.i])
                    .map_err(|_| "invalid UTF-8 in string".to_string())?;
                out.extend(s.encode_utf16());
            }
            match self.peek() {
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.i += 1;
                    let c = self.peek().ok_or("unterminated escape")?;
                    self.i += 1;
                    match c {
                        b'"' => out.push(0x22),
                        b'\\' => out.push(0x5C),
                        b'/' => out.push(0x2F),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'n' => out.push(10),
                        b'r' => out.push(13),
                        b't' => out.push(9),
                        b'u' => {
                            let hex = self
                                .b
                                .get(self.i..self.i + 4)
                                .ok_or("truncated \\u escape")?;
                            let s = std::str::from_utf8(hex).map_err(|_| "invalid \\u escape")?;
                            let u = u16::from_str_radix(s, 16).map_err(|_| "invalid \\u escape")?;
                            out.push(u);
                            self.i += 4;
                        }
                        _ => return Err(format!("invalid escape at offset {}", self.i)),
                    }
                }
                Some(_) => return Err("control character in string".into()),
                None => return Err("unterminated string".into()),
            }
        }
    }
}

/// 把解析結果重新輸出為 JSON（用於把快照的 `meta`、`coverage` 原樣帶入報表）。
pub fn write(out: &mut Vec<u8>, v: &Val<'_>) {
    use std::io::Write;
    match v {
        Val::Null => out.extend_from_slice(b"null"),
        Val::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        Val::Num(n) => {
            let _ = write!(out, "{n}");
        }
        Val::Str(s) => crate::json::push_str_u16(out, s),
        Val::Arr(a) => {
            out.push(b'[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write(out, x);
            }
            out.push(b']');
        }
        Val::Obj(o) => {
            out.push(b'{');
            for (i, (k, x)) in o.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                crate::json::push_str(out, k);
                out.push(b':');
                write(out, x);
            }
            out.push(b'}');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_objects_arrays_and_scalars() {
        let v = parse(br#"{"a":1,"b":[true,false,null],"c":"x\"y","d":{"e":-5}}"#).unwrap();
        assert_eq!(v.get("a").unwrap().as_u64(), Some(1));
        assert_eq!(v.get("c").unwrap().as_string().as_deref(), Some("x\"y"));
        assert_eq!(v.get("d").unwrap().get("e"), Some(&Val::Num(-5)));
        assert!(matches!(v.get("b"), Some(Val::Arr(a)) if a.len() == 3));
    }

    #[test]
    fn unpaired_surrogate_escapes_survive_a_round_trip() {
        let src = br#"{"p":"a\ud800b","q":"\udc00","r":"\u00e9\n"}"#;
        let v = parse(src).unwrap();
        assert_eq!(
            v.get("p").unwrap().as_units().unwrap(),
            [0x61, 0xD800, 0x62]
        );
        let mut out = Vec::new();
        write(&mut out, &v);
        assert_eq!(out, r#"{"p":"a\ud800b","q":"\udc00","r":"é\n"}"#.as_bytes());
    }

    #[test]
    fn rejects_malformed_input_without_panicking() {
        for bad in [
            &b""[..],
            b"{",
            b"{\"a\":",
            b"{\"a\":1",
            b"{\"a\":1}}",
            b"[1,",
            b"\"abc",
            b"\"\\u12\"",
            b"\"\\x\"",
            b"{\"a\\n\":1}",
            b"1.5",
            b"99999999999999999999",
            b"{\"a\":tru}",
            b"\xff\xfe",
            b"\"\xff\"",
            b"-",
        ] {
            assert!(
                parse(bad).is_err(),
                "should reject {:?}",
                String::from_utf8_lossy(bad)
            );
        }
        let deep = "[".repeat(1000);
        assert!(parse(deep.as_bytes()).is_err());
    }

    #[test]
    fn every_truncation_of_a_valid_line_is_rejected_or_valid_but_never_panics() {
        let line = br#"{"type":"file","path":"C:\\a\\b","size_logical":12,"x":[1,2,{"y":null}]}"#;
        for cut in 0..line.len() {
            let _ = parse(&line[..cut]);
        }
        assert!(parse(line).is_ok());
    }
}
