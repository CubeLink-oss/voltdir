<#
.SYNOPSIS
  比對兩份 voltdir 快照（例如 walk 與 mft 對同一磁碟區的掃描），以「類型＋路徑」為鍵。
.DESCRIPTION
  兩份快照的輸出順序相同，所以以同步讀取、只對暫時對不上的項目建字典，記憶體用量很小。
  比對目錄的 rollup／direct／屬性／狀態／時間，與檔案的大小／屬性／時間／file_id／link_count。
  -IgnoreTimes 忽略 mtime 與 latest_mtime（掃描期間與目錄時間戳記延遲更新造成的差異）。
  -SummaryJson 另存摘要。結束代碼一律為 0，結果以輸出判讀。
#>
param(
    [Parameter(Mandatory)][string]$A,
    [Parameter(Mandatory)][string]$B,
    [int]$ShowDiffs = 30,
    [switch]$IgnoreTimes,
    [string]$SummaryJson
)
$ErrorActionPreference = 'Stop'
$str = '(?:[^"\\]|\\.)*'
$opt = [System.Text.RegularExpressions.RegexOptions]::Compiled
$dirRe = [regex]::new('^\{"type":"dir","path":"(?<path>' + $str + ')","name":"' + $str + '","depth":\d+,"mtime":"(?<mtime>[^"]*)","latest_mtime":"(?<lmtime>[^"]*)","attrs":\[[^\]]*\],"attrs_raw":"(?<attrs>[^"]*)","reparse":"(?<reparse>[^"]*)","reserved":(?<reserved>\w+),"file_id":(?<fid>"[^"]*"|null),"scan":"(?<scan>[^"]*)","error":(?<err>null|{[^}]*}),"rollup":\{"files":(?<rf>\d+),"dirs":(?<rd>\d+),"logical":(?<rl>\d+),"physical":(?<rp>\d+)\},"direct":\{"files":(?<df>\d+),"logical":(?<dl>\d+),"physical":(?<dp>\d+)\}', $opt)
$fileRe = [regex]::new('^\{"type":"file","path":"(?<path>' + $str + ')","name":"' + $str + '","ext":(?:"' + $str + '"|null),"depth":\d+,"size_logical":(?<sl>\d+),"size_physical":(?<sp>\d+),"mtime":"(?<mtime>[^"]*)","attrs":\[[^\]]*\],"attrs_raw":"(?<attrs>[^"]*)","reparse":"(?<reparse>[^"]*)","reserved":(?<reserved>\w+),"file_id":(?<fid>"[^"]*"|null),"link_count":(?<lc>\w+),"hardlink_secondary":(?<sec>\w+)\}', $opt)

$dirFields = @('attrs', 'reparse', 'reserved', 'fid', 'scan', 'err', 'rf', 'rd', 'rl', 'rp', 'df', 'dl', 'dp')
$fileFields = @('sl', 'sp', 'attrs', 'reparse', 'reserved', 'fid', 'lc', 'sec')
if (-not $IgnoreTimes) { $dirFields += @('mtime', 'lmtime'); $fileFields += @('mtime') }

# 一行 → @(鍵, 欄位陣列)；不是 dir／file 的行回傳 $null
function Read-Entry([string]$line) {
    if ($line.StartsWith('{"type":"dir"')) {
        $m = $dirRe.Match($line)
        if (-not $m.Success) { throw "無法解析目錄行：$($line.Substring(0, [Math]::Min(200, $line.Length)))" }
        $v = foreach ($f in $dirFields) { $m.Groups[$f].Value }
        return , @(('d|' + $m.Groups['path'].Value), $v, $dirFields)
    }
    if ($line.StartsWith('{"type":"file"')) {
        $m = $fileRe.Match($line)
        if (-not $m.Success) { throw "無法解析檔案行：$($line.Substring(0, [Math]::Min(200, $line.Length)))" }
        $v = foreach ($f in $fileFields) { $m.Groups[$f].Value }
        return , @(('f|' + $m.Groups['path'].Value), $v, $fileFields)
    }
    return $null
}

