//! 萬用字元比對（不分大小寫）：`*` 比對任意長度（含路徑分隔符），`?` 比對單一 UTF-16 單位。
//! 樣式裡的 `/` 視同 `\`：Windows 路徑兩種分隔符都合法，檔名又不可能含其中任何一個，
//! 所以 `*/node_modules/*` 與 `*\node_modules\*` 是同一個意思（路徑本身一律用 `\`）。

use crate::names::fold;
use std::sync::atomic::{AtomicBool, Ordering};

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

/// 一組預先編譯好的排除樣式，並記錄每個樣式是否至少命中過一次。
///
/// 實際使用的樣式幾乎都是固定形狀：`*\node_modules`（以某字串結尾）、`*\.git\*`（含有某字串）、
/// `C:\Windows`（整條路徑相同）。這些不需要通用比對器的回溯；而實際路徑平均超過一百個字元、
/// 一次掃描要比對上百萬次（每個項目乘上每個樣式），差別很大：編譯過的規則每條路徑約 0.5 微秒，
/// 通用比對器約 3.7 微秒。答案與 `matches` 完全相同，測試逐一核對。
#[derive(Default)]
pub struct ExcludeSet {
    rules: Vec<Rule>,
    hits: Vec<AtomicBool>,
}

enum Rule {
    /// `*字面`：路徑以它結尾（含最常見的 `*\名稱`）
    Suffix(Box<[u16]>),
    /// `*字面*`：路徑含有它
    Contains(Box<[u16]>),
    /// `字面*`：路徑以它開頭
    Prefix(Box<[u16]>),
    /// 沒有萬用字元：整條路徑相同
    Exact(Box<[u16]>),
    /// 其他（內部有 `*`，或有 `?`）：交給通用比對器
    Glob(Box<[u16]>),
}

/// 字面字元轉成比對用的形式：`/` 當成 `\`，再轉成大寫（與 `same` 一致）。
fn fold_literal(s: &[u16]) -> Box<[u16]> {
    s.iter()
        .map(|&c| {
            fold(if c == u16::from(b'/') {
                u16::from(b'\\')
            } else {
                c
            })
        })
        .collect()
}

fn compile(pattern: &[u16]) -> Rule {
    let star = u16::from(b'*');
    let any = u16::from(b'?');
    let lead = pattern.first() == Some(&star);
    let trail = pattern.len() > 1 && pattern.last() == Some(&star);
    let body = &pattern[usize::from(lead)..pattern.len() - usize::from(trail)];
    if body.iter().any(|&c| c == star || c == any) {
        return Rule::Glob(pattern.into());
    }
    let lit = fold_literal(body);
    match (lead, trail) {
        (true, true) => Rule::Contains(lit),
        (true, false) => Rule::Suffix(lit),
        (false, true) => Rule::Prefix(lit),
        (false, false) => Rule::Exact(lit),
    }
}

/// `text` 從 `at` 開始是否逐字等於已轉成大寫的字面 `lit`（呼叫者保證長度夠）。
fn eq_at(text: &[u16], at: usize, lit: &[u16]) -> bool {
    lit.iter().zip(&text[at..]).all(|(&l, &t)| {
        // 不是字母的 ASCII 字面只可能等於它自己，省下（較慢的）轉大寫
        if l < 128 && !(0x41..=0x5A).contains(&l) {
            t == l
        } else {
            fold(t) == l
        }
    })
}

impl Rule {
    fn matches(&self, path: &[u16]) -> bool {
        match self {
            Rule::Suffix(l) => path.len() >= l.len() && eq_at(path, path.len() - l.len(), l),
            Rule::Prefix(l) => path.len() >= l.len() && eq_at(path, 0, l),
            Rule::Exact(l) => path.len() == l.len() && eq_at(path, 0, l),
            Rule::Contains(l) => {
                path.len() >= l.len() && (0..=path.len() - l.len()).any(|i| eq_at(path, i, l))
            }
            Rule::Glob(p) => matches(p, path),
        }
    }
}

