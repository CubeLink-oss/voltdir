//! NTFS 磁碟格式解析（依公開的 NTFS 格式文件：開機磁區、檔案記錄、屬性、執行串列）。
//!
//! 全部是安全程式碼：每次讀取都經過邊界檢查，任何輸入都只會得到 `Err` 或部分結果，
//! 不會 panic、越界或無限迴圈（屬性迴圈每圈至少前進 16 位元組；執行串列每筆至少 2 位元組）。

/// 檔案記錄 ID 的低 48 位元是記錄編號，高 16 位元是序號。
pub const REF_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
/// 根目錄的記錄編號。
pub const ROOT_RECORD: u64 = 5;
/// 編號小於此值者是檔案系統保留的中繼資料檔。
pub const RESERVED_RECORDS: u64 = 24;

const ATTR_STANDARD_INFORMATION: u32 = 0x10;
const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_DATA: u32 = 0x80;
const ATTR_REPARSE_POINT: u32 = 0xC0;
const ATTR_END: u32 = 0xFFFF_FFFF;

pub const NS_DOS: u8 = 2;

const TAG_MOUNT_POINT: u32 = 0xA000_0003;
/// 夾帶式壓縮（WOF）把壓縮後內容放在這個具名資料串流。
const WOF_STREAM: &str = "WofCompressedData";
const VOLUME_PREFIX: &str = r"\??\Volume{";

/// 更新序列陣列的保護間隔固定為 512 位元組。
const FIXUP_STRIDE: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
pub enum Error {
    BadBoot,
    /// 資料不足或不是有效的檔案記錄
    BadMagic,
    /// 更新序列陣列驗證失敗（損壞，或讀到寫入一半的記錄）
    BadFixup,
    BadHeader,
    BadAttribute,
    BadRuns,
}

fn u16le(b: &[u8], o: usize) -> Option<u16> {
    let s = b.get(o..o.checked_add(2)?)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

fn u32le(b: &[u8], o: usize) -> Option<u32> {
    let s = b.get(o..o.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn u64le(b: &[u8], o: usize) -> Option<u64> {
    let s = b.get(o..o.checked_add(8)?)?;
    let mut a = [0u8; 8];
    a.copy_from_slice(s);
    Some(u64::from_le_bytes(a))
}

/// 開機磁區中與掃描有關的欄位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Boot {
    pub bytes_per_sector: u32,
    pub cluster_bytes: u32,
    pub record_bytes: u32,
    pub mft_lcn: u64,
    pub total_clusters: u64,
    pub serial: u64,
}

pub fn parse_boot(b: &[u8]) -> Result<Boot, Error> {
    if b.len() < 512 || &b[3..11] != b"NTFS    " {
        return Err(Error::BadBoot);
    }
    let bps = u32::from(u16le(b, 0x0B).ok_or(Error::BadBoot)?);
    if !matches!(bps, 512 | 1024 | 2048 | 4096) {
        return Err(Error::BadBoot);
    }
    let spc_raw = u32::from(b[0x0D]);
    let spc = if spc_raw == 0 {
        return Err(Error::BadBoot);
    } else if spc_raw <= 0x80 {
        spc_raw
    } else {
        // 0xF4 之類：2 的 (256 - 值) 次方
        let shift = 256 - spc_raw;
        if shift > 12 {
            return Err(Error::BadBoot);
        }
        1u32 << shift
    };
    if !spc.is_power_of_two() {
        return Err(Error::BadBoot);
    }
    let cluster_bytes = bps.checked_mul(spc).ok_or(Error::BadBoot)?;
    if cluster_bytes > 2 * 1024 * 1024 {
        return Err(Error::BadBoot);
    }
    let total_sectors = u64le(b, 0x28).ok_or(Error::BadBoot)?;
    let mft_lcn = u64le(b, 0x30).ok_or(Error::BadBoot)?;
    let total_clusters = total_sectors / u64::from(spc);
    if total_clusters == 0 || mft_lcn >= total_clusters {
        return Err(Error::BadBoot);
    }
    let rec = b[0x40] as i8;
    let record_bytes = if rec > 0 {
        cluster_bytes
            .checked_mul(rec as u32)
            .ok_or(Error::BadBoot)?
    } else {
        let shift = u32::from(rec.unsigned_abs());
        if !(9..=16).contains(&shift) {
            return Err(Error::BadBoot);
        }
        1u32 << shift
    };
    if !(512..=65536).contains(&record_bytes) || record_bytes % FIXUP_STRIDE as u32 != 0 {
        return Err(Error::BadBoot);
    }
    Ok(Boot {
        bytes_per_sector: bps,
        cluster_bytes,
        record_bytes,
        mft_lcn,
        total_clusters,
        serial: u64le(b, 0x48).ok_or(Error::BadBoot)?,
    })
}

/// 資料串流的一段連續叢集；`lcn` 為 `None` 表示稀疏（沒有配置）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub vcn: u64,
    pub len: u64,
    pub lcn: Option<u64>,
}

/// 解碼執行串列，對每一段呼叫 `f`。到資料尾端沒有結束符號也視為結束。
pub fn parse_runs(b: &[u8], start_vcn: u64, mut f: impl FnMut(Run)) -> Result<(), Error> {
    let mut i = 0usize;
    let mut vcn = start_vcn;
    let mut lcn: i64 = 0;
    while let Some(&h) = b.get(i) {
        if h == 0 {
            break;
        }
        let ls = usize::from(h & 0x0F);
        let os = usize::from(h >> 4);
        if ls == 0 || ls > 8 || os > 8 {
            return Err(Error::BadRuns);
        }
        i += 1;
        let len_bytes = b.get(i..i + ls).ok_or(Error::BadRuns)?;
        i += ls;
        let mut len = 0u64;
        for (k, &x) in len_bytes.iter().enumerate() {
            len |= u64::from(x) << (8 * k);
        }
        if len == 0 {
            return Err(Error::BadRuns);
        }
        let mut run_lcn = None;
        if os > 0 {
            let off_bytes = b.get(i..i + os).ok_or(Error::BadRuns)?;
            i += os;
            let mut v = 0u64;
            for (k, &x) in off_bytes.iter().enumerate() {
                v |= u64::from(x) << (8 * k);
            }
            // 符號延伸
            if off_bytes[os - 1] & 0x80 != 0 && os < 8 {
                v |= u64::MAX << (8 * os);
            }
            lcn = lcn.checked_add(v as i64).ok_or(Error::BadRuns)?;
            if lcn < 0 {
                return Err(Error::BadRuns);
            }
            run_lcn = Some(lcn as u64);
        }
        f(Run {
            vcn,
            len,
            lcn: run_lcn,
        });
        vcn = vcn.checked_add(len).ok_or(Error::BadRuns)?;
    }
    Ok(())
}

/// 套用更新序列陣列：驗證每個 512 位元組區段尾端的序號，並還原被取代的原始位元組。
pub fn apply_fixup(rec: &mut [u8]) -> Result<(), Error> {
    if rec.len() < FIXUP_STRIDE || !rec.len().is_multiple_of(FIXUP_STRIDE) {
        return Err(Error::BadMagic);
    }
    let usa_off = usize::from(u16le(rec, 4).ok_or(Error::BadHeader)?);
    let usa_count = usize::from(u16le(rec, 6).ok_or(Error::BadHeader)?);
    let sectors = rec.len() / FIXUP_STRIDE;
    if usa_count != sectors + 1 || usa_off < 0x2A || usa_off + 2 * usa_count > rec.len() {
        return Err(Error::BadFixup);
    }
    let usn = [rec[usa_off], rec[usa_off + 1]];
    for i in 1..usa_count {
        let end = i * FIXUP_STRIDE;
        if rec[end - 2] != usn[0] || rec[end - 1] != usn[1] {
            return Err(Error::BadFixup);
        }
        rec[end - 2] = rec[usa_off + 2 * i];
        rec[end - 1] = rec[usa_off + 2 * i + 1];
    }
    Ok(())
}

/// 檔案名稱屬性（位置指向記錄緩衝區內的 UTF-16LE 名稱）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NameRef {
    pub parent: u64,
    pub parent_seq: u16,
    pub namespace: u8,
    pub off: u32,
    pub units: u16,
}