$fieldDiffs = @{}
$examples = New-Object System.Collections.Generic.List[string]
$onlyA = New-Object 'System.Collections.Generic.Dictionary[string,object]'
$onlyB = New-Object 'System.Collections.Generic.Dictionary[string,object]'
$matched = 0
$identical = 0

function Compare-Pair($key, $va, $vb, $names) {
    $script:matched++
    $same = $true
    for ($i = 0; $i -lt $names.Count; $i++) {
        if ($va[$i] -cne $vb[$i]) {
            $same = $false
            $k = $key.Substring(0, 1) + '.' + $names[$i]
            if ($fieldDiffs.ContainsKey($k)) { $fieldDiffs[$k]++ } else { $fieldDiffs[$k] = 1 }
            if ($examples.Count -lt 400) { $examples.Add("$k`t$($key.Substring(2))`tA=$($va[$i])`tB=$($vb[$i])") }
        }
    }
    if ($same) { $script:identical++ }
}

$ra = [System.IO.File]::OpenText($A)
$rb = [System.IO.File]::OpenText($B)
try {
    $doneA = $false; $doneB = $false
    while (-not ($doneA -and $doneB)) {
        $ea = $null; $eb = $null
        while (-not $doneA -and $null -eq $ea) {
            $l = $ra.ReadLine()
            if ($null -eq $l) { $doneA = $true } else { $ea = Read-Entry $l }
        }
        while (-not $doneB -and $null -eq $eb) {
            $l = $rb.ReadLine()
            if ($null -eq $l) { $doneB = $true } else { $eb = Read-Entry $l }
        }
        if ($null -ne $ea) {
            if ($onlyB.ContainsKey($ea[0])) {
                $o = $onlyB[$ea[0]]; $null = $onlyB.Remove($ea[0])
                Compare-Pair $ea[0] $ea[1] $o $ea[2]
            } else { $onlyA[$ea[0]] = $ea }
        }
        if ($null -ne $eb) {
            if ($onlyA.ContainsKey($eb[0])) {
                $o = $onlyA[$eb[0]]; $null = $onlyA.Remove($eb[0])
                Compare-Pair $eb[0] $o[1] $eb[1] $eb[2]
            } else { $onlyB[$eb[0]] = $eb[1] }
        }
    }
} finally { $ra.Dispose(); $rb.Dispose() }

Write-Host "配對項目：$matched；完全相同：$identical；只在 A：$($onlyA.Count)；只在 B：$($onlyB.Count)"
Write-Host '--- 欄位差異次數（d=目錄, f=檔案）'
$fieldDiffs.GetEnumerator() | Sort-Object Name | ForEach-Object { Write-Host ("  {0,-14} {1}" -f $_.Name, $_.Value) }
Write-Host "--- 前 $ShowDiffs 筆差異"
$examples | Select-Object -First $ShowDiffs | ForEach-Object { Write-Host "  $_" }
Write-Host "--- 只在 A（前 $ShowDiffs 筆）"
$onlyA.Keys | Select-Object -First $ShowDiffs | ForEach-Object { Write-Host "  $_" }
Write-Host "--- 只在 B（前 $ShowDiffs 筆）"
$onlyB.Keys | Select-Object -First $ShowDiffs | ForEach-Object { Write-Host "  $_" }
if ($SummaryJson) {
    [ordered]@{
        matched = $matched; identical = $identical; onlyA = $onlyA.Count; onlyB = $onlyB.Count
        fieldDiffs = $fieldDiffs; examples = @($examples | Select-Object -First 200)
        onlyAFirst = @($onlyA.Keys | Select-Object -First 200); onlyBFirst = @($onlyB.Keys | Select-Object -First 200)
    } | ConvertTo-Json -Depth 4 | Set-Content $SummaryJson -Encoding UTF8
}
