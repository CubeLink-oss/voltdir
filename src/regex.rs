//! `find --name-regex` 用的正規表示式：自己實作的 Thompson NFA 模擬，執行時間與
//! 「模式長度 × 文字長度」成正比，沒有回溯，因此不會有指數級的最壞情況。
//!
//! 支援：字面字元、`.`、`[...]`（範圍、`^` 否定、跳脫）、`\d \w \s \D \W \S`、`\` 跳脫、
//! `( )`、`|`、`* + ?`、`{n}`、`{n,}`、`{n,m}`、`^`、`$`。比對是「文字中任何位置符合即成立」
//! （和 grep 一樣；要整段比對請自行加 `^`、`$`），不分大小寫，以 UTF-16 單位為單位
//! （`.` 比對一個單位，所以一個代理字元對需要兩個 `.`）。不支援反向參照與環視。

use crate::names::fold;

/// 展開後的指令數上限；`a{1000}{1000}` 這類模式會被拒絕，而不是耗盡記憶體。
const MAX_PROGRAM: usize = 20_000;
const MAX_REPEAT: u32 = 1000;

#[derive(Debug, Clone)]
enum Node {
    Empty,
    Char(u16),
    Any,
    Class(Box<Class>),
    Start,
    End,
    Cat(Vec<Node>),
    Alt(Vec<Node>),
    Repeat(Box<Node>, u32, Option<u32>),
}

#[derive(Debug, Clone, Default)]
struct Class {
    negated: bool,
    ranges: Vec<(u16, u16)>,
}

impl Class {
    fn matches(&self, c: u16) -> bool {
        let f = fold(c);
        let lower = lower_of(c);
        let hit = self.ranges.iter().any(|&(lo, hi)| {
            (lo..=hi).contains(&c) || (lo..=hi).contains(&f) || (lo..=hi).contains(&lower)
        });
        hit != self.negated
    }
}

fn lower_of(c: u16) -> u16 {
    match char::from_u32(u32::from(c)) {
        Some(ch) => {
            let mut it = ch.to_lowercase();
            match (it.next(), it.next()) {
                (Some(l), None) if (l as u32) <= 0xFFFF => l as u32 as u16,
                _ => c,
            }
        }
        None => c,
    }
}

#[derive(Debug, Clone)]
enum Inst {
    Char(u16),
    Any,
    Class(Box<Class>),
    Start,
    End,
    Split(usize, usize),
    Jmp(usize),
    Match,
}

#[derive(Debug)]
pub struct Regex {
    prog: Vec<Inst>,
}

struct Parser<'a> {
    s: &'a [u16],
    i: usize,
    depth: usize,
}

type PResult<T> = Result<T, String>;

