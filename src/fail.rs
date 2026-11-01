//! 錯誤訊息的統一輸出。
//!
//! 文字模式是兩行：`error: <發生什麼>` 與 `hint: <怎麼辦>`；`--log json` 時是單一 JSON 物件。
//! 訊息裡的路徑是資料，不可帶著控制字元把輸出弄成多行。

use crate::json;
use std::ffi::OsString;
use std::sync::atomic::{AtomicBool, Ordering};

static LOG_JSON: AtomicBool = AtomicBool::new(false);

pub fn set_json(on: bool) {
    LOG_JSON.store(on, Ordering::Relaxed);
}

pub fn is_json() -> bool {
    LOG_JSON.load(Ordering::Relaxed)
}

/// 在完整解析命令列之前就決定錯誤格式，這樣連用法錯誤也能輸出 JSON。
pub fn detect_json(args: &[OsString]) -> bool {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--log=json" {
            return true;
        }
        if a == "--log" {
            return it.next().is_some_and(|v| v == "json");
        }
    }
    false
}

/// 文字模式用：控制字元換成可見的 `\u{..}`，確保訊息各自只佔一行。
fn flatten(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if json::needs_escape(c) {
            out.push_str(&format!("\\u{{{:x}}}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

pub fn render(json_mode: bool, code: &str, message: &str, hint: &str) -> String {
    if json_mode {
        let mut b = Vec::new();
        b.extend_from_slice(b"{\"level\":\"error\",\"code\":");
        json::push_str(&mut b, code);
        b.extend_from_slice(b",\"message\":");
        json::push_str(&mut b, message);
        b.extend_from_slice(b",\"hint\":");
        json::push_str(&mut b, hint);
        b.extend_from_slice(b"}\n");
        String::from_utf8(b).expect("json is utf-8")
    } else {
        format!("error: {}\nhint: {}\n", flatten(message), flatten(hint))
    }
}

/// 把一則錯誤寫到標準錯誤。`code` 是穩定的代碼，之後不隨文字改動。
pub fn print(code: &str, message: &str, hint: &str) {
    eprint!(
        "{}",
        render(LOG_JSON.load(Ordering::Relaxed), code, message, hint)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_mode_is_two_lines() {
        let s = render(false, "x", "boom", "try again");
        assert_eq!(s, "error: boom\nhint: try again\n");
    }

    #[test]
    fn text_mode_flattens_control_characters_in_paths() {
        let s = render(false, "x", "bad path a\nb\u{1b}[31m", "h");
        assert_eq!(s.lines().count(), 2);
        assert!(!s.contains('\u{1b}'));
    }

    #[test]
    fn json_mode_is_one_parsable_line() {
        let s = render(true, "out_dir_missing", "a \"b\"\n", "hint");
        assert_eq!(s.lines().count(), 1);
        assert!(s.starts_with("{\"level\":\"error\",\"code\":\"out_dir_missing\""));
        assert!(s.contains("\\n"));
        assert!(s.contains("\"hint\":\"hint\""));
    }

    #[test]
    fn detects_log_json_in_either_spelling() {
        let v = |a: &[&str]| a.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(detect_json(&v(&["scan", "C:\\", "--log", "json"])));
        assert!(detect_json(&v(&["scan", "--log=json", "C:\\"])));
        assert!(!detect_json(&v(&["scan", "--log", "text"])));
        assert!(!detect_json(&v(&["scan", "C:\\"])));
    }
}
