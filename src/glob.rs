//! 萬用字元比對（不分大小寫）：`*` 比對任意長度（含路徑分隔符），`?` 比對單一 UTF-16 單位。
//! 樣式裡的 `/` 視同 `\`：Windows 路徑兩種分隔符都合法，檔名又不可能含其中任何一個，
//! 所以 `*/node_modules/*` 與 `*\node_modules\*` 是同一個意思（路徑本身一律用 `\`）。

use crate::names::fold;

/// 樣式的單位與文字的單位是否相同（`/` 當成 `\`，其餘不分大小寫）。
fn same(pattern_unit: u16, text_unit: u16) -> bool {
    let p = if pattern_unit == u16::from(b'/') {
        u16::from(b'\\')
    } else {
        pattern_unit
    };
    fold(p) == fold(text_unit)
}

pub fn matches(pattern: &[u16], text: &[u16]) -> bool {
    let star = u16::from(b'*');
    let any = u16::from(b'?');
    let (mut p, mut t) = (0usize, 0usize);
    // 最近一個 `*` 的位置，以及它目前吞到文字的哪裡（回溯用）
    let mut backtrack: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && pattern[p] == star {
            backtrack = Some((p, t));
            p += 1;
        } else if p < pattern.len() && (pattern[p] == any || same(pattern[p], text[t])) {
            p += 1;
            t += 1;
        } else if let Some((bp, bt)) = backtrack {
            p = bp + 1;
            t = bt + 1;
            backtrack = Some((bp, bt + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&c| c == star)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(p: &str, t: &str) -> bool {
        let w = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
        matches(&w(p), &w(t))
    }

    #[test]
    fn star_and_question() {
        assert!(m("*.txt", "a.txt"));
        assert!(m("*.TXT", "a.txt"));
        assert!(!m("*.txt", "a.txt.bak"));
        assert!(m("a?c", "abc"));
        assert!(!m("a?c", "ac"));
        assert!(m("*", ""));
        assert!(m("**a**", "bab"));
        assert!(!m("a", ""));
    }

    #[test]
    fn star_crosses_path_separators() {
        assert!(m(r"C:\Users\*\AppData\*", r"C:\Users\bob\AppData\Local"));
        assert!(m(r"*\node_modules", r"C:\proj\sub\node_modules"));
        assert!(!m(r"*\node_modules", r"C:\proj\node_modules2"));
    }

    #[test]
    fn forward_slash_in_a_pattern_means_backslash() {
        // 回歸：`*/node_modules/*` 曾經什麼都比對不到，命令照樣成功，使用者以為已經排除
        assert!(m(
            "*/node_modules/*",
            r"C:\proj\node_modules\left-pad\index.js"
        ));
        assert!(m("C:/Users/*/AppData/*", r"C:\Users\bob\AppData\Local"));
        assert!(m(r"*\node_modules\*", r"C:\proj\node_modules\x"));
        assert!(m("*/NODE_MODULES", r"c:\proj\node_modules"));
        assert!(!m("*/node_modules/*", r"C:\proj\node_modules2\x"));
        // 路徑裡的 / 不會被當成分隔符：只有樣式那一側有這個等價
        assert!(!m(r"*\node_modules\*", "C:/proj/node_modules/x"));
    }

    #[test]
    fn brackets_and_spaces_are_literal() {
        assert!(m("a[1] b", "a[1] b"));
        assert!(!m("a[1]", "a1"));
        assert!(m("資料*", "資料夾"));
    }

    #[test]
    fn backtracking_cases() {
        assert!(m("*ab*ab", "xabyabab"));
        assert!(!m("*ab*ab", "xabyaba"));
    }
}
