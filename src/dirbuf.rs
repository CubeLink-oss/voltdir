//! 解析 `FILE_ID_EXTD_DIR_INFO` 緩衝區（純位元組解析；每個讀取都有邊界檢查）。
//!
//! 結構（公開文件）：
//! 0 NextEntryOffset(4) 4 FileIndex(4) 8 CreationTime 16 LastAccessTime 24 LastWriteTime
//! 32 ChangeTime 40 EndOfFile(8) 48 AllocationSize(8) 56 FileAttributes(4) 60 FileNameLength(4)
//! 64 EaSize(4) 68 ReparsePointTag(4) 72 FileId(16) 88 FileName

const HEADER: usize = 88;

pub struct RawEntry<'a> {
    pub name: &'a [u8],
    pub attrs: u32,
    pub logical: u64,
    pub physical: u64,
    pub mtime: u64,
    pub reparse_tag: u32,
    pub id: u128,
}

impl RawEntry<'_> {
    pub fn name_units(&self) -> impl Iterator<Item = u16> + '_ {
        self.name
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
    }
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

/// 走訪緩衝區內的所有項目。輸入格式不合法（截斷、偏移越界、迴圈）時回傳 `Err`，
/// 已回呼的項目有效。任何輸入都不會 panic 或越界。
pub fn for_each_entry<'a>(buf: &'a [u8], mut f: impl FnMut(RawEntry<'a>)) -> Result<(), ()> {
    let mut pos = 0usize;
    loop {
        let rec = buf.get(pos..).ok_or(())?;
        if rec.len() < HEADER {
            return Err(());
        }
        let next = u32_at(rec, 0).ok_or(())? as usize;
        let name_len = u32_at(rec, 60).ok_or(())? as usize;
        let name = rec
            .get(HEADER..HEADER.checked_add(name_len).ok_or(())?)
            .ok_or(())?;
        if !name_len.is_multiple_of(2) {
            return Err(());
        }
        let id = u128::from_le_bytes(rec.get(72..88).ok_or(())?.try_into().map_err(|_| ())?);
        f(RawEntry {
            name,
            attrs: u32_at(rec, 56).ok_or(())?,
            logical: u64_at(rec, 40).ok_or(())?,
            physical: u64_at(rec, 48).ok_or(())?,
            mtime: u64_at(rec, 24).ok_or(())?,
            reparse_tag: u32_at(rec, 68).ok_or(())?,
            id,
        });
        if next == 0 {
            return Ok(());
        }
        // 偏移必須前進且至少容納標頭，避免無限迴圈
        if next < HEADER {
            return Err(());
        }
        pos = pos.checked_add(next).ok_or(())?;
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// 構造一筆記錄（測試用）。
    pub fn record(name: &str, attrs: u32, eof: u64, alloc: u64, pad_to: usize) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut r = vec![0u8; HEADER];
        r[24..32].copy_from_slice(&7u64.to_le_bytes());
        r[40..48].copy_from_slice(&eof.to_le_bytes());
        r[48..56].copy_from_slice(&alloc.to_le_bytes());
        r[56..60].copy_from_slice(&attrs.to_le_bytes());
        r[60..64].copy_from_slice(&((units.len() * 2) as u32).to_le_bytes());
        r[72..88].copy_from_slice(&0x1122u128.to_le_bytes());
        for u in units {
            r.extend_from_slice(&u.to_le_bytes());
        }
        while !r.len().is_multiple_of(8) || r.len() < pad_to {
            r.push(0);
        }
        r
    }

    fn chain(mut recs: Vec<Vec<u8>>) -> Vec<u8> {
        let last = recs.len() - 1;
        for (i, r) in recs.iter_mut().enumerate() {
            if i != last {
                let n = r.len() as u32;
                r[0..4].copy_from_slice(&n.to_le_bytes());
            }
        }
        recs.concat()
    }

    #[test]
    fn parses_a_chain_of_entries() {
        let buf = chain(vec![
            record(".", 0x10, 0, 0, 0),
            record("a.txt", 0x20, 1234, 4096, 0),
            record("資料", 0x20, 5, 0, 0),
        ]);
        let mut seen = Vec::new();
        for_each_entry(&buf, |e| {
            seen.push((
                e.name_units().collect::<Vec<_>>(),
                e.logical,
                e.physical,
                e.id,
            ))
        })
        .unwrap();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[1].1, 1234);
        assert_eq!(seen[1].2, 4096);
        assert_eq!(seen[1].3, 0x1122);
        assert_eq!(seen[2].0, "資料".encode_utf16().collect::<Vec<_>>());
    }

    #[test]
    fn rejects_truncated_and_hostile_buffers_without_panicking() {
        let good = chain(vec![record("a", 0x20, 1, 1, 0), record("b", 0x20, 1, 1, 0)]);
        // 任意截斷
        for cut in 0..good.len() {
            let _ = for_each_entry(&good[..cut], |_| {});
        }
        // 名稱長度超過緩衝區
        let mut bad = record("a", 0x20, 1, 1, 0);
        bad[60..64].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
        assert!(for_each_entry(&bad, |_| {}).is_err());
        // 奇數名稱長度
        let mut odd = record("a", 0x20, 1, 1, 0);
        odd[60..64].copy_from_slice(&3u32.to_le_bytes());
        assert!(for_each_entry(&odd, |_| {}).is_err());
        // NextEntryOffset 指向自己或更小（無限迴圈）
        let mut looped = record("a", 0x20, 1, 1, 0);
        looped[0..4].copy_from_slice(&8u32.to_le_bytes());
        assert!(for_each_entry(&looped, |_| {}).is_err());
        // NextEntryOffset 越界
        let mut far = record("a", 0x20, 1, 1, 0);
        far[0..4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        assert!(for_each_entry(&far, |_| {}).is_err());
        // 全 0xFF 與空緩衝區
        assert!(for_each_entry(&vec![0xFF; 256], |_| {}).is_err());
        assert!(for_each_entry(&[], |_| {}).is_err());
    }
}