/// 一筆檔案記錄解析出的資訊。可重複使用以避免配置。
#[derive(Debug, Default)]
pub struct Record {
    pub in_use: bool,
    pub is_dir: bool,
    pub seq: u16,
    pub links: u16,
    /// 這是延伸記錄（基底記錄參照不為 0；$MFT 自己的延伸記錄參照 0 號、序號 ≥ 1，也算）
    pub is_ext: bool,
    /// 延伸記錄所屬的基底記錄編號
    pub base: u64,
    pub has_si: bool,
    pub si_attrs: u32,
    pub mtime: u64,
    pub names: Vec<NameRef>,
    pub has_reparse: bool,
    pub reparse_tag: u32,
    pub reparse_volume: bool,
    /// 重解析屬性為非常駐時，資料第一段的起始叢集（0 表示沒有）
    pub reparse_lcn: u64,
    pub has_attr_list: bool,
    /// 屬性清單的值在記錄緩衝區內的位置（常駐時）
    pub attr_list: Option<(usize, usize)>,
    pub has_data: bool,
    pub data_resident: bool,
    /// 見到起始 VCN 為 0 的區段（只有該區段帶有大小）
    pub data_first: bool,
    pub data_logical: u64,
    /// 預設資料串流中有配置的叢集數（不含稀疏）
    pub data_clusters: u64,
    /// `WofCompressedData` 串流中有配置的叢集數
    pub wof_clusters: u64,
    pub data_runs: Vec<Run>,
}

/// 名稱的 UTF-16 單位（`buf` 是解析時使用的同一個記錄緩衝區）。
pub fn name_units(buf: &[u8], n: NameRef) -> impl Iterator<Item = u16> + '_ {
    let start = n.off as usize;
    let end = start + 2 * usize::from(n.units);
    buf.get(start..end)
        .unwrap_or(&[])
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
}