impl ExcludeSet {
    pub fn new(patterns: &[Vec<u16>]) -> Self {
        Self {
            rules: patterns.iter().map(|p| compile(p)).collect(),
            hits: patterns.iter().map(|_| AtomicBool::new(false)).collect(),
        }
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// 路徑是否被任何樣式排除。每個樣式都會比對（不提早結束），
    /// 這樣才分得出哪些樣式從頭到尾沒用到。
    pub fn is_match(&self, path: &[u16]) -> bool {
        let mut any = false;
        for (rule, hit) in self.rules.iter().zip(&self.hits) {
            if rule.matches(path) {
                // 先讀再寫：多執行緒掃描時避免每筆命中都去搶同一條快取線
                if !hit.load(Ordering::Relaxed) {
                    hit.store(true, Ordering::Relaxed);
                }
                any = true;
            }
        }
        any
    }

    /// 從頭到尾沒有命中任何路徑的樣式（編號與建立時的順序相同）。
    pub fn unmatched(&self) -> Vec<usize> {
        (0..self.hits.len())
            .filter(|&i| !self.hits[i].load(Ordering::Relaxed))
            .collect()
    }
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

    fn u(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn compiled_rules_give_exactly_the_generic_matchers_answers() {
        // 每種形狀：*後綴、*包含*、前綴*、整條相同、內部有 *、有 ?、只有 *、空樣式、`/`、大小寫、非 ASCII
        let patterns = [
            r"*\node_modules",
            "*/node_modules",
            r"*\NODE_MODULES",
            r"*\Program Files*",
            r"*\.git\*",
            r"C:\Windows",
            "C:/Windows",
            r"c:\windows",
            r"C:\Users\*",
            "*.tmp",
            "*",
            "**",
            "",
            "a?c",
            "*a*b*",
            "?*",
            r"*\a\b\*",
            r"*\資料夾",
            r"*\資料夾*",
            "*ß*",
            "*ı",
            "*İ*",
            "*i",
        ];
        let paths = [
            "",
            r"C:\\",
            r"C:\",
            r"C:\proj",
            r"C:\proj\node_modules",
            r"C:\proj\NODE_MODULES",
            r"C:\proj\Node_Modules\x\index.js",
            r"C:\proj\node_modules2",
            r"C:\proj\sub\node_modules",
            "node_modules",
            r"\node_modules",
            r"C:\Program Files\x",
            r"C:\Program Files (x86)",
            r"c:\program files",
            r"C:\Windows",
            r"c:\WINDOWS",
            r"C:\windows\system32",
            r"C:\Users\bob\a.tmp",
            r"C:\Users",
            r"C:\Users\",
            r"D:\x\.git\HEAD",
            r"D:\x\.GIT\objects",
            r"D:\x\.git",
            r"D:\x\.gitignore",
            "abc",
            "aXc",
            "ac",
            "a1b2",
            r"C:\a\b\c",
            r"C:\x\a\b",
            r"資料夾\x",
            r"C:\資料夾",
            r"C:\x\資料夾\y.pdf",
            r"C:\x\ı",
            r"C:\x\I",
            r"C:\x\i",
            r"C:\x\İ",
            r"C:\x\ß",
            r"C:\x\SS",
        ];
        for pat in patterns {
            let pattern = u(pat);
            let set = ExcludeSet::new(std::slice::from_ref(&pattern));
            for path in paths {
                let text = u(path);
                assert_eq!(
                    set.is_match(&text),
                    matches(&pattern, &text),
                    "pattern {pat:?} against {path:?}"
                );
            }
        }
        // 一整組一起：任何一個命中就算命中，與逐一用通用比對器再 any 相同
        let all: Vec<Vec<u16>> = patterns.iter().map(|p| u(p)).collect();
        let set = ExcludeSet::new(&all);
        for path in paths {
            let text = u(path);
            assert_eq!(
                set.is_match(&text),
                all.iter().any(|p| matches(p, &text)),
                "whole set against {path:?}"
            );
        }
    }

    #[test]
    fn the_set_remembers_which_patterns_never_matched() {
        let set = ExcludeSet::new(&[
            u(r"*\node_modules"),
            u(r"*\typo"),
            u(r"*\.git\*"),
            u("*.tmp"),
        ]);
        assert_eq!(set.unmatched(), [0, 1, 2, 3], "nothing evaluated yet");
        assert!(set.is_match(&u(r"C:\p\node_modules")));
        assert!(set.is_match(&u(r"C:\p\a.tmp")));
        assert!(!set.is_match(&u(r"C:\p\src")));
        // 命中第一個樣式時，後面的樣式仍然會被評估：`*.tmp` 不能因為排在後面就被誤報成沒用到
        assert_eq!(set.unmatched(), [1, 2]);
        assert_eq!(set.len(), 4);
        assert_eq!(ExcludeSet::default().len(), 0);
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
