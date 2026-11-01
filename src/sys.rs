//! 全部 Windows 系統呼叫都在這個模組（專案中唯一允許 `unsafe` 的地方）。
//!
//! 唯讀保證：所有對被掃描對象的開啟，存取權限只有 `FILE_LIST_DIRECTORY`（列舉目錄）
//! 或 `FILE_READ_ATTRIBUTES`（讀屬性），絕不要求寫入或讀取資料內容的權限。

use std::ffi::c_void;

type Handle = *mut c_void;

const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;

const FILE_LIST_DIRECTORY: u32 = 0x0000_0001;
const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
const FILE_SHARE_ALL: u32 = 0x0000_0007; // READ | WRITE | DELETE：不阻擋其他程式
const OPEN_EXISTING: u32 = 3;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000; // 開啟目錄所需
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000; // 不跟隨連結、不觸發雲端下載

const FILE_STANDARD_INFO: i32 = 1;
const FILE_ID_EXTD_DIRECTORY_INFO: i32 = 19;
const FILE_ID_EXTD_DIRECTORY_RESTART_INFO: i32 = 20;

pub const ERROR_NO_MORE_FILES: u32 = 18;
const FSCTL_GET_REPARSE_POINT: u32 = 0x0009_00A8;
const TOKEN_QUERY: u32 = 0x0008;
const TOKEN_ELEVATION_CLASS: i32 = 20;

pub const DRIVE_REMOTE: u32 = 4;

#[repr(C)]
struct ByHandleInfo {
    attrs: u32,
    creation: [u32; 2],
    access: [u32; 2],
    write: [u32; 2],
    serial: u32,
    size_high: u32,
    size_low: u32,
    links: u32,
    index_high: u32,
    index_low: u32,
}

/// `ReadFile` 的 OVERLAPPED 參數；同步句柄上只用來指定讀取位移。
#[repr(C)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    event: Handle,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        security: *mut c_void,
        disposition: u32,
        flags: u32,
        template: Handle,
    ) -> Handle;
    fn CloseHandle(h: Handle) -> i32;
    fn ReadFile(
        h: Handle,
        buf: *mut c_void,
        len: u32,
        read: *mut u32,
        overlapped: *mut Overlapped,
    ) -> i32;
    fn GetLastError() -> u32;
    fn GetFileInformationByHandleEx(h: Handle, class: i32, buf: *mut c_void, size: u32) -> i32;
    fn GetFileInformationByHandle(h: Handle, info: *mut ByHandleInfo) -> i32;
    fn GetCompressedFileSizeW(path: *const u16, high: *mut u32) -> u32;
    fn DeviceIoControl(
        h: Handle,
        code: u32,
        inbuf: *mut c_void,
        insize: u32,
        outbuf: *mut c_void,
        outsize: u32,
        returned: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn FormatMessageW(
        flags: u32,
        source: *const c_void,
        id: u32,
        lang: u32,
        buf: *mut u16,
        size: u32,
        args: *mut c_void,
    ) -> u32;
    fn GetCurrentProcess() -> Handle;
    fn GetLogicalDriveStringsW(len: u32, buf: *mut u16) -> u32;
    fn GetDriveTypeW(root: *const u16) -> u32;
    fn GetVolumePathNameW(path: *const u16, out: *mut u16, len: u32) -> i32;
    fn GetVolumeInformationW(
        root: *const u16,
        name: *mut u16,
        name_len: u32,
        serial: *mut u32,
        max_component: *mut u32,
        flags: *mut u32,
        fs_name: *mut u16,
        fs_len: u32,
    ) -> i32;
    fn GetDiskFreeSpaceW(
        root: *const u16,
        sectors_per_cluster: *mut u32,
        bytes_per_sector: *mut u32,
        free_clusters: *mut u32,
        total_clusters: *mut u32,
    ) -> i32;
    fn GetDiskFreeSpaceExW(
        root: *const u16,
        available: *mut u64,
        total: *mut u64,
        free: *mut u64,
    ) -> i32;
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> i32;
    fn GetTokenInformation(
        token: Handle,
        class: i32,
        info: *mut c_void,
        len: u32,
        returned: *mut u32,
    ) -> i32;
}

fn last_error() -> u32 {
    // SAFETY: GetLastError 無參數、無前置條件。
    unsafe { GetLastError() }
}

fn nul_terminated(path: &[u16]) -> Vec<u16> {
    let mut v = Vec::with_capacity(path.len() + 1);
    v.extend_from_slice(path);
    v.push(0);
    v
}