impl Record {
    fn reset(&mut self) {
        self.in_use = false;
        self.is_dir = false;
        self.seq = 0;
        self.links = 0;
        self.is_ext = false;
        self.base = 0;
        self.has_si = false;
        self.si_attrs = 0;
        self.mtime = 0;
        self.names.clear();
        self.has_reparse = false;
        self.reparse_tag = 0;
        self.reparse_volume = false;
        self.reparse_lcn = 0;
        self.has_attr_list = false;
        self.attr_list = None;
        self.has_data = false;
        self.data_resident = false;
        self.data_first = false;
        self.data_logical = 0;
        self.data_clusters = 0;
        self.wof_clusters = 0;
        self.data_runs.clear();
    }
}

/// 比對屬性名稱（UTF-16LE 位元組）是否等於 ASCII 字串 `s`。
fn name_equals(bytes: &[u8], s: &str) -> bool {
    bytes.len() == s.len() * 2
        && bytes
            .chunks_exact(2)
            .zip(s.bytes())
            .all(|(c, a)| c[0] == a && c[1] == 0)
}

fn name_starts_with(bytes: &[u8], s: &str) -> bool {
    bytes.len() >= s.len() * 2
        && bytes
            .chunks_exact(2)
            .zip(s.bytes())
            .all(|(c, a)| c[0] == a && c[1] == 0)
}

/// 解析一筆檔案記錄。`buf` 的長度必須是整筆記錄（512 的倍數）；更新序列陣列會就地還原。
///
/// - 未使用的記錄（旗標沒有 IN_USE）回傳 `Ok` 且 `in_use == false`。
/// - 全零的記錄（從未使用）回傳 `Err(BadMagic)`，由呼叫端決定是否算作損壞。
/// - `keep_runs` 為真時把預設資料串流的執行串列存進 `out.data_runs`。
pub fn parse_record(buf: &mut [u8], keep_runs: bool, out: &mut Record) -> Result<(), Error> {
    out.reset();
    if buf.len() < FIXUP_STRIDE || !buf.len().is_multiple_of(FIXUP_STRIDE) {
        return Err(Error::BadMagic);
    }
    match &buf[0..4] {
        b"FILE" => {}
        b"BAAD" => return Err(Error::BadFixup),
        _ => return Err(Error::BadMagic),
    }
    apply_fixup(buf)?;
    let buf: &[u8] = buf;
    let flags = u16le(buf, 0x16).ok_or(Error::BadHeader)?;
    out.in_use = flags & 1 != 0;
    out.is_dir = flags & 2 != 0;
    if !out.in_use {
        return Ok(());
    }
    out.seq = u16le(buf, 0x10).ok_or(Error::BadHeader)?;
    out.links = u16le(buf, 0x12).ok_or(Error::BadHeader)?;
    let first = usize::from(u16le(buf, 0x14).ok_or(Error::BadHeader)?);
    let used = u32le(buf, 0x18).ok_or(Error::BadHeader)? as usize;
    let base_ref = u64le(buf, 0x20).ok_or(Error::BadHeader)?;
    out.is_ext = base_ref != 0;
    out.base = base_ref & REF_MASK;
    if used > buf.len() || first < 0x18 || first + 8 > used {
        return Err(Error::BadHeader);
    }
    let mut p = first;
    loop {
        let Some(t) = u32le(buf, p) else {
            return Err(Error::BadAttribute);
        };
        if p + 4 > used {
            return Err(Error::BadAttribute);
        }
        if t == ATTR_END {
            break;
        }
        let len = u32le(buf, p + 4).ok_or(Error::BadAttribute)? as usize;
        if len < 16 || !len.is_multiple_of(8) || p + len > used {
            return Err(Error::BadAttribute);
        }
        parse_attribute(buf, p, len, t, keep_runs, out)?;
        p += len;
        if p + 8 > used {
            // 沒有結束標記但剛好用完
            break;
        }
    }
    Ok(())
}

