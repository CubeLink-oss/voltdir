//! 名稱排序與副檔名。

use std::cmp::Ordering;

/// 單一 UTF-16 單位的簡單大寫對應（只處理基本多文種平面中可一對一對應者）。
pub fn fold(u: u16) -> u16 {
    if u < 128 {
        return if (0x61..=0x7A).contains(&u) {
            u - 32
        } else {
            u
        };
    }
    match char::from_u32(u32::from(u)) {
        Some(c) => {
            let mut it = c.to_uppercase();
            match (it.next(), it.next()) {
                (Some(up), None) if (up as u32) <= 0xFFFF => up as u32 as u16,
                _ => u,
            }
        }
        None => u, // 代理字元保持不變
    }
}

/// 先做不分大小寫的序數比較；相同時再做區分大小寫的序數比較。
pub fn cmp_names(a: &[u16], b: &[u16]) -> Ordering {
    // 逐單位比較摺疊後的值；第一個不同處決定結果。與「先比摺疊序列、再比原字串」等價，
    // 但不經過迭代器鏈，是掃描時排序每個目錄檔案名稱的熱路徑。
    let n = a.len().min(b.len());
    for i in 0..n {
        let (x, y) = (a[i], b[i]);
        if x != y {
            let (fx, fy) = (fold(x), fold(y));
            if fx != fy {
                return fx.cmp(&fy);
            }
        }
    }
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

/// 轉小寫（逐字元；不成對的代理字元原樣保留）。
fn lower_u16(units: &[u16]) -> Vec<u16> {
    let mut out = Vec::with_capacity(units.len());
    for r in char::decode_utf16(units.iter().copied()) {
        match r {
            Ok(c) => {
                let mut buf = [0u16; 2];
                for lc in c.to_lowercase() {
                    out.extend_from_slice(lc.encode_utf16(&mut buf));
                }
            }
            Err(e) => out.push(e.unpaired_surrogate()),
        }
    }
    out
}

/// 副檔名：最後一個點起的字串（含點）轉小寫；沒有點則為空（B-12）。
pub fn ext_of(name: &[u16]) -> Vec<u16> {
    match name.iter().rposition(|&u| u == u16::from(b'.')) {
        Some(i) => lower_u16(&name[i..]),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    fn sorted(mut v: Vec<&str>) -> Vec<&str> {
        v.sort_by(|a, b| cmp_names(&w(a), &w(b)));
        v
    }

    #[test]
    fn case_insensitive_first() {
        assert_eq!(sorted(vec!["b", "A", "a", "B"]), vec!["A", "a", "B", "b"]);
        assert_eq!(
            sorted(vec!["file10", "File2", "file1"]),
            vec!["file1", "file10", "File2"]
        );
    }

    #[test]
    fn case_only_difference_is_ordered_deterministically() {
        // 排序規則：大寫（碼位較小）排在同名小寫之前
        assert_eq!(cmp_names(&w("Readme"), &w("README")), Ordering::Greater);
        assert_eq!(cmp_names(&w("README"), &w("Readme")), Ordering::Less);
        assert_eq!(cmp_names(&w("a"), &w("a")), Ordering::Equal);
    }

    #[test]
    fn ordinal_not_locale() {
        // 序數比較（折成大寫後）：'-'(0x2D) < 'B'(0x42) < '_'(0x5F)
        assert_eq!(sorted(vec!["a_b", "ab", "a-b"]), vec!["a-b", "ab", "a_b"]);
    }

    #[test]
    fn non_ascii_folds() {
        assert_eq!(cmp_names(&w("é"), &w("É")), Ordering::Greater);
        assert_eq!(sorted(vec!["ä", "z"]), vec!["z", "ä"]);
    }

    #[test]
    fn extension_rules_b12() {
        assert_eq!(ext_of(&w("Report.TAR.GZ")), w(".gz"));
        assert_eq!(ext_of(&w("Makefile")), w(""));
        assert_eq!(ext_of(&w(".gitignore")), w(".gitignore"));
        assert_eq!(ext_of(&w("a.")), w("."));
        assert_eq!(ext_of(&[0x61, 0x2E, 0xD800]), vec![0x2E, 0xD800]);
    }

    /// 熱路徑的手寫迴圈必須與「先比摺疊序列、再比原字串」的原始定義完全一致。
    #[test]
    fn cmp_names_matches_the_reference_definition() {
        fn reference(a: &[u16], b: &[u16]) -> Ordering {
            let ci = a.iter().map(|&u| fold(u)).cmp(b.iter().map(|&u| fold(u)));
            ci.then_with(|| a.cmp(b))
        }
        let pool: Vec<u16> = "aAbBzZ_-.0 éÉαΑ中".encode_utf16().collect();
        let mut seed = 12345u64;
        let mut next = move || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as usize
        };
        for _ in 0..20_000 {
            let mut mk = || -> Vec<u16> {
                let len = next() % 6;
                (0..len).map(|_| pool[next() % pool.len()]).collect()
            };
            let a = mk();
            let b = mk();
            assert_eq!(cmp_names(&a, &b), reference(&a, &b), "{a:?} vs {b:?}");
        }
    }
}