/// 擁有的 Windows 句柄，離開範圍時關閉。
struct OwnedHandle(Handle);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: 句柄由成功的 CreateFileW／OpenProcessToken 取得，只在此關閉一次。
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// `follow` 為真時跟隨重解析點（只用於 `--follow-links`）；否則開啟連結本身。
fn open(path: &[u16], access: u32, follow: bool) -> Result<OwnedHandle, u32> {
    let reparse_flag = if follow {
        0
    } else {
        FILE_FLAG_OPEN_REPARSE_POINT
    };
    let wide = nul_terminated(path);
    // SAFETY: `wide` 以 NUL 結尾且在呼叫期間有效；其餘參數為常數或空指標，符合文件規定。
    // 存取權限只會是 FILE_LIST_DIRECTORY 或 FILE_READ_ATTRIBUTES（唯讀）。
    let h = unsafe {
        CreateFileW(
            wide.as_ptr(),
            access,
            FILE_SHARE_ALL,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | reparse_flag,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        Err(last_error())
    } else {
        Ok(OwnedHandle(h))
    }
}

/// 8 位元組對齊的緩衝區，供目錄列舉結構使用。
pub struct AlignedBuf {
    words: Vec<u64>,
}

impl AlignedBuf {
    pub fn new(bytes: usize) -> Self {
        AlignedBuf {
            words: vec![0; bytes.div_ceil(8)],
        }
    }

    pub fn as_bytes(&self, len: usize) -> &[u8] {
        let total = self.words.len() * 8;
        let len = len.min(total);
        // SAFETY: u64 切片轉為位元組切片：指標有效、對齊要求更低、長度不超過配置大小，
        // 且 u8 沒有無效位元模式。
        unsafe { std::slice::from_raw_parts(self.words.as_ptr().cast::<u8>(), len) }
    }
}

/// 以唯讀（僅列舉）方式開啟的目錄。
pub struct DirHandle(OwnedHandle);

impl DirHandle {
    /// `path` 必須是延伸長度路徑（`\\?\...`）。失敗時回傳 Windows 錯誤碼。
    pub fn open(path: &[u16], follow: bool) -> Result<DirHandle, u32> {
        open(path, FILE_LIST_DIRECTORY, follow).map(DirHandle)
    }

    /// 讀取下一批目錄項目；回傳寫入的位元組數，0 表示已列舉完畢。
    pub fn read_batch(&self, buf: &mut AlignedBuf, restart: bool) -> Result<usize, u32> {
        let class = if restart {
            FILE_ID_EXTD_DIRECTORY_RESTART_INFO
        } else {
            FILE_ID_EXTD_DIRECTORY_INFO
        };
        let size = (buf.words.len() * 8).min(u32::MAX as usize) as u32;
        // SAFETY: 句柄為有效的目錄句柄；緩衝區由 AlignedBuf 持有，指標有效、8 位元組對齊，
        // 且 `size` 不超過其位元組長度。
        let ok = unsafe {
            GetFileInformationByHandleEx(
                (self.0).0,
                class,
                buf.words.as_mut_ptr().cast::<c_void>(),
                size,
            )
        };
        if ok != 0 {
            // API 不回報寫入長度；回傳整個緩衝區，由解析器依 NextEntryOffset 鏈走訪，
            // 遇到 NextEntryOffset == 0 的最後一筆即停止，不會讀到舊資料。
            Ok(buf.words.len() * 8)
        } else {
            let e = last_error();
            if e == ERROR_NO_MORE_FILES {
                Ok(0)
            } else {
                Err(e)
            }
        }
    }
}

const GENERIC_READ: u32 = 0x8000_0000;
const ERROR_HANDLE_EOF: u32 = 38;

/// 以唯讀方式開啟的磁碟區（`\\.\X:`），供 MFT 引擎依位移讀取原始位元組。
///
/// 唯讀保證：存取權限只有 `GENERIC_READ`，沒有任何寫入、控制碼或鎖定磁碟區的呼叫。
pub struct VolumeReader(OwnedHandle);

// SAFETY: Windows 句柄是行程層級的識別碼，可在執行緒間傳遞；`read_at` 每次呼叫都以
// 明確位移讀取，不依賴也不修改任何呼叫端共享的狀態（同步句柄上的並行讀取由系統序列化）。
unsafe impl Send for VolumeReader {}
unsafe impl Sync for VolumeReader {}

impl VolumeReader {
    /// `drive` 是磁碟機根目錄（例如 `C:\`）。未提權時失敗並回傳 `ERROR_ACCESS_DENIED`（5）。
    pub fn open(drive: &[u16]) -> Result<VolumeReader, u32> {
        let letter = *drive.first().ok_or(87u32)?;
        let mut path: Vec<u16> = r"\\.\".encode_utf16().collect();
        path.push(letter);
        path.push(u16::from(b':'));
        let wide = nul_terminated(&path);
        // SAFETY: `wide` 以 NUL 結尾且在呼叫期間有效；存取權限只有 GENERIC_READ（唯讀），
        // 共用模式允許其他程式同時讀寫，其餘參數為常數或空指標。
        let h = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_ALL,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            Err(last_error())
        } else {
            Ok(VolumeReader(OwnedHandle(h)))
        }
    }

    /// 從磁碟區的 `offset` 讀入 `buf`，讀滿或到達尾端為止；回傳實際讀到的位元組數。
    /// 磁碟區讀取要求位移與長度都是磁區大小的倍數（呼叫端負責）。可同時由多個執行緒呼叫。
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, u32> {
        let mut done = 0usize;
        while done < buf.len() {
            let pos = offset + done as u64;
            let want = (buf.len() - done).min(1 << 30) as u32;
            let mut ov = Overlapped {
                internal: 0,
                internal_high: 0,
                offset: pos as u32,
                offset_high: (pos >> 32) as u32,
                event: std::ptr::null_mut(),
            };
            let mut got = 0u32;
            // SAFETY: 句柄是有效的磁碟區句柄；`buf[done..]` 至少有 `want` 位元組可寫入；
            // `ov` 與 `got` 在呼叫期間有效。句柄為同步模式，呼叫在返回前完成，不會在返回後
            // 繼續使用這些指標。
            let ok = unsafe {
                ReadFile(
                    (self.0).0,
                    buf[done..].as_mut_ptr().cast::<c_void>(),
                    want,
                    &mut got,
                    &mut ov,
                )
            };
            if ok == 0 {
                let e = last_error();
                if e == ERROR_HANDLE_EOF {
                    break;
                }
                return Err(e);
            }
            if got == 0 {
                break;
            }
            done += got as usize;
        }
        Ok(done)
    }
}

