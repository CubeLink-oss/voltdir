<#
.SYNOPSIS
  建立 M3 用的夾具磁碟區：一個 VHDX（NTFS，預設掛成 V:），內含各種邊界情況，供 walk 與 mft 兩個引擎互相比對。
.DESCRIPTION
  需要系統管理員身分（建立並掛載 VHD）。只動 -Vhd 指定的檔案與新掛載的磁碟機代號，不碰其他磁碟區。
  -Remove 卸載並刪除 VHDX。
  夾具內容：常駐小檔、空檔、稀疏檔（含 5 GB 邏輯大小）、NTFS 壓縮檔、WOF 壓縮檔、硬連結（含大量名稱而需要屬性清單）、
  接合點、目錄與檔案符號連結、長路徑（>260）、Unicode 與代理字元名稱、具名資料串流、隱藏＋系統目錄、
  嚴重碎片化的檔案（執行串列延伸到其他記錄）、區分大小寫目錄（若系統支援）。
#>
[CmdletBinding()]
param(
    [string]$Vhd = (Join-Path (Split-Path -Parent $PSScriptRoot) 'bench\fixture.vhdx'),
    [ValidatePattern('^[A-Za-z]$')][string]$Letter = 'V',
    [int]$SizeMB = 1024,
    [switch]$Remove
)
$ErrorActionPreference = 'Stop'
$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) { throw '需要系統管理員身分。' }
if ($Vhd -match '^[Cc]:') { throw 'VHDX 不可放在 C 槽。' }
$drive = "$($Letter.ToUpper()):"

function Invoke-Diskpart([string[]]$lines) {
    $f = [System.IO.Path]::GetTempFileName()
    try {
        Set-Content $f $lines -Encoding ASCII
        $out = & diskpart.exe /s $f 2>&1
        $out | ForEach-Object { Write-Host "  [diskpart] $_" }
        if ($LASTEXITCODE -ne 0) { throw "diskpart 失敗（$LASTEXITCODE）" }
    } finally { Remove-Item $f -Force -ErrorAction SilentlyContinue }
}