impl Parser<'_> {
    fn peek(&self) -> Option<u16> {
        self.s.get(self.i).copied()
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c as u16) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn alt(&mut self) -> PResult<Node> {
        let mut branches = vec![self.cat()?];
        while self.eat('|') {
            branches.push(self.cat()?);
        }
        Ok(if branches.len() == 1 {
            branches.pop().expect("one branch")
        } else {
            Node::Alt(branches)
        })
    }

    fn cat(&mut self) -> PResult<Node> {
        let mut items = Vec::new();
        while let Some(c) = self.peek() {
            if c == u16::from(b'|') || c == u16::from(b')') {
                break;
            }
            items.push(self.repeat()?);
        }
        Ok(match items.len() {
            0 => Node::Empty,
            1 => items.pop().expect("one item"),
            _ => Node::Cat(items),
        })
    }

    fn repeat(&mut self) -> PResult<Node> {
        let mut atom = self.atom()?;
        loop {
            let (min, max) = if self.eat('*') {
                (0, None)
            } else if self.eat('+') {
                (1, None)
            } else if self.eat('?') {
                (0, Some(1))
            } else if self.peek() == Some(u16::from(b'{')) {
                match self.counted()? {
                    Some(r) => r,
                    None => break,
                }
            } else {
                break;
            };
            atom = Node::Repeat(Box::new(atom), min, max);
        }
        Ok(atom)
    }

    /// `{n}`、`{n,}`、`{n,m}`；不是這個形式時視為字面的 `{`（回傳 `None`，位置不動）。
    fn counted(&mut self) -> PResult<Option<(u32, Option<u32>)>> {
        let save = self.i;
        self.i += 1;
        let num = |p: &mut Self| -> Option<u32> {
            let start = p.i;
            let mut n: u32 = 0;
            while let Some(c) = p.peek() {
                if !(0x30..=0x39).contains(&c) {
                    break;
                }
                n = n.saturating_mul(10).saturating_add(u32::from(c - 0x30));
                p.i += 1;
            }
            (p.i > start).then_some(n)
        };
        let Some(min) = num(self) else {
            self.i = save;
            return Ok(None);
        };
        let max = if self.eat(',') {
            if self.peek() == Some(u16::from(b'}')) {
                None
            } else {
                match num(self) {
                    Some(m) => Some(m),
                    None => {
                        self.i = save;
                        return Ok(None);
                    }
                }
            }
        } else {
            Some(min)
        };
        if !self.eat('}') {
            self.i = save;
            return Ok(None);
        }
        if min > MAX_REPEAT || max.is_some_and(|m| m > MAX_REPEAT || m < min) {
            return Err(format!(
                "repeat counts must be at most {MAX_REPEAT} and min <= max"
            ));
        }
        Ok(Some((min, max)))
    }

    fn atom(&mut self) -> PResult<Node> {
        let c = self.peek().ok_or("unexpected end of pattern")?;
        self.i += 1;
        match char::from_u32(u32::from(c)) {
            Some('(') => {
                self.depth += 1;
                if self.depth > 100 {
                    return Err("groups are nested too deeply".into());
                }
                let inner = self.alt()?;
                self.depth -= 1;
                if !self.eat(')') {
                    return Err("missing closing ')'".into());
                }
                Ok(inner)
            }
            Some('.') => Ok(Node::Any),
            Some('^') => Ok(Node::Start),
            Some('$') => Ok(Node::End),
            Some('[') => self.class(),
            Some('\\') => self.escape(false).map(|e| match e {
                Escape::Unit(u) => Node::Char(u),
                Escape::Class(c) => Node::Class(Box::new(c)),
            }),
            Some('*' | '+' | '?') => Err("nothing to repeat".into()),
            _ => Ok(Node::Char(c)),
        }
    }

    fn escape(&mut self, in_class: bool) -> PResult<Escape> {
        let c = self.peek().ok_or("a backslash at the end of the pattern")?;
        self.i += 1;
        let ch = char::from_u32(u32::from(c));
        let shorthand = |neg: bool, ranges: &[(u16, u16)]| {
            Ok(Escape::Class(Class {
                negated: neg,
                ranges: ranges.to_vec(),
            }))
        };
        let digits: &[(u16, u16)] = &[(0x30, 0x39)];
        let word: &[(u16, u16)] = &[(0x30, 0x39), (0x41, 0x5A), (0x61, 0x7A), (0x5F, 0x5F)];
        let space: &[(u16, u16)] = &[(0x20, 0x20), (0x09, 0x0D)];
        match ch {
            Some('d') => shorthand(false, digits),
            Some('D') => shorthand(true, digits),
            Some('w') => shorthand(false, word),
            Some('W') => shorthand(true, word),
            Some('s') => shorthand(false, space),
            Some('S') => shorthand(true, space),
            Some('t') => Ok(Escape::Unit(0x09)),
            Some('n') => Ok(Escape::Unit(0x0A)),
            Some('r') => Ok(Escape::Unit(0x0D)),
            Some(x) if x.is_ascii_alphanumeric() => {
                let _ = in_class;
                Err(format!("unknown escape '\\{x}'"))
            }
            _ => Ok(Escape::Unit(c)),
        }
    }

    fn class(&mut self) -> PResult<Node> {
        let mut cls = Class::default();
        if self.eat('^') {
            cls.negated = true;
        }
        let mut first = true;
        loop {
            let c = self.peek().ok_or("missing closing ']'")?;
            if c == u16::from(b']') && !first {
                self.i += 1;
                break;
            }
            first = false;
            self.i += 1;
            let lo = if c == u16::from(b'\\') {
                match self.escape(true)? {
                    Escape::Unit(u) => u,
                    Escape::Class(sub) => {
                        // `\d`、`\w`、`\s` 在字元集合裡展開成範圍（否定形式 `\D` 等不支援）
                        if sub.negated {
                            return Err(
                                "negated shorthand classes are not supported inside [...]".into()
                            );
                        }
                        cls.ranges.extend(sub.ranges);
                        continue;
                    }
                }
            } else {
                c
            };
            // 範圍 a-z（`-` 在最後或最前面是字面字元）
            if self.peek() == Some(u16::from(b'-'))
                && self
                    .s
                    .get(self.i + 1)
                    .is_some_and(|&n| n != u16::from(b']'))
            {
                self.i += 1;
                let hc = self.peek().ok_or("missing closing ']'")?;
                self.i += 1;
                let hi = if hc == u16::from(b'\\') {
                    match self.escape(true)? {
                        Escape::Unit(u) => u,
                        Escape::Class(_) => return Err("invalid range in [...]".into()),
                    }
                } else {
                    hc
                };
                if hi < lo {
                    return Err("range is out of order in [...]".into());
                }
                cls.ranges.push((lo, hi));
            } else {
                cls.ranges.push((lo, lo));
            }
        }
        Ok(Node::Class(Box::new(cls)))
    }
}