/// 單一路徑的屬性資訊（不讀取內容）。
#[derive(Clone, Copy, Debug)]
pub struct FileInfo {
    pub attrs: u32,
    pub mtime: u64,
    pub serial: u32,
    pub links: u32,
    pub index: u64,
}

pub fn file_info(path: &[u16]) -> Result<FileInfo, u32> {
    file_info_impl(path, false)
}

/// 跟隨連結，取得連結目標的資訊（用於 `--follow-links` 的迴圈偵測）。
pub fn file_info_follow(path: &[u16]) -> Result<FileInfo, u32> {
    file_info_impl(path, true)
}

fn file_info_impl(path: &[u16], follow: bool) -> Result<FileInfo, u32> {
    let h = open(path, FILE_READ_ATTRIBUTES, follow)?;
    let mut info = std::mem::MaybeUninit::<ByHandleInfo>::zeroed();
    // SAFETY: 句柄有效；`info` 為大小正確的可寫結構（已清零）。
    let ok = unsafe { GetFileInformationByHandle(h.0, info.as_mut_ptr()) };
    if ok == 0 {
        return Err(last_error());
    }
    // SAFETY: 呼叫成功時系統已填入結構；欄位全為整數，任何位元模式皆有效。
    let info = unsafe { info.assume_init() };
    Ok(FileInfo {
        attrs: info.attrs,
        mtime: u64::from(info.write[0]) | (u64::from(info.write[1]) << 32),
        serial: info.serial,
        links: info.links,
        index: u64::from(info.index_low) | (u64::from(info.index_high) << 32),
    })
}

/// 檔案目前實際配置的位元組數（`FileStandardInfo.AllocationSize`，以屬性句柄查詢，不讀取內容）。
/// 對 NTFS 壓縮檔與稀疏檔，這個值是真正配置的叢集數（含叢集進位），與基準一致。
pub fn standard_allocation(path: &[u16]) -> Result<u64, u32> {
    // 跟隨重解析點開啟，讓檔案系統過濾器（如 WOF）以邏輯檔案的身分回答
    let h = open(path, FILE_READ_ATTRIBUTES, true)?;
    // FILE_STANDARD_INFO：AllocationSize(8) EndOfFile(8) NumberOfLinks(4) DeletePending(1) Directory(1)
    let mut buf = [0u64; 3];
    // SAFETY: 句柄有效；緩衝區為 24 位元組、8 位元組對齊，大小與 FILE_STANDARD_INFO 相符。
    let ok = unsafe {
        GetFileInformationByHandleEx(
            h.0,
            FILE_STANDARD_INFO,
            buf.as_mut_ptr().cast::<c_void>(),
            std::mem::size_of_val(&buf) as u32,
        )
    };
    if ok == 0 {
        return Err(last_error());
    }
    Ok(buf[0])
}

