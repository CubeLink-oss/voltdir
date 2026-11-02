<#
.SYNOPSIS
  基準對照組之一（powershell-gci）：用一條 Get-ChildItem 管線，在一次走訪中算出
  總計、前 N 大資料夾、前 N 大檔案、依副檔名的合計，輸出一個 JSON 物件。
.DESCRIPTION
  這是「寫得好的 PowerShell」，不是笨寫法：只走訪一次、串流處理（不把一百萬個 FileInfo 全留在記憶體）、
  用雜湊表累計、-ErrorAction SilentlyContinue 略過讀不到的位置。資料夾大小為含底下所有內容的合計。
  資訊量必須和 voltdir 的 report（--top-files、--top-dirs、--by-ext）對應的欄位相同。
#>
param(
    [Parameter(Mandatory)][string]$Root,
    [int]$Top = 20,
    [int]$TopExt = 10
)
$ErrorActionPreference = 'SilentlyContinue'
# 磁碟機根目錄要保留結尾的反斜線：「F:」指的是 F: 的目前資料夾，不是它的根目錄
$rootFull = [System.IO.Path]::GetFullPath($Root)
if ($rootFull.Length -gt 3) { $rootFull = $rootFull.TrimEnd('\') }
$count = 0L; $total = 0L
$ext = @{}; $dirs = @{}
$top = [System.Collections.Generic.List[object]]::new()
$floor = 0L

Get-ChildItem -LiteralPath $Root -Recurse -File -Force -ErrorAction SilentlyContinue | ForEach-Object {
    $len = $_.Length
    $count++; $total += $len
    $e = $_.Extension.ToLowerInvariant()
    $ext[$e] += $len
    $dirs[$_.DirectoryName] += $len
    if ($top.Count -lt $Top -or $len -gt $floor) {
        $top.Add([pscustomobject]@{ path = $_.FullName; size = $len })
        if ($top.Count -gt 4 * $Top) {
            $sorted = $top | Sort-Object size -Descending | Select-Object -First $Top
            $top.Clear(); $top.AddRange([object[]]@($sorted))
            $floor = $top[$top.Count - 1].size
        }
    }
}

# 資料夾合計：把每個資料夾的直接大小加到它所有的上層（不含掃描根）
$roll = @{}
foreach ($kv in $dirs.GetEnumerator()) {
    $d = $kv.Key
    while ($d -and $d.Length -gt $rootFull.Length) {
        $roll[$d] += $kv.Value
        $d = [System.IO.Path]::GetDirectoryName($d)
    }
}

[ordered]@{
    total_files = $count
    total_bytes = $total
    top_dirs    = @($roll.GetEnumerator() | Sort-Object @{ e = { $_.Value }; Descending = $true }, @{ e = { $_.Key } } | Select-Object -First $Top | ForEach-Object { [ordered]@{ path = $_.Key; size = $_.Value } })
    top_files   = @($top | Sort-Object size -Descending | Select-Object -First $Top)
    by_ext      = @($ext.GetEnumerator() | Sort-Object Value -Descending | Select-Object -First $TopExt | ForEach-Object { [ordered]@{ ext = $_.Key; size = $_.Value } })
} | ConvertTo-Json -Depth 5 -Compress