enum Escape {
    Unit(u16),
    Class(Class),
}

fn emit(node: &Node, prog: &mut Vec<Inst>) -> PResult<()> {
    if prog.len() > MAX_PROGRAM {
        return Err("the pattern is too large".into());
    }
    match node {
        Node::Empty => {}
        Node::Char(c) => prog.push(Inst::Char(fold(*c))),
        Node::Any => prog.push(Inst::Any),
        Node::Class(c) => prog.push(Inst::Class(c.clone())),
        Node::Start => prog.push(Inst::Start),
        Node::End => prog.push(Inst::End),
        Node::Cat(items) => {
            for n in items {
                emit(n, prog)?;
            }
        }
        Node::Alt(branches) => {
            let mut jumps = Vec::new();
            for (k, b) in branches.iter().enumerate() {
                if k + 1 < branches.len() {
                    let split = prog.len();
                    prog.push(Inst::Split(split + 1, 0));
                    emit(b, prog)?;
                    jumps.push(prog.len());
                    prog.push(Inst::Jmp(0));
                    let next = prog.len();
                    prog[split] = Inst::Split(split + 1, next);
                } else {
                    emit(b, prog)?;
                }
            }
            let end = prog.len();
            for j in jumps {
                prog[j] = Inst::Jmp(end);
            }
        }
        Node::Repeat(inner, min, max) => {
            for _ in 0..*min {
                emit(inner, prog)?;
            }
            match max {
                None => {
                    // inner*：L1: split L2, L3; L2: inner; jmp L1; L3:
                    let l1 = prog.len();
                    prog.push(Inst::Split(l1 + 1, 0));
                    emit(inner, prog)?;
                    prog.push(Inst::Jmp(l1));
                    let l3 = prog.len();
                    prog[l1] = Inst::Split(l1 + 1, l3);
                }
                Some(m) => {
                    // 還可以再出現 m-min 次，每次都是可省略的
                    let mut splits = Vec::new();
                    for _ in *min..*m {
                        splits.push(prog.len());
                        prog.push(Inst::Split(0, 0));
                        emit(inner, prog)?;
                    }
                    let end = prog.len();
                    for s in splits {
                        prog[s] = Inst::Split(s + 1, end);
                    }
                }
            }
        }
    }
    Ok(())
}

impl Regex {
    pub fn new(pattern: &[u16]) -> Result<Regex, String> {
        let mut p = Parser {
            s: pattern,
            i: 0,
            depth: 0,
        };
        let ast = p.alt()?;
        if p.i < pattern.len() {
            return Err("unmatched ')'".into());
        }
        let mut prog = Vec::new();
        emit(&ast, &mut prog)?;
        prog.push(Inst::Match);
        Ok(Regex { prog })
    }

    /// 文字中任何位置是否符合。
    pub fn is_match(&self, text: &[u16]) -> bool {
        self.is_match_with(text, &mut self.scratch())
    }

    /// 比對大量名稱時用：暫存空間可以重複使用，不必每個名稱配置三個向量。
    pub fn scratch(&self) -> Scratch {
        let n = self.prog.len();
        Scratch {
            cur: Threads::new(n),
            next: Threads::new(n),
            stack: Vec::new(),
        }
    }

    pub fn is_match_with(&self, text: &[u16], scratch: &mut Scratch) -> bool {
        let Scratch { cur, next, stack } = scratch;
        cur.clear();
        next.clear();
        for pos in 0..=text.len() {
            // 每個位置都可以開始一次新的比對（未錨定的搜尋）
            self.add(cur, 0, pos, text.len(), stack);
            if cur
                .list
                .iter()
                .any(|&pc| matches!(self.prog[pc], Inst::Match))
            {
                return true;
            }
            if pos == text.len() {
                break;
            }
            let c = text[pos];
            let fc = fold(c);
            next.clear();
            for k in 0..cur.list.len() {
                let pc = cur.list[k];
                let ok = match &self.prog[pc] {
                    Inst::Char(x) => *x == fc,
                    Inst::Any => true,
                    Inst::Class(cls) => cls.matches(c),
                    _ => false,
                };
                if ok {
                    self.add(next, pc + 1, pos + 1, text.len(), stack);
                }
            }
            std::mem::swap(cur, next);
        }
        false
    }