fn parse_attribute(
    buf: &[u8],
    p: usize,
    len: usize,
    t: u32,
    keep_runs: bool,
    out: &mut Record,
) -> Result<(), Error> {
    let a = &buf[p..p + len];
    let nonres = *a.get(8).ok_or(Error::BadAttribute)? != 0;
    let name_len = usize::from(*a.get(9).ok_or(Error::BadAttribute)?);
    let name_off = usize::from(u16le(a, 10).ok_or(Error::BadAttribute)?);
    let name: &[u8] = if name_len == 0 {
        &[]
    } else {
        a.get(name_off..name_off + 2 * name_len)
            .ok_or(Error::BadAttribute)?
    };
    match t {
        ATTR_STANDARD_INFORMATION if !nonres => {
            let v = resident_value(a)?;
            if let (Some(m), Some(at)) = (u64le(v.1, 8), u32le(v.1, 32)) {
                out.has_si = true;
                out.mtime = m;
                out.si_attrs = at;
            }
        }
        ATTR_ATTRIBUTE_LIST => {
            out.has_attr_list = true;
            if !nonres {
                let (voff, v) = resident_value(a)?;
                out.attr_list = Some((p + voff, v.len()));
            }
        }
        ATTR_FILE_NAME if !nonres => {
            let (voff, v) = resident_value(a)?;
            let nl = usize::from(*v.get(64).ok_or(Error::BadAttribute)?);
            let ns = *v.get(65).ok_or(Error::BadAttribute)?;
            if v.get(66..66 + 2 * nl).is_none() {
                return Err(Error::BadAttribute);
            }
            let parent_ref = u64le(v, 0).ok_or(Error::BadAttribute)?;
            out.names.push(NameRef {
                parent: parent_ref & REF_MASK,
                parent_seq: (parent_ref >> 48) as u16,
                namespace: ns,
                off: (p + voff + 66) as u32,
                units: nl as u16,
            });
        }
        ATTR_REPARSE_POINT => {
            out.has_reparse = true;
            if !nonres {
                let (_, v) = resident_value(a)?;
                (out.reparse_tag, out.reparse_volume) = reparse_info(v);
            } else if a.len() >= 64 && u64le(a, 16) == Some(0) {
                // 記錄塞不下時屬性會變成非常駐；標籤在第一段叢集裡，由引擎稍後讀取
                let runs_off = usize::from(u16le(a, 32).ok_or(Error::BadAttribute)?);
                let runs = a.get(runs_off..).ok_or(Error::BadAttribute)?;
                let mut first = None;
                parse_runs(runs, 0, |r| {
                    if first.is_none() && r.vcn == 0 {
                        first = r.lcn;
                    }
                })?;
                out.reparse_lcn = first.unwrap_or(0);
            }
        }
        ATTR_DATA => {
            let unnamed = name_len == 0;
            let wof = !unnamed && name_equals(name, WOF_STREAM);
            if !unnamed && !wof {
                return Ok(());
            }
            if unnamed {
                out.has_data = true;
            }
            if !nonres {
                if unnamed {
                    out.data_resident = true;
                    out.data_first = true;
                    let (_, v) = resident_value(a)?;
                    out.data_logical = v.len() as u64;
                }
                return Ok(());
            }
            if a.len() < 64 {
                return Err(Error::BadAttribute);
            }
            let start_vcn = u64le(a, 16).ok_or(Error::BadAttribute)?;
            let runs_off = usize::from(u16le(a, 32).ok_or(Error::BadAttribute)?);
            let real = u64le(a, 48).ok_or(Error::BadAttribute)?;
            let runs = a.get(runs_off..).ok_or(Error::BadAttribute)?;
            let mut clusters = 0u64;
            parse_runs(runs, start_vcn, |r| {
                if r.lcn.is_some() {
                    clusters = clusters.saturating_add(r.len);
                }
                if keep_runs && unnamed {
                    out.data_runs.push(r);
                }
            })?;
            if unnamed {
                out.data_clusters = out.data_clusters.saturating_add(clusters);
                if start_vcn == 0 {
                    out.data_first = true;
                    out.data_logical = real;
                }
            } else {
                out.wof_clusters = out.wof_clusters.saturating_add(clusters);
            }
        }
        _ => {}
    }
    Ok(())
}

/// 重解析資料開頭：回傳 (標籤, 是否為指向磁碟區的掛載點)。
pub fn reparse_info(v: &[u8]) -> (u32, bool) {
    let tag = u32le(v, 0).unwrap_or(0);
    let mut volume = false;
    if tag == TAG_MOUNT_POINT {
        // 替代名稱位於 PathBuffer（標頭 8 位元組＋4 個 u16）
        if let (Some(so), Some(sl)) = (u16le(v, 8), u16le(v, 10)) {
            let start = 16 + usize::from(so);
            if let Some(sub) = v.get(start..start + usize::from(sl)) {
                volume = name_starts_with(sub, VOLUME_PREFIX);
            }
        }
    }
    (tag, volume)
}

/// 常駐屬性的值：回傳 (值在屬性內的位移, 值)。
fn resident_value(a: &[u8]) -> Result<(usize, &[u8]), Error> {
    let vlen = u32le(a, 16).ok_or(Error::BadAttribute)? as usize;
    let voff = usize::from(u16le(a, 20).ok_or(Error::BadAttribute)?);
    let end = voff.checked_add(vlen).ok_or(Error::BadAttribute)?;
    let v = a.get(voff..end).ok_or(Error::BadAttribute)?;
    Ok((voff, v))
}

/// 屬性清單的一筆。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListEntry {
    pub attr_type: u32,
    pub start_vcn: u64,
    pub record: u64,
}

/// 解析（常駐）屬性清單；遇到格式錯誤時回傳已解析的部分。
pub fn parse_attr_list(v: &[u8]) -> Vec<ListEntry> {
    let mut out = Vec::new();
    let mut p = 0usize;
    while p + 0x1A <= v.len() {
        let rec_len = usize::from(u16le(v, p + 4).unwrap_or(0));
        if rec_len < 0x1A {
            break;
        }
        let (Some(t), Some(vcn), Some(r)) = (u32le(v, p), u64le(v, p + 8), u64le(v, p + 0x10))
        else {
            break;
        };
        out.push(ListEntry {
            attr_type: t,
            start_vcn: vcn,
            record: r & REF_MASK,
        });
        p += rec_len;
    }
    out
}

pub const ATTR_TYPE_DATA: u32 = ATTR_DATA;

/// 把 `$MFT` 內的位元組位移換成磁碟區上的位元組位移，並回傳該段剩餘的連續位元組數。
pub fn map_offset(runs: &[Run], cluster_bytes: u64, off: u64) -> Option<(u64, u64)> {
    let vcn = off / cluster_bytes;
    for r in runs {
        if vcn >= r.vcn && vcn - r.vcn < r.len {
            let lcn = r.lcn?;
            let within = off - r.vcn * cluster_bytes;
            let remain = r.len * cluster_bytes - within;
            return Some((lcn * cluster_bytes + within, remain));
        }
    }
    None
}