/// 查詢檔案實際佔用的磁碟位元組數（`GetCompressedFileSizeW`，不讀取檔案內容）。
/// 目錄列舉對稀疏檔與檔案系統過濾器壓縮（如 WOF）的檔案會回報 0 或不可靠的配置大小，
/// 這類檔案改用這個值。呼叫端必須排除雲端佔位檔，避免觸發下載。
pub fn allocation_size(path: &[u16]) -> Result<u64, u32> {
    let wide = nul_terminated(path);
    let mut high = 0u32;
    // SAFETY: 路徑以 NUL 結尾；`high` 為本地輸出變數。此 API 在內部以讀取屬性的權限開啟檔案。
    let low = unsafe { GetCompressedFileSizeW(wide.as_ptr(), &mut high) };
    if low == u32::MAX {
        let e = last_error();
        if e != 0 {
            return Err(e);
        }
    }
    Ok(u64::from(low) | (u64::from(high) << 32))
}
/// 讀取重解析點的原始資料（`FSCTL_GET_REPARSE_POINT`，唯讀）；回傳緩衝區與有效位元組數。
fn read_reparse(path: &[u16]) -> Option<(AlignedBuf, usize)> {
    let h = open(path, FILE_READ_ATTRIBUTES, false).ok()?;
    let mut buf = AlignedBuf::new(16 * 1024);
    let mut returned = 0u32;
    // SAFETY: 句柄有效；輸出緩衝區為 AlignedBuf，長度正確；不使用輸入緩衝區與 overlapped。
    // FSCTL_GET_REPARSE_POINT 只讀取重解析資料，不修改任何東西。
    let ok = unsafe {
        DeviceIoControl(
            h.0,
            FSCTL_GET_REPARSE_POINT,
            std::ptr::null_mut(),
            0,
            buf.words.as_mut_ptr().cast::<c_void>(),
            (buf.words.len() * 8) as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return None;
    }
    Some((buf, returned as usize))
}

/// 重解析標籤（REPARSE_DATA_BUFFER 的前 4 位元組）。批次列舉偶爾不提供標籤（回報 0）時用來補查。
pub fn reparse_tag(path: &[u16]) -> Option<u32> {
    let (buf, n) = read_reparse(path)?;
    Some(u32::from_le_bytes(
        buf.as_bytes(n).get(0..4)?.try_into().ok()?,
    ))
}

/// 讀取掛載點／接合點重解析資料中的替代名稱（UTF-16）。
pub fn reparse_substitute_name(path: &[u16]) -> Option<Vec<u16>> {
    let (buf, n) = read_reparse(path)?;
    let bytes = buf.as_bytes(n);
    // REPARSE_DATA_BUFFER：標頭 8 位元組，其後 MountPointReparseBuffer：
    // SubstituteNameOffset(2) SubstituteNameLength(2) PrintNameOffset(2) PrintNameLength(2) PathBuffer
    let off = u16::from_le_bytes(bytes.get(8..10)?.try_into().ok()?) as usize;
    let len = u16::from_le_bytes(bytes.get(10..12)?.try_into().ok()?) as usize;
    let start = 16usize.checked_add(off)?;
    let name = bytes.get(start..start.checked_add(len)?)?;
    Some(
        name.chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect(),
    )
}

/// Windows 錯誤碼對應的英文訊息（取不到時回傳通用文字）。
pub fn error_message(code: u32) -> String {
    let mut buf = [0u16; 512];
    // SAFETY: 緩衝區為本地陣列，長度與參數一致；FROM_SYSTEM|IGNORE_INSERTS 不需要來源與引數。
    let n = unsafe {
        FormatMessageW(
            0x1000 | 0x200,
            std::ptr::null(),
            code,
            0x0409,
            buf.as_mut_ptr(),
            buf.len() as u32,
            std::ptr::null_mut(),
        )
    };
    if n == 0 {
        return format!("Windows error {code}");
    }
    String::from_utf16_lossy(&buf[..n as usize])
        .trim_end()
        .to_string()
}

pub fn is_elevated() -> bool {
    let mut token: Handle = std::ptr::null_mut();
    // SAFETY: GetCurrentProcess 回傳偽句柄；token 為本地輸出變數。
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let token = OwnedHandle(token);
    let mut elevation = 0u32;
    let mut returned = 0u32;
    // SAFETY: TokenElevation 的輸出是單一 DWORD；長度參數與其大小一致。
    let ok = unsafe {
        GetTokenInformation(
            token.0,
            TOKEN_ELEVATION_CLASS,
            (&mut elevation as *mut u32).cast::<c_void>(),
            std::mem::size_of::<u32>() as u32,
            &mut returned,
        )
    };
    ok != 0 && elevation != 0
}

/// 磁碟區資訊。
pub struct VolumeDetails {
    pub root: Vec<u16>,
    pub fs: String,
    pub cluster_bytes: u64,
    pub capacity_bytes: u64,
    pub free_bytes: u64,
    pub is_network: bool,
}

/// 所有本機（含網路）已掛載的磁碟機根路徑，例如 `C:\`。
pub fn logical_drives() -> Vec<Vec<u16>> {
    let mut buf = vec![0u16; 1024];
    // SAFETY: 緩衝區長度與參數一致。
    let n = unsafe { GetLogicalDriveStringsW(buf.len() as u32, buf.as_mut_ptr()) } as usize;
    if n == 0 || n > buf.len() {
        return Vec::new();
    }
    buf[..n]
        .split(|&c| c == 0)
        .filter(|s| !s.is_empty())
        .map(<[u16]>::to_vec)
        .collect()
}

/// 路徑所在的磁碟區根路徑。
pub fn volume_path_name(path: &[u16]) -> Option<Vec<u16>> {
    let wide = nul_terminated(path);
    let mut out = vec![0u16; 1024];
    // SAFETY: 輸入以 NUL 結尾；輸出緩衝區長度與參數一致。
    let ok = unsafe { GetVolumePathNameW(wide.as_ptr(), out.as_mut_ptr(), out.len() as u32) };
    if ok == 0 {
        return None;
    }
    let end = out.iter().position(|&c| c == 0)?;
    out.truncate(end);
    Some(out)
}

pub fn volume_details(root: &[u16]) -> Option<VolumeDetails> {
    let wide = nul_terminated(root);
    let mut fs = [0u16; 64];
    // SAFETY: 輸入以 NUL 結尾；僅取檔案系統名稱，其他輸出參數傳空指標（文件允許）。
    let ok = unsafe {
        GetVolumeInformationW(
            wide.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fs.as_mut_ptr(),
            fs.len() as u32,
        )
    };
    if ok == 0 {
        return None;
    }
    let fs_len = fs.iter().position(|&c| c == 0).unwrap_or(fs.len());
    let (mut spc, mut bps, mut free_c, mut total_c) = (0u32, 0u32, 0u32, 0u32);
    // SAFETY: 四個輸出皆為本地變數。
    let cluster_ok =
        unsafe { GetDiskFreeSpaceW(wide.as_ptr(), &mut spc, &mut bps, &mut free_c, &mut total_c) };
    let cluster_bytes = if cluster_ok != 0 {
        u64::from(spc) * u64::from(bps)
    } else {
        0
    };
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    // SAFETY: 三個輸出皆為本地變數。
    let space_ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, &mut total, &mut free) };
    // SAFETY: 輸入以 NUL 結尾。
    let dtype = unsafe { GetDriveTypeW(wide.as_ptr()) };
    Some(VolumeDetails {
        root: root.to_vec(),
        fs: String::from_utf16_lossy(&fs[..fs_len]),
        cluster_bytes,
        capacity_bytes: if space_ok != 0 { total } else { 0 },
        free_bytes: if space_ok != 0 { avail } else { 0 },
        is_network: dtype == DRIVE_REMOTE,
    })
}

static INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetConsoleCtrlHandler(handler: Option<extern "system" fn(u32) -> i32>, add: i32) -> i32;
}

extern "system" fn ctrl_handler(kind: u32) -> i32 {
    // CTRL_C_EVENT (0) 與 CTRL_BREAK_EVENT (1)：只設旗標，由掃描迴圈自行收尾
    if kind == 0 || kind == 1 {
        INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
        1
    } else {
        0
    }
}

/// 安裝 Ctrl+C／Ctrl+Break 處理常式；之後以 `interrupted()` 查詢。
pub fn install_interrupt_handler() {
    // SAFETY: 傳入的處理常式是本模組中簽章正確、不 panic、只存取原子變數的函式。
    unsafe {
        SetConsoleCtrlHandler(Some(ctrl_handler), 1);
    }
}

pub fn interrupted() -> bool {
    INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst)
}