    /// 把 `pc` 與它經由 Split／Jmp／錨點可到達的所有「會消耗字元或結束」的指令加入。
    fn add(&self, set: &mut Threads, pc: usize, pos: usize, len: usize, stack: &mut Vec<usize>) {
        stack.clear();
        stack.push(pc);
        while let Some(pc) = stack.pop() {
            if set.mark[pc] {
                continue;
            }
            set.mark[pc] = true;
            match &self.prog[pc] {
                Inst::Jmp(t) => stack.push(*t),
                Inst::Split(a, b) => {
                    stack.push(*b);
                    stack.push(*a);
                }
                Inst::Start => {
                    if pos == 0 {
                        stack.push(pc + 1);
                    }
                }
                Inst::End => {
                    if pos == len {
                        stack.push(pc + 1);
                    }
                }
                _ => set.list.push(pc),
            }
        }
    }
}

/// `is_match_with` 的暫存空間；同一個執行緒可以一直重複使用。
pub struct Scratch {
    cur: Threads,
    next: Threads,
    stack: Vec<usize>,
}

struct Threads {
    list: Vec<usize>,
    mark: Vec<bool>,
}

impl Threads {
    fn new(n: usize) -> Threads {
        Threads {
            list: Vec::with_capacity(n),
            mark: vec![false; n],
        }
    }

    fn clear(&mut self) {
        self.list.clear();
        self.mark.iter_mut().for_each(|m| *m = false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    fn m(p: &str, t: &str) -> bool {
        Regex::new(&w(p)).unwrap().is_match(&w(t))
    }

    #[test]
    fn literals_anchors_and_case() {
        assert!(m("abc", "xxABCxx"));
        assert!(!m("^abc", "xabc"));
        assert!(m("^abc$", "ABC"));
        assert!(!m("^abc$", "abcd"));
        assert!(m("資料", "我的資料夾"));
    }

    #[test]
    fn classes_and_shorthands() {
        assert!(m("^[a-c]+$", "abcAB"));
        assert!(!m("^[^a-c]+$", "abx"));
        assert!(m(r"^\d{3}-\d{4}$", "555-1234"));
        assert!(m(r"^\w+\.txt$", "my_file1.TXT"));
        assert!(m(r"a\.b", "a.b"));
        assert!(!m(r"a\.b", "axb"));
        assert!(m(r"[\]]", "]"));
        assert!(m("[a-]", "-"));
    }

    #[test]
    fn alternation_groups_and_repeats() {
        assert!(m("^(cat|dog)s?$", "Dogs"));
        assert!(!m("^(cat|dog)s?$", "birds"));
        assert!(m("^a{2,3}$", "aaa"));
        assert!(!m("^a{2,3}$", "aaaa"));
        assert!(!m("^a{2,3}$", "a"));
        assert!(m("^(ab)*$", ""));
        assert!(m("^(ab)*c$", "ababc"));
        assert!(m("a{,3}", "a{,3}")); // 不是合法計數，視為字面
    }

    #[test]
    fn special_characters_in_names() {
        assert!(m(r"\*", "a*b"));
        assert!(m(r"\[1\]", "file[1].txt"));
        assert!(m("a b", "a b"));
        assert!(m(r"\?", "what?"));
    }

    #[test]
    fn invalid_patterns_are_rejected() {
        for bad in [
            "(", ")", "*a", "[a", "a{3,2}", r"\q", "a{2000}", "[z-a]", "+",
        ] {
            assert!(Regex::new(&w(bad)).is_err(), "{bad} should be rejected");
        }
        // 展開後太大
        assert!(Regex::new(&w("((a{1000}){1000}){1000}")).is_err());
    }

    #[test]
    fn catastrophic_patterns_run_in_linear_time() {
        // 回溯式引擎在這種模式上會指數級變慢；NFA 模擬必須立即完成
        let text = "a".repeat(5000) + "b";
        let r = Regex::new(&w("^(a+)+$")).unwrap();
        let t = std::time::Instant::now();
        assert!(!r.is_match(&w(&text)));
        assert!(t.elapsed().as_secs() < 5, "took {:?}", t.elapsed());
        let r2 = Regex::new(&w("(a|aa)+c")).unwrap();
        assert!(!r2.is_match(&w(&text)));
    }
}
