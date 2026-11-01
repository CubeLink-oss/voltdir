//! 記憶體中的掃描結果與屬性分類。

pub const ATTR_READONLY: u32 = 0x1;
pub const ATTR_HIDDEN: u32 = 0x2;
pub const ATTR_SYSTEM: u32 = 0x4;
pub const ATTR_DIRECTORY: u32 = 0x10;
pub const ATTR_ARCHIVE: u32 = 0x20;
pub const ATTR_TEMPORARY: u32 = 0x100;
pub const ATTR_SPARSE: u32 = 0x200;
pub const ATTR_REPARSE: u32 = 0x400;
pub const ATTR_COMPRESSED: u32 = 0x800;
pub const ATTR_OFFLINE: u32 = 0x1000;
pub const ATTR_NOT_INDEXED: u32 = 0x2000;
pub const ATTR_ENCRYPTED: u32 = 0x4000;
pub const ATTR_RECALL_ON_OPEN: u32 = 0x40000;
pub const ATTR_PINNED: u32 = 0x80000;
pub const ATTR_UNPINNED: u32 = 0x100000;
pub const ATTR_RECALL_ON_DATA_ACCESS: u32 = 0x400000;

pub const TAG_MOUNT_POINT: u32 = 0xA000_0003;
pub const TAG_SYMLINK: u32 = 0xA000_000C;

/// 屬性名稱（順序固定）。
const ATTR_NAMES: [(u32, &str); 14] = [
    (ATTR_READONLY, "readonly"),
    (ATTR_HIDDEN, "hidden"),
    (ATTR_SYSTEM, "system"),
    (ATTR_ARCHIVE, "archive"),
    (ATTR_COMPRESSED, "compressed"),
    (ATTR_ENCRYPTED, "encrypted"),
    (ATTR_SPARSE, "sparse"),
    (ATTR_OFFLINE, "offline"),
    (ATTR_TEMPORARY, "temporary"),
    (ATTR_NOT_INDEXED, "not_content_indexed"),
    (ATTR_RECALL_ON_OPEN, "recall_on_open"),
    (ATTR_RECALL_ON_DATA_ACCESS, "recall_on_data_access"),
    (ATTR_PINNED, "pinned"),
    (ATTR_UNPINNED, "unpinned"),
];

pub fn attr_names(attrs: u32) -> impl Iterator<Item = &'static str> {
    ATTR_NAMES
        .iter()
        .filter(move |(bit, _)| attrs & bit != 0)
        .map(|&(_, n)| n)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reparse {
    None,
    Symlink,
    Junction,
    Mount,
    Other,
}

impl Reparse {
    pub fn as_str(self) -> &'static str {
        match self {
            Reparse::None => "none",
            Reparse::Symlink => "symlink",
            Reparse::Junction => "junction",
            Reparse::Mount => "mount",
            Reparse::Other => "other",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScanState {
    /// 尚未處理（只在掃描進行中出現）
    Pending,
    Complete,
    Partial,
    Unreadable,
    Missing,
    NotFollowed,
    Excluded,
    Cancelled,
}

impl ScanState {
    pub fn as_str(self) -> &'static str {
        match self {
            ScanState::Pending | ScanState::Complete => "complete",
            ScanState::Partial => "partial",
            ScanState::Unreadable => "unreadable",
            ScanState::Missing => "missing",
            ScanState::NotFollowed => "not_followed",
            ScanState::Excluded => "excluded",
            ScanState::Cancelled => "cancelled",
        }
    }

    /// 列入 `coverage` 的「沒讀到」：讀取失敗，加上被取消而未處理者。
    pub fn is_uncovered(self) -> bool {
        self.is_incomplete() || self == ScanState::Cancelled
    }

    /// 非 complete、excluded、not_followed 者使結束代碼為 2。
    pub fn is_incomplete(self) -> bool {
        matches!(
            self,
            ScanState::Partial | ScanState::Unreadable | ScanState::Missing
        )
    }
}

#[derive(Clone, Copy, Default, Debug)]
pub struct FileEntry {
    pub name_off: u32,
    pub name_len: u16,
    pub reserved: bool,
    /// 硬連結次要名稱（由後處理填入）
    pub secondary: bool,
    pub reparse_other: bool,
    pub attrs: u32,
    pub logical: u64,
    pub physical: u64,
    pub mtime: u64,
    pub id: u128,
    /// 後處理查詢到的硬連結數；0 表示未知
    pub links: u32,
}

#[derive(Clone, Copy, Default, Debug)]
pub struct Totals {
    pub files: u64,
    pub dirs: u64,
    pub logical: u64,
    pub physical: u64,
}

#[derive(Debug)]
pub struct DirNode {
    pub parent: u32,
    pub root: u32,
    pub name: Box<[u16]>,
    pub attrs: u32,
    pub reparse: Reparse,
    pub reserved: bool,
    pub mtime: u64,
    pub id: u128,
    /// 所在磁碟區序號（跟隨連結後可能與父目錄不同）
    pub serial: u32,
    pub state: ScanState,
    pub error: Option<u32>,
    /// 此目錄所有檔案名稱的串接
    pub names: Vec<u16>,
    /// 已依名稱排序
    pub files: Vec<FileEntry>,
    /// 子目錄節點編號，已依名稱排序
    pub subdirs: Vec<u32>,
    pub direct: Totals,
    pub rollup: Totals,
    /// 因 `--min-size` 而不輸出的直接子檔案合計（實際大小以「對 direct 的貢獻」計，次要名稱為 0）
    pub omitted: Totals,
    pub latest_mtime: u64,
}

impl DirNode {
    pub fn file_name(&self, f: &FileEntry) -> &[u16] {
        let s = f.name_off as usize;
        &self.names[s..s + f.name_len as usize]
    }
}

/// 一個掃描根。
pub struct RootInfo {
    /// 顯示用的 Win32 路徑（不含 `\\?\`）
    pub display: Vec<u16>,
    pub node: u32,
}

pub struct ScanData {
    pub dirs: Vec<DirNode>,
    pub roots: Vec<RootInfo>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribute_names_follow_spec_order() {
        let all = ATTR_READONLY | ATTR_HIDDEN | ATTR_SYSTEM | ATTR_ARCHIVE | ATTR_UNPINNED;
        let v: Vec<_> = attr_names(all).collect();
        assert_eq!(v, ["readonly", "hidden", "system", "archive", "unpinned"]);
        // 未列入名稱的位元（例如 DIRECTORY、REPARSE）只出現在 attrs_raw
        assert_eq!(attr_names(ATTR_DIRECTORY | ATTR_REPARSE).count(), 0);
    }

    #[test]
    fn only_read_failures_are_incomplete() {
        assert!(ScanState::Unreadable.is_incomplete());
        assert!(ScanState::Partial.is_incomplete());
        assert!(ScanState::Missing.is_incomplete());
        assert!(!ScanState::Complete.is_incomplete());
        assert!(!ScanState::NotFollowed.is_incomplete());
        assert!(!ScanState::Excluded.is_incomplete());
        assert!(!ScanState::Cancelled.is_incomplete());
        assert!(ScanState::Cancelled.is_uncovered());
        assert_eq!(ScanState::Excluded.as_str(), "excluded");
        assert_eq!(ScanState::Cancelled.as_str(), "cancelled");
    }
}