if ($Remove) {
    if (Test-Path $Vhd) {
        Invoke-Diskpart @("select vdisk file=`"$Vhd`"", 'detach vdisk')
        Remove-Item $Vhd -Force
    }
    Write-Host "已移除夾具磁碟區 $Vhd"
    return
}

if (Test-Path $drive) { throw "磁碟機代號 $drive 已被使用。" }
if (Test-Path $Vhd) { throw "$Vhd 已存在；請先用 -Remove 移除。" }
New-Item -ItemType Directory -Force (Split-Path $Vhd) | Out-Null
Invoke-Diskpart @(
    "create vdisk file=`"$Vhd`" maximum=$SizeMB type=expandable",
    "select vdisk file=`"$Vhd`"", 'attach vdisk',
    'create partition primary',
    'format fs=ntfs quick label=DSFIX',
    "assign letter=$($Letter.ToUpper())"
)
for ($i = 0; $i -lt 30 -and -not (Test-Path "$drive\"); $i++) { Start-Sleep -Milliseconds 500 }
if (-not (Test-Path "$drive\")) { throw "$drive 沒有出現。" }

$r = "$drive\"
function Make-Dir([string]$p) { $null = New-Item -ItemType Directory -Force -Path $p }
function Write-Bytes([string]$p, [byte[]]$b) { [System.IO.File]::WriteAllBytes($p, $b) }
$rng = [System.Random]::new(12345)

# --- 常駐小檔、空檔、一般檔 ---
Make-Dir "${r}tiny"; Make-Dir "${r}empty"; Make-Dir "${r}plain"
Write-Bytes "${r}tiny\a.txt" ([byte[]](72, 105))
Write-Bytes "${r}tiny\b81.txt" ([byte[]](, 65 * 81))
Write-Bytes "${r}tiny\zero.bin" ([byte[]]@())
foreach ($n in 1, 4095, 4096, 4097, 100000, 1048576) {
    $b = New-Object byte[] $n; $rng.NextBytes($b); Write-Bytes "${r}plain\f$n.bin" $b
}

# --- 稀疏檔 ---
Make-Dir "${r}sparse"
Write-Bytes "${r}sparse\s.bin" ([byte[]]@())
& fsutil.exe sparse setflag "${r}sparse\s.bin" | Out-Null
$fs = [System.IO.File]::Open("${r}sparse\s.bin", 'Open', 'Write', 'ReadWrite')
try { $fs.SetLength(5GB); $fs.Position = 1GB; $b = New-Object byte[] 65536; $rng.NextBytes($b); $fs.Write($b, 0, $b.Length) } finally { $fs.Dispose() }

# --- NTFS 壓縮與 WOF 壓縮 ---
Make-Dir "${r}comp"; Make-Dir "${r}wof"
$z = New-Object byte[] (2MB); Write-Bytes "${r}comp\zeros.bin" $z
& compact.exe /c /i "${r}comp\zeros.bin" | Out-Null
$t = [System.Text.Encoding]::ASCII.GetBytes(('voltdir fixture line ' * 60000))
Write-Bytes "${r}wof\text.bin" $t
& compact.exe /c /exe:lzx /f "${r}wof\text.bin" | Out-Null

# --- 硬連結（含 60 個名稱，迫使 FILE_NAME 超出基底記錄而使用屬性清單）---
Make-Dir "${r}hl"
$b = New-Object byte[] 10000; $rng.NextBytes($b); Write-Bytes "${r}hl\orig.bin" $b
& fsutil.exe hardlink create "${r}hl\second.bin" "${r}hl\orig.bin" | Out-Null
Make-Dir "${r}hl\many"
for ($i = 0; $i -lt 60; $i++) {
    $nm = ('link_with_a_fairly_long_name_to_use_up_record_space_{0:D3}_' -f $i) + ('x' * 60) + '.bin'
    & fsutil.exe hardlink create "${r}hl\many\$nm" "${r}hl\orig.bin" | Out-Null
}

# --- 連結 ---
Make-Dir "${r}links"
& cmd.exe /c mklink /j "${r}links\junc" "${r}empty" | Out-Null
& cmd.exe /c mklink /d "${r}links\dsym" "${r}empty" | Out-Null
& cmd.exe /c mklink "${r}links\fsym" "${r}tiny\a.txt" | Out-Null

# --- Unicode 與代理字元 ---
Make-Dir "${r}uni"
$names = @("測試檔案.txt", "emoji_$([char]::ConvertFromUtf32(0x1F642)).txt", "ÄÖÜ ß.txt", "a b  c.txt")
foreach ($n in $names) { Write-Bytes "\\?\${r}uni\$n" ([byte[]](1, 2, 3)) }

# --- 長路徑 ---
$long = "\\?\${r}longpath"
for ($i = 0; $i -lt 12; $i++) { $long += '\' + ('d' * 30) + $i }
$null = [System.IO.Directory]::CreateDirectory($long)
Write-Bytes "$long\deep.txt" ([byte[]](1, 2, 3, 4, 5))

# --- 具名資料串流（預設串流不含）---
Make-Dir "${r}ads"
Write-Bytes "${r}ads\f.txt" ([byte[]](1, 2, 3))
Set-Content -Path "${r}ads\f.txt" -Stream 'extra' -Value ('y' * 5000)

# --- 屬性 ---
Make-Dir "${r}attrs"
Write-Bytes "${r}attrs\ro.txt" ([byte[]](1)); Write-Bytes "${r}attrs\hid.txt" ([byte[]](1)); Write-Bytes "${r}attrs\sys.txt" ([byte[]](1))
& attrib.exe +r "${r}attrs\ro.txt"; & attrib.exe +h "${r}attrs\hid.txt"; & attrib.exe +s +h "${r}attrs\sys.txt"
Make-Dir "${r}hs"; Write-Bytes "${r}hs\x.txt" ([byte[]](1, 2)); & attrib.exe +s +h "${r}hs"

# --- 碎片化檔案：三個檔案輪流追加 4 KB，執行串列會很長 ---
Make-Dir "${r}frag"
$streams = 1..3 | ForEach-Object { [System.IO.FileStream]::new("${r}frag\f$_.bin", [System.IO.FileMode]::Create, [System.IO.FileAccess]::Write, [System.IO.FileShare]::Read, 4096, [System.IO.FileOptions]::WriteThrough) }
try {
    $chunk = New-Object byte[] 4096
    for ($i = 0; $i -lt 6000; $i++) { foreach ($s in $streams) { $rng.NextBytes($chunk); $s.Write($chunk, 0, $chunk.Length); $s.Flush() } }
} finally { $streams | ForEach-Object { $_.Dispose() } }

# --- 大量小檔 ---
for ($d = 0; $d -lt 20; $d++) {
    Make-Dir "${r}many\d$d"
    for ($i = 0; $i -lt 100; $i++) { Write-Bytes "${r}many\d$d\f$i.txt" ([byte[]](, [byte]($i % 250) * (($i % 40) + 1))) }
}

# --- 區分大小寫目錄---
try {
    Make-Dir "${r}cs"
    & fsutil.exe file setCaseSensitiveInfo "${r}cs" enable | Out-Null
    if ($LASTEXITCODE -eq 0) {
        Write-Bytes "${r}cs\a.txt" ([byte[]](1)); Write-Bytes "${r}cs\A.txt" ([byte[]](1, 2))
    } else { Write-Host '略過區分大小寫目錄（fsutil 不支援）' }
} catch { Write-Host "略過區分大小寫目錄：$_" }

Write-Host "夾具磁碟區已就緒：$drive （$Vhd）"