#[cfg(test)]
pub mod testutil {
    //! 依公開格式文件自行構造的測試記錄（不取自任何其他實作的測試資料）。

    pub const REC: usize = 1024;

    pub fn le16(v: &mut Vec<u8>, x: u16) {
        v.extend_from_slice(&x.to_le_bytes());
    }
    pub fn le32(v: &mut Vec<u8>, x: u32) {
        v.extend_from_slice(&x.to_le_bytes());
    }
    pub fn le64(v: &mut Vec<u8>, x: u64) {
        v.extend_from_slice(&x.to_le_bytes());
    }

    fn pad8(v: &mut Vec<u8>) {
        while !v.len().is_multiple_of(8) {
            v.push(0);
        }
    }

    /// 常駐屬性。
    pub fn resident(t: u32, name: &str, value: &[u8]) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut a = Vec::new();
        le32(&mut a, t);
        le32(&mut a, 0); // 長度稍後填
        a.push(0); // 常駐
        a.push(units.len() as u8);
        le16(&mut a, 0x18); // 名稱位移
        le16(&mut a, 0); // 旗標
        le16(&mut a, 0); // 屬性 ID
        le32(&mut a, value.len() as u32);
        let name_bytes = units.len() * 2;
        let value_off = (0x18 + name_bytes).div_ceil(8) * 8;
        le16(&mut a, value_off as u16);
        le16(&mut a, 0);
        while a.len() < 0x18 {
            a.push(0);
        }
        for u in &units {
            le16(&mut a, *u);
        }
        while a.len() < value_off {
            a.push(0);
        }
        a.extend_from_slice(value);
        pad8(&mut a);
        let n = a.len() as u32;
        a[4..8].copy_from_slice(&n.to_le_bytes());
        a
    }

    /// 非常駐屬性。
    pub fn nonresident(t: u32, name: &str, start_vcn: u64, real: u64, runs: &[u8]) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut a = Vec::new();
        le32(&mut a, t);
        le32(&mut a, 0);
        a.push(1);
        a.push(units.len() as u8);
        le16(&mut a, 0x40);
        le16(&mut a, 0);
        le16(&mut a, 0);
        le64(&mut a, start_vcn);
        le64(&mut a, 0); // 最後 VCN（測試不使用）
        let runs_off = (0x40 + units.len() * 2).div_ceil(8) * 8;
        le16(&mut a, runs_off as u16);
        le16(&mut a, 0);
        le32(&mut a, 0);
        le64(&mut a, real); // 配置大小（測試不使用）
        le64(&mut a, real);
        le64(&mut a, real);
        while a.len() < 0x40 {
            a.push(0);
        }
        for u in &units {
            le16(&mut a, *u);
        }
        while a.len() < runs_off {
            a.push(0);
        }
        a.extend_from_slice(runs);
        pad8(&mut a);
        let n = a.len() as u32;
        a[4..8].copy_from_slice(&n.to_le_bytes());
        a
    }

    pub fn si_value(mtime: u64, attrs: u32) -> Vec<u8> {
        let mut v = vec![0u8; 72];
        v[8..16].copy_from_slice(&mtime.to_le_bytes());
        v[32..36].copy_from_slice(&attrs.to_le_bytes());
        v
    }

    pub fn fn_value(parent: u64, parent_seq: u16, ns: u8, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut v = vec![0u8; 66];
        let pref = parent | (u64::from(parent_seq) << 48);
        v[0..8].copy_from_slice(&pref.to_le_bytes());
        v[64] = units.len() as u8;
        v[65] = ns;
        for u in units {
            v.extend_from_slice(&u.to_le_bytes());
        }
        v
    }

    /// 執行串列：每段 (長度, Some(絕對 LCN) | None=稀疏)。
    pub fn runs(list: &[(u64, Option<u64>)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut prev = 0i64;
        for &(len, lcn) in list {
            let lb = len.to_le_bytes();
            let ln = lb.iter().rposition(|&x| x != 0).map_or(1, |i| i + 1);
            match lcn {
                None => {
                    out.push(ln as u8);
                    out.extend_from_slice(&lb[..ln]);
                }
                Some(l) => {
                    let delta = l as i64 - prev;
                    prev = l as i64;
                    let db = delta.to_le_bytes();
                    // 用 8 位元組保持簡單且符號正確
                    out.push((8 << 4) | ln as u8);
                    out.extend_from_slice(&lb[..ln]);
                    out.extend_from_slice(&db);
                }
            }
        }
        out.push(0);
        out
    }

    /// 組出整筆記錄（含更新序列陣列）。
    pub fn record(flags: u16, seq: u16, links: u16, base: u64, attrs: &[Vec<u8>]) -> Vec<u8> {
        let mut r = vec![0u8; REC];
        r[0..4].copy_from_slice(b"FILE");
        r[4..6].copy_from_slice(&0x30u16.to_le_bytes());
        r[6..8].copy_from_slice(&((REC / 512 + 1) as u16).to_le_bytes());
        r[0x10..0x12].copy_from_slice(&seq.to_le_bytes());
        r[0x12..0x14].copy_from_slice(&links.to_le_bytes());
        r[0x14..0x16].copy_from_slice(&0x38u16.to_le_bytes());
        r[0x16..0x18].copy_from_slice(&flags.to_le_bytes());
        r[0x1C..0x20].copy_from_slice(&(REC as u32).to_le_bytes());
        r[0x20..0x28].copy_from_slice(&base.to_le_bytes());
        let mut p = 0x38;
        for a in attrs {
            r[p..p + a.len()].copy_from_slice(a);
            p += a.len();
        }
        r[p..p + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        p += 8;
        r[0x18..0x1C].copy_from_slice(&(p as u32).to_le_bytes());
        // 更新序列陣列：序號 0x0102，原始尾端位元組存入陣列
        let usn = [0x01u8, 0x02];
        r[0x30..0x32].copy_from_slice(&usn);
        for i in 1..=REC / 512 {
            let end = i * 512;
            let (a, b) = (r[end - 2], r[end - 1]);
            r[0x30 + 2 * i] = a;
            r[0x30 + 2 * i + 1] = b;
            r[end - 2] = usn[0];
            r[end - 1] = usn[1];
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;

    fn parse(mut r: Vec<u8>) -> (Result<(), Error>, Record, Vec<u8>) {
        let mut out = Record::default();
        let e = parse_record(&mut r, true, &mut out);
        (e, out, r)
    }

    fn file_record() -> Vec<u8> {
        record(
            1,
            7,
            2,
            0,
            &[
                resident(0x10, "", &si_value(1234, 0x20)),
                resident(0x30, "", &fn_value(5, 5, 1, "hello.txt")),
                resident(0x30, "", &fn_value(40, 3, 2, "HELLO~1.TXT")),
                nonresident(0x80, "", 0, 10_000, &runs(&[(3, Some(100)), (2, None)])),
            ],
        )
    }

    #[test]
    fn parses_names_data_and_si() {
        let (e, rec, buf) = parse(file_record());
        e.unwrap();
        assert!(rec.in_use && !rec.is_dir);
        assert_eq!((rec.seq, rec.links, rec.base), (7, 2, 0));
        assert_eq!((rec.mtime, rec.si_attrs), (1234, 0x20));
        assert_eq!(rec.names.len(), 2);
        let n = rec.names[0];
        assert_eq!((n.parent, n.parent_seq, n.namespace), (5, 5, 1));
        let s: String = char::decode_utf16(name_units(&buf, n))
            .map(|c| c.unwrap())
            .collect();
        assert_eq!(s, "hello.txt");
        assert_eq!(rec.names[1].namespace, NS_DOS);
        assert!(rec.has_data && !rec.data_resident && rec.data_first);
        assert_eq!(rec.data_logical, 10_000);
        // 稀疏的 2 個叢集不計入配置
        assert_eq!(rec.data_clusters, 3);
        assert_eq!(
            rec.data_runs,
            [
                Run {
                    vcn: 0,
                    len: 3,
                    lcn: Some(100)
                },
                Run {
                    vcn: 3,
                    len: 2,
                    lcn: None
                }
            ]
        );
    }

    #[test]
    fn resident_data_has_no_clusters() {
        let r = record(
            1,
            1,
            1,
            0,
            &[
                resident(0x30, "", &fn_value(5, 5, 3, "tiny")),
                resident(0x80, "", b"hello"),
            ],
        );
        let (e, rec, _) = parse(r);
        e.unwrap();
        assert!(rec.data_resident);
        assert_eq!((rec.data_logical, rec.data_clusters), (5, 0));
    }

    #[test]
    fn named_streams_are_ignored_except_wof() {
        let r = record(
            1,
            1,
            1,
            0,
            &[
                resident(0x30, "", &fn_value(5, 5, 3, "f")),
                nonresident(0x80, "", 0, 0, &runs(&[])),
                nonresident(0x80, "ads", 0, 50, &runs(&[(9, Some(500))])),
                nonresident(0x80, "WofCompressedData", 0, 70, &runs(&[(4, Some(900))])),
            ],
        );
        let (e, rec, _) = parse(r);
        e.unwrap();
        assert_eq!(rec.data_clusters, 0);
        assert_eq!(rec.wof_clusters, 4);
    }

    #[test]
    fn extension_record_reports_its_base() {
        let r = record(
            1,
            1,
            0,
            0x0001_0000_0000_0123,
            &[nonresident(0x80, "", 16, 0, &runs(&[(7, Some(70))]))],
        );
        let (e, rec, _) = parse(r);
        e.unwrap();
        assert_eq!(rec.base, 0x123);
        assert!(!rec.data_first);
        assert_eq!(rec.data_clusters, 7);
    }

    #[test]
    fn directory_and_reparse() {
        let mut v = vec![0u8; 8];
        v[0..4].copy_from_slice(&0xA000_0003u32.to_le_bytes());
        // 掛載點資料：替代名稱 \??\Volume{
        let sub: Vec<u8> = r"\??\Volume{abc}\"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let mut body = Vec::new();
        le16(&mut body, 0);
        le16(&mut body, sub.len() as u16);
        le16(&mut body, sub.len() as u16 + 2);
        le16(&mut body, 0);
        body.extend_from_slice(&sub);
        v.extend_from_slice(&body);
        let r = record(
            3,
            4,
            1,
            0,
            &[
                resident(0x30, "", &fn_value(5, 5, 1, "mnt")),
                resident(0xC0, "", &v),
            ],
        );
        let (e, rec, _) = parse(r);
        e.unwrap();
        assert!(rec.is_dir && rec.has_reparse);
        assert_eq!(rec.reparse_tag, 0xA000_0003);
        assert!(rec.reparse_volume);
    }

    #[test]
    fn junction_is_not_volume() {
        let mut v = vec![0u8; 8];
        v[0..4].copy_from_slice(&0xA000_0003u32.to_le_bytes());
        let sub: Vec<u8> = r"\??\C:\target"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let mut body = Vec::new();
        le16(&mut body, 0);
        le16(&mut body, sub.len() as u16);
        le16(&mut body, sub.len() as u16 + 2);
        le16(&mut body, 0);
        body.extend_from_slice(&sub);
        v.extend_from_slice(&body);
        let r = record(3, 4, 1, 0, &[resident(0xC0, "", &v)]);
        let (e, rec, _) = parse(r);
        e.unwrap();
        assert!(!rec.reparse_volume);
    }

    #[test]
    fn unused_record_is_ok_but_not_in_use() {
        let r = record(0, 1, 0, 0, &[]);
        let (e, rec, _) = parse(r);
        e.unwrap();
        assert!(!rec.in_use);
    }

    // 損壞的記錄不 panic、不越界
    #[test]
    fn ec14_bad_fixup_is_rejected() {
        let mut r = file_record();
        r[510] ^= 0xFF;
        let (e, _, _) = parse(r);
        assert_eq!(e, Err(Error::BadFixup));
    }

    #[test]
    fn ec14_bad_magic_and_baad() {
        let mut r = file_record();
        r[0..4].copy_from_slice(b"BAAD");
        assert_eq!(parse(r.clone()).0, Err(Error::BadFixup));
        r[0..4].copy_from_slice(b"XXXX");
        assert_eq!(parse(r).0, Err(Error::BadMagic));
        assert_eq!(parse(vec![0u8; 1024]).0, Err(Error::BadMagic));
        assert_eq!(parse(vec![0u8; 100]).0, Err(Error::BadMagic));
    }

    #[test]
    fn ec14_attribute_length_overflow() {
        let mut r = file_record();
        // 把第一個屬性的長度改成超過已用大小；需要先還原序號才能通過 fixup，
        // 所以直接改在區段內（位移 0x3C）
        r[0x38 + 4..0x38 + 8].copy_from_slice(&0x7FFF_FFF8u32.to_le_bytes());
        assert_eq!(parse(r).0, Err(Error::BadAttribute));
    }

    #[test]
    fn ec14_zero_length_attribute_does_not_loop() {
        let mut r = file_record();
        r[0x38 + 4..0x38 + 8].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(parse(r).0, Err(Error::BadAttribute));
    }

    #[test]
    fn ec14_truncated_runs_are_errors() {
        let mut bad = runs(&[(3, Some(100))]);
        bad.truncate(3);
        let r = record(1, 1, 1, 0, &[nonresident(0x80, "", 0, 1, &bad)]);
        assert_eq!(parse(r).0, Err(Error::BadRuns));
    }

    #[test]
    fn runs_decode_negative_offsets() {
        // 長度 1，偏移 -1（0xFF）：先到 LCN 10，再退到 9
        let list = [0x11, 1, 10, 0x11, 1, 0xFF, 0];
        let mut got = Vec::new();
        parse_runs(&list, 0, |r| got.push(r)).unwrap();
        assert_eq!(got[0].lcn, Some(10));
        assert_eq!(got[1].lcn, Some(9));
        assert_eq!(got[1].vcn, 1);
        // 退到負的 LCN 是錯誤
        assert!(parse_runs(&[0x11, 1, 0xFF, 0], 0, |_| {}).is_err());
    }

    #[test]
    fn runs_reject_bad_headers() {
        assert!(parse_runs(&[0x09, 1], 0, |_| {}).is_err()); // 長度欄位 9 位元組
        assert!(parse_runs(&[0x01, 0, 0], 0, |_| {}).is_err()); // 長度 0
        assert!(parse_runs(&[0x90, 1], 0, |_| {}).is_err()); // 偏移欄位 9 位元組
        parse_runs(&[], 0, |_| panic!("no runs")).unwrap();
    }

    #[test]
    fn attribute_list_entries() {
        let mut v = Vec::new();
        for (t, vcn, rec) in [(0x80u32, 0u64, 5u64), (0x80, 100, 0x0003_0000_0000_0777)] {
            let mut e = Vec::new();
            le32(&mut e, t);
            le16(&mut e, 0x20);
            e.extend_from_slice(&[0, 0x1A]);
            le64(&mut e, vcn);
            le64(&mut e, rec);
            le16(&mut e, 0);
            e.resize(0x20, 0);
            v.extend_from_slice(&e);
        }
        let list = parse_attr_list(&v);
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].record, 0x777);
        assert_eq!(list[1].start_vcn, 100);
        // 長度為 0 的項目不會造成無限迴圈
        let mut z = v.clone();
        z[4] = 0;
        z[5] = 0;
        assert!(parse_attr_list(&z).is_empty());
    }

    #[test]
    fn boot_sector() {
        let mut b = vec![0u8; 512];
        b[3..11].copy_from_slice(b"NTFS    ");
        b[0x0B..0x0D].copy_from_slice(&512u16.to_le_bytes());
        b[0x0D] = 8;
        b[0x28..0x30].copy_from_slice(&8_000_000u64.to_le_bytes());
        b[0x30..0x38].copy_from_slice(&786_432u64.to_le_bytes());
        b[0x40] = 0xF6; // -10 → 1024
        b[0x48..0x50].copy_from_slice(&0xAABB_CCDDu64.to_le_bytes());
        let boot = parse_boot(&b).unwrap();
        assert_eq!(boot.cluster_bytes, 4096);
        assert_eq!(boot.record_bytes, 1024);
        assert_eq!(boot.mft_lcn, 786_432);
        assert_eq!(boot.total_clusters, 1_000_000);
        assert_eq!(boot.serial, 0xAABB_CCDD);
        b[3] = b'X';
        assert_eq!(parse_boot(&b), Err(Error::BadBoot));
    }

    #[test]
    fn boot_rejects_nonsense() {
        let mut b = vec![0u8; 512];
        b[3..11].copy_from_slice(b"NTFS    ");
        b[0x0B..0x0D].copy_from_slice(&512u16.to_le_bytes());
        b[0x0D] = 0;
        assert_eq!(parse_boot(&b), Err(Error::BadBoot));
        b[0x0D] = 8;
        b[0x28..0x30].copy_from_slice(&100u64.to_le_bytes());
        b[0x30..0x38].copy_from_slice(&999_999u64.to_le_bytes());
        b[0x40] = 0xF6;
        assert_eq!(parse_boot(&b), Err(Error::BadBoot));
        assert_eq!(parse_boot(&b[..100]), Err(Error::BadBoot));
    }

    #[test]
    fn map_offset_follows_runs() {
        let runs = [
            Run {
                vcn: 0,
                len: 2,
                lcn: Some(10),
            },
            Run {
                vcn: 2,
                len: 3,
                lcn: None,
            },
            Run {
                vcn: 5,
                len: 1,
                lcn: Some(50),
            },
        ];
        assert_eq!(
            map_offset(&runs, 4096, 4096 + 100),
            Some((10 * 4096 + 4096 + 100, 4096 - 100))
        );
        assert_eq!(map_offset(&runs, 4096, 2 * 4096), None); // 稀疏
        assert_eq!(map_offset(&runs, 4096, 5 * 4096), Some((50 * 4096, 4096)));
        assert_eq!(map_offset(&runs, 4096, 6 * 4096), None);
    }

    /// 簡單的確定性亂數（xorshift），讓變異測試可重現。
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    fn mutate_and_parse(iterations: u64, seed: u64) {
        let seeds = [
            file_record(),
            record(
                3,
                1,
                1,
                0,
                &[
                    resident(0x10, "", &si_value(1, 0x10)),
                    resident(0x30, "", &fn_value(5, 5, 1, "d")),
                    resident(0x20, "", &[0u8; 64]),
                ],
            ),
        ];
        let mut rng = Rng(seed | 1);
        let mut out = Record::default();
        for _ in 0..iterations {
            let mut r = seeds[(rng.next() % seeds.len() as u64) as usize].clone();
            for _ in 0..(1 + rng.next() % 8) {
                let pos = (rng.next() % r.len() as u64) as usize;
                match rng.next() % 4 {
                    0 => r[pos] = rng.next() as u8,
                    1 => r[pos] ^= 1 << (rng.next() % 8),
                    2 => {
                        let end = (pos + 8).min(r.len());
                        for b in &mut r[pos..end] {
                            *b = 0xFF;
                        }
                    }
                    _ => {
                        let end = (pos + 8).min(r.len());
                        for b in &mut r[pos..end] {
                            *b = 0;
                        }
                    }
                }
            }
            // 有時讓 fixup 通過，才能走到屬性解析
            if rng.next().is_multiple_of(2) {
                r[510] = r[0x30];
                r[511] = r[0x31];
            }
            let cut = if rng.next().is_multiple_of(8) {
                512 * (1 + (rng.next() % 2) as usize)
            } else {
                r.len()
            };
            let _ = parse_record(&mut r[..cut], true, &mut out);
            // 名稱位置必須落在緩衝區內
            for n in out.names.clone() {
                let _ = name_units(&r, n).count();
            }
        }
    }

    #[test]
    fn mutation_never_panics() {
        mutate_and_parse(100_000, 0x1234_5678_9ABC_DEF1);
    }

    /// 長時間模糊測試：`cargo test --release -- --ignored fuzz_long`，
    /// 以環境變數 VOLTDIR_FUZZ_ITERS 調整次數。
    #[test]
    #[ignore]
    fn fuzz_long() {
        let iters = std::env::var("VOLTDIR_FUZZ_ITERS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(50_000_000u64);
        for seed in 1..=8u64 {
            mutate_and_parse(iters / 8, seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        }
    }
}
