<#
.SYNOPSIS
  把 voltdir.bench/1 結果紀錄彙整成誠實的 Markdown 結果表。
.DESCRIPTION
  誠實規則（設計文件第 3 節）在這裡強制執行，而不是靠人記得：
  - 每格至少 n = 5 才顯示倍數，否則顯示「樣本不足」。
  - 倍數只在同一個 metric 內計算，並一律標明量的是什麼。
  - 對照組取最佳者（powershell-gci、dotnet-enum、dotnet-enum-stream、python-scandir、python-scandir-threads；耗時取最快、記憶體取最低，每個 metric 各自挑）。
  - voltdir 較慢時照實顯示（倍數小於 1），不隱藏。
  - 答錯的 agent 執行不計入倍數，但顯示「答錯 k 次」。
  - 提權與非提權不得混在同一格；快取狀態（warm/cold）也不得混用。
  - 紀錄或輸出含使用者名稱、主機名稱、個人資料夾路徑時拒絕。
  - 沒有紀錄的任務列出「未量測」。
.EXAMPLE
  pwsh -File benchmarks\Build-ResultsTable.ps1 -ResultsDir benchmarks\results -Out results.md
  pwsh -File benchmarks\Build-ResultsTable.ps1 -SelfTest
#>
[CmdletBinding()]
param(
    [string]$ResultsDir = (Join-Path $PSScriptRoot 'results'),
    [string]$Out,
    [switch]$SelfTest
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Schema = 'voltdir.bench/1'
$MinN = 5
$TaskIds = @('B1', 'B2', 'B3', 'B4', 'B5')
$TaskTitles = @{
    B1 = '整顆磁碟，前 20 大資料夾與檔案（系統管理員）'
    B2 = '整顆磁碟，前 20 大資料夾與檔案（一般權限）'
    B3 = '掃一次再追問 5 個問題（同一份資料；權限見每列）'
    B4 = '比昨天多了什麼（同路徑與跨路徑）'
    B5 = '100 萬檔案的資料夾，一次問常見問題'
}
$VoltdirTools = @('voltdir-walk', 'voltdir-mft', 'voltdir-index')
$BaselineTools = @('powershell-gci', 'dotnet-enum', 'dotnet-enum-stream', 'python-scandir', 'python-scandir-threads')
$MetricInfo = @{
    tool_seconds  = @{ kind = 'tool'; label = '工具耗時'; unit = 's' }
    peak_ws_mb    = @{ kind = 'tool'; label = '峰值記憶體'; unit = 'MB' }
    followup_seconds = @{ kind = 'tool'; label = '5 個追問的耗時'; unit = 's' }
    agent_seconds = @{ kind = 'agent'; label = 'agent 端到端耗時'; unit = 's' }
    tokens        = @{ kind = 'agent'; label = 'token 用量'; unit = 'tokens' }
    tool_calls    = @{ kind = 'agent'; label = '呼叫次數'; unit = '次' }
    tool_errors   = @{ kind = 'agent'; label = '工具錯誤次數'; unit = 'errors' }
}
$MetricOrder = @('tool_seconds', 'peak_ws_mb', 'followup_seconds', 'agent_seconds', 'tokens', 'tool_calls', 'tool_errors')
$inv = [System.Globalization.CultureInfo]::InvariantCulture

function Get-Median([double[]]$v) {
    $s = @($v | Sort-Object)
    $n = $s.Count
    if ($n -eq 0) { return $null }
    if ($n % 2 -eq 1) { return [double]$s[($n - 1) / 2] }
    return ([double]$s[$n / 2 - 1] + [double]$s[$n / 2]) / 2
}

function Format-Num([double]$x) { $x.ToString('0.##', $inv) }

function Test-Privacy([string]$text, [string]$what) {
    foreach ($bad in @($env:USERNAME, $env:COMPUTERNAME)) {
        if ($bad -and $bad.Length -ge 3 -and $text.IndexOf($bad, [StringComparison]::OrdinalIgnoreCase) -ge 0) {
            throw "$what 含使用者名稱或主機名稱，拒絕（隱私規則）。"
        }
    }
    if ($text -match '(?i)[a-z]:\\{1,2}users\\{1,2}[^\\"]') {
        throw "$what 含個人資料夾的路徑（X:\Users\...），拒絕（隱私規則）。"
    }
}

function Test-Record($r, [string]$where) {
    foreach ($f in 'schema', 'task', 'tool', 'kind', 'metric', 'unit', 'n', 'median', 'runs', 'elevated', 'env') {
        if (-not $r.PSObject.Properties[$f]) { throw "$where：缺少欄位 '$f'。" }
    }
    if ($r.schema -ne $Schema) { throw "$where：schema 必須是 $Schema，實際是 '$($r.schema)'。" }
    if ($r.task -notin $TaskIds) { throw "$where：task 必須是 B1 到 B5，實際是 '$($r.task)'。" }
    if ($r.tool -notin ($VoltdirTools + $BaselineTools + @('agent-voltdir', 'agent-baseline'))) { throw "$where：未知的 tool '$($r.tool)'。" }
    if (-not $MetricInfo.ContainsKey([string]$r.metric)) { throw "$where：未知的 metric '$($r.metric)'。" }
    $mk = $MetricInfo[[string]$r.metric].kind
    if ($r.kind -ne $mk) { throw "$where：metric $($r.metric) 屬於 kind=$mk，實際是 '$($r.kind)'。" }
    if (($r.tool -like 'agent-*') -ne ($r.kind -eq 'agent')) { throw "$where：tool $($r.tool) 與 kind $($r.kind) 不一致。" }
    if ($r.elevated -isnot [bool]) { throw "$where：elevated 必須是布林值。" }
    $runs = @($r.runs)
    if ($runs.Count -eq 0) { throw "$where：runs 不可為空。" }
    if ([int]$r.n -ne $runs.Count) { throw "$where：n=$($r.n) 與 runs 的筆數 $($runs.Count) 不符。" }
    $m = Get-Median ([double[]]$runs)
    if ([math]::Abs($m - [double]$r.median) -gt 0.01) { throw "$where：median=$($r.median) 與 runs 算出的 $m 不符。" }
    if ($r.kind -eq 'agent' -and (-not $r.PSObject.Properties['correct'] -or $r.correct -isnot [bool])) {
        throw "$where：agent 紀錄必須有布林欄位 correct。"
    }
}

function Get-Prop($o, [string]$name, $default = $null) {
    if ($o.PSObject.Properties[$name]) { $o.$name } else { $default }
}

# 把紀錄依「任務 × 變體 × 工具 × metric」併成格子，並強制執行不得混用的規則。
function Group-Cells($records) {
    $cells = [ordered]@{}
    foreach ($r in $records) {
        $variant = [string](Get-Prop $r 'variant' '')
        $key = "$($r.task)|$variant|$($r.tool)|$($r.metric)"
        if (-not $cells.Contains($key)) {
            $cells[$key] = [pscustomobject]@{
                task = [string]$r.task; variant = $variant; tool = [string]$r.tool; metric = [string]$r.metric
                elevated = [bool]$r.elevated; cache = [string]$r.env.cache; values = @(); wrong = 0; noise = @()
            }
        }
        $c = $cells[$key]
        if ($c.elevated -ne [bool]$r.elevated) { throw "同一格（$key）同時有 elevated=true 與 elevated=false 的紀錄，不可混用。" }
        if ($c.cache -ne [string]$r.env.cache) { throw "同一格（$key）同時有 cache=$($c.cache) 與 cache=$($r.env.cache) 的紀錄，不可混用。" }
        if ($r.kind -eq 'agent') {
            if ($r.correct -eq $false) { $c.wrong += 1 } else { $c.values += [double[]]@($r.runs) }
            $c.wrong += [int](Get-Prop $r 'incorrect' 0)
        } else {
            $c.values += [double[]]@($r.runs)
        }
        $nw = Get-Prop $r 'noise_warning' $null
        if ($nw -and ($c.noise -notcontains [string]$nw)) { $c.noise += [string]$nw }
    }
    # 同一個比較（任務 × 變體 × metric）內，各工具的權限也必須一致
    $cmp = @{}
    foreach ($c in $cells.Values) {
        $k = "$($c.task)|$($c.variant)|$($c.metric)"
        if ($cmp.ContainsKey($k) -and $cmp[$k] -ne $c.elevated) {
            throw "同一個比較（$k）內的工具有的提權、有的沒提權（elevated 不一致），不可相比。"
        }
        $cmp[$k] = $c.elevated
    }
    $cells
}

function Format-Side($c) {
    if ($c.values.Count -eq 0) { return "$($c.tool)（沒有有效執行）" }
    "$($c.tool) $(Format-Num (Get-Median $c.values)) $($MetricInfo[$c.metric].unit)"
}

function Format-Results($records) {
    $cells = Group-Cells $records
    $sb = [System.Text.StringBuilder]::new()
    foreach ($t in $TaskIds) {
        [void]$sb.AppendLine("## $t — $($TaskTitles[$t])")
        [void]$sb.AppendLine()
        $mine = @($cells.Values | Where-Object { $_.task -eq $t })
        if ($mine.Count -eq 0) {
            [void]$sb.AppendLine('**未量測。**')
            [void]$sb.AppendLine()
            continue
        }
        [void]$sb.AppendLine('| 任務 | 量的是什麼 | voltdir | 對照組（取較快者） | 倍數（對照 ÷ voltdir） | 判定 | 樣本數 | 備註 |')
        [void]$sb.AppendLine('|---|---|---|---|---|---|---|---|')
        foreach ($variant in @($mine | ForEach-Object { $_.variant } | Select-Object -Unique)) {
            foreach ($metric in $MetricOrder) {
                $group = @($mine | Where-Object { $_.variant -eq $variant -and $_.metric -eq $metric })
                if ($group.Count -eq 0) { continue }
                $kind = $MetricInfo[$metric].kind
                $vTools = if ($kind -eq 'agent') { @('agent-voltdir') } else { $VoltdirTools }
                $bTools = if ($kind -eq 'agent') { @('agent-baseline') } else { $BaselineTools }
                $vs = @($group | Where-Object { $_.tool -in $vTools })
                $bs = @($group | Where-Object { $_.tool -in $bTools })
                $perm = if ($group[0].elevated) { '提權' } else { '一般權限' }
                $label = "$t$(if ($variant) { " $variant" }) · $perm"
                $what = "$($MetricInfo[$metric].label)（$($MetricInfo[$metric].unit)）"
                $noise = @($group | ForEach-Object { $_.noise } | Select-Object -Unique)
                $notes = @()
                if ($noise.Count) { $notes += "雜訊警告：$($noise -join '；')" }
                $wrongV = 0; foreach ($x in $vs) { $wrongV += $x.wrong }
                $wrongB = 0; foreach ($x in $bs) { $wrongB += $x.wrong }
                if ($wrongV) { $notes += "voltdir 組答錯 $wrongV 次（不計入）" }
                if ($wrongB) { $notes += "對照組答錯 $wrongB 次（不計入）" }
                if ($vs.Count -eq 0 -or $bs.Count -eq 0) {
                    $missing = if ($vs.Count -eq 0) { 'voltdir' } else { '對照組' }
                    $shown = ($group | ForEach-Object { Format-Side $_ }) -join '；'
                    [void]$sb.AppendLine("| $label | $what | $(if ($vs.Count) { ($vs | ForEach-Object { Format-Side $_ }) -join '；' } else { '—' }) | $(if ($bs.Count) { ($bs | ForEach-Object { Format-Side $_ }) -join '；' } else { '—' }) | — | 未量測（缺$missing） | — | $((@($shown) + $notes) -join '；') |")
                    continue
                }
                foreach ($v in $vs) {
                    $eligible = @($bs | Where-Object { $_.values.Count -ge $MinN })
                    $vn = $v.values.Count
                    $bn = 0; foreach ($x in $bs) { if ($x.values.Count -gt $bn) { $bn = $x.values.Count } }
                    $nText = "voltdir n=$vn / 對照 n=$bn"
                    $row = $null
                    if ($vn -lt $MinN -or $eligible.Count -eq 0) {
                        $row = @((Format-Side $v), (($bs | ForEach-Object { Format-Side $_ }) -join '；'), '—', "樣本不足（每格需 n≥$MinN）")
                    } else {
                        $best = $eligible | Sort-Object { Get-Median $_.values } | Select-Object -First 1
                        $vm = Get-Median $v.values
                        $bm = Get-Median $best.values
                        if ($vm -eq 0) {
                            $zeroText = if ($bm -eq 0) { '雙方皆為 0（相同）' } else { "voltdir 為 0、對照組為 $(Format-Num $bm)（除以 0，不換算成倍數；voltdir 用量較少）" }
                            $row = @((Format-Side $v), (Format-Side $best), '—', $zeroText)
                        } else {
                            $ratio = $bm / $vm
                            $rt = Format-Num $ratio
                            $verdict = if ($ratio -ge 10) { "達 10 倍（$rt 倍）" }
                            elseif ($ratio -ge 1) { "未達 10 倍（$rt 倍）" }
                            elseif ($metric -in 'tool_seconds', 'followup_seconds', 'agent_seconds') { "voltdir 較慢（$rt 倍），未達 10 倍" }
                            else { "voltdir 用量較高（$rt 倍），未達 10 倍" }
                            $row = @((Format-Side $v), (Format-Side $best), "$rt 倍", $verdict)
                        }
                    }
                    [void]$sb.AppendLine("| $label | $what | $($row[0]) | $($row[1]) | $($row[2]) | $($row[3]) | $nText | $($notes -join '；') |")
                }
            }
        }
        [void]$sb.AppendLine()
    }
    $sb.ToString()
}

function Get-Footer {
@'
## 量法規則（摘要）

1. 對照組是寫得好的 PowerShell：`Get-ChildItem` 管線、精簡 .NET 列舉與邊走邊彙總的 .NET 串流；耗時取**最快**、記憶體取**最低**的一個。
2. 每格至少 n = 5 才顯示倍數，取中位數；樣本不足的格子不顯示倍數。
3. 每個倍數只在同一個量測項目內計算，並標明量的是什麼（工具耗時、agent 端到端耗時、token、呼叫次數或峰值記憶體）；不同項目不互相換算。
4. agent 答錯的執行不計入倍數，但會顯示答錯的次數。
5. 提權與一般權限、快取狀態不同的紀錄不會被混在同一格。
6. 量測當下若偵測到其他程序長期占用 CPU，該格會標「雜訊警告」，數字僅供參考。

## 未達標項目不隱藏

任何一項沒有達到 10 倍，包括 voltdir 比對照組慢的情況，都照實列在上表，並列入檢討；不會改題目、換對照組或刪除不利的執行。沒有量測的任務明確標示「未量測」。
'@
}

function Build-All($records) {
    $body = Format-Results $records
    $text = "# voltdir 基準結果`n`n由 ``benchmarks/Build-ResultsTable.ps1`` 從 ``benchmarks/results/*.json`` 產生，請勿手動編輯。`n`n$body`n$(Get-Footer)`n"
    Test-Privacy $text '結果表'
    $text
}

function Read-Records([string]$dir) {
    $records = @()
    foreach ($f in Get-ChildItem -LiteralPath $dir -Filter '*.json' -File -ErrorAction SilentlyContinue | Sort-Object Name) {
        $text = Get-Content -LiteralPath $f.FullName -Raw -Encoding UTF8
        Test-Privacy $text $f.Name
        $r = $text | ConvertFrom-Json
        Test-Record $r $f.Name
        $records += $r
    }
    $records
}

# ---------------- 自我測試 ----------------
function New-Rec {
    param([string]$Task = 'B5', [string]$Tool, [string]$Metric = 'tool_seconds', [double[]]$Runs, [bool]$Elevated = $false,
        $Correct = $null, [string]$Noise = $null, [string]$Variant = $null, [int]$Incorrect = 0, [string]$Cache = 'warm', [string]$Notes = '')
    $kind = $MetricInfo[$Metric].kind
    $o = [ordered]@{
        schema = $Schema; task = $Task; tool = $Tool; kind = $kind; metric = $Metric; unit = $MetricInfo[$Metric].unit
        n = $Runs.Count; median = (Get-Median $Runs); runs = $Runs; elevated = $Elevated
        noise_warning = $Noise; env = [ordered]@{ os = 'Windows'; cpu_threads = 8; voltdir = 'test'; cache = $Cache }; notes = $Notes
    }
    if ($kind -eq 'agent') { $o.correct = if ($null -eq $Correct) { $true } else { [bool]$Correct } }
    if ($Variant) { $o.variant = $Variant }
    if ($Incorrect) { $o.incorrect = $Incorrect }
    [pscustomobject]$o
}

function Invoke-SelfTest {
    $fails = 0
    $check = {
        param([string]$name, [bool]$ok)
        if ($ok) { Write-Host "PASS: $name" } else { Write-Host "FAIL: $name"; $script:fails++ }
    }
    $script:fails = 0
    $five = { param($x) [double[]]@($x, $x, $x, $x, $x) }

    # (a) n=3 → 樣本不足、沒有倍數
    $t = Format-Results @((New-Rec -Tool 'voltdir-walk' -Runs @(2, 2, 2)), (New-Rec -Tool 'dotnet-enum' -Runs (& $five 4)))
    & $check '(a) n=3 顯示樣本不足' ($t -match '樣本不足')
    & $check '(a) n=3 不顯示倍數' ($t -notmatch '\d(\.\d+)? 倍')

    # (b) voltdir 較慢
    $t = Format-Results @((New-Rec -Tool 'voltdir-walk' -Runs (& $five 12)), (New-Rec -Tool 'dotnet-enum' -Runs (& $five 2)))
    & $check '(b) 顯示「voltdir 較慢」' ($t -match 'voltdir 較慢')
    & $check '(b) 倍數為 0.17' ($t -match '0\.17')

    $t = Format-Results @((New-Rec -Tool 'voltdir-walk' -Metric 'peak_ws_mb' -Runs (& $five 200)), (New-Rec -Tool 'dotnet-enum' -Metric 'peak_ws_mb' -Runs (& $five 100)))
    & $check '(b2) 記憶體用量較高時寫「用量較高」而不是「較慢」' (($t -match 'voltdir 用量較高') -and ($t -notmatch 'voltdir 較慢'))

    # 三個對照組時，記憶體倍數以最低者為準（dotnet-enum-stream 的 20 MB，不是 dotnet-enum 的 160 MB）
    $t = Format-Results @((New-Rec -Tool 'voltdir-walk' -Metric 'peak_ws_mb' -Runs (& $five 40)), (New-Rec -Tool 'dotnet-enum' -Metric 'peak_ws_mb' -Runs (& $five 160)), (New-Rec -Tool 'dotnet-enum-stream' -Metric 'peak_ws_mb' -Runs (& $five 20)))
    & $check '(b3) 記憶體倍數取最低的對照組（dotnet-enum-stream 20，0.5 倍，voltdir 用量較高）' (($t -match 'dotnet-enum-stream 20') -and ($t -match '0\.5') -and ($t -match 'voltdir 用量較高'))

    # tool_errors（agent 的工具錯誤次數）：同樣是越低越好，出現在表中，voltdir 較高時寫「用量較高」
    $t = Format-Results @((New-Rec -Task 'B5' -Tool 'agent-voltdir' -Metric 'tool_errors' -Runs @(0, 0, 1, 0, 0)), (New-Rec -Task 'B5' -Tool 'agent-baseline' -Metric 'tool_errors' -Runs @(3, 4, 5, 4, 4)))
    & $check '(b3) tool_errors 出現在表中（單位 errors），voltdir 為 0 時不除以 0' (($t -match '工具錯誤次數') -and ($t -match 'errors') -and ($t -match '不換算成倍數'))

    # (c) 同格提權與非提權混用 → 錯誤
    $msg = $null
    try { Format-Results @((New-Rec -Tool 'voltdir-walk' -Runs (& $five 1) -Elevated $true), (New-Rec -Tool 'voltdir-walk' -Runs (& $five 1) -Elevated $false)) | Out-Null } catch { $msg = $_.Exception.Message }
    & $check '(c) 同格混用提權時丟出含 elevated 的錯誤' ($msg -and $msg -match 'elevated')
    $msg = $null
    try { Format-Results @((New-Rec -Tool 'voltdir-walk' -Runs (& $five 1) -Elevated $true), (New-Rec -Tool 'dotnet-enum' -Runs (& $five 9) -Elevated $false)) | Out-Null } catch { $msg = $_.Exception.Message }
    & $check '(c) 同一比較內工具權限不同也丟出錯誤' ($msg -and $msg -match 'elevated')

    # (d) 取較快的對照組
    $t = Format-Results @((New-Rec -Tool 'voltdir-walk' -Runs (& $five 1)), (New-Rec -Tool 'powershell-gci' -Runs (& $five 20)), (New-Rec -Tool 'dotnet-enum' -Runs (& $five 10)))
    & $check '(d) 倍數為 10（取較快者 dotnet-enum）' (($t -match '10 倍') -and ($t -match 'dotnet-enum 10'))

    # (e) 答錯不進倍數，且顯示答錯次數
    $recs = @(
        (New-Rec -Task 'B5' -Tool 'agent-voltdir' -Metric 'agent_seconds' -Runs @(10, 10, 10, 10, 10, 10) -Correct $true),
        (New-Rec -Task 'B5' -Tool 'agent-voltdir' -Metric 'agent_seconds' -Runs @(1) -Correct $false),
        (New-Rec -Task 'B5' -Tool 'agent-baseline' -Metric 'agent_seconds' -Runs @(20, 20, 20, 20, 20))
    )
    $t = Format-Results $recs
    & $check '(e) 輸出含「答錯 1 次」' ($t -match '答錯 1 次')
    & $check '(e) 答錯的執行不進倍數（20 ÷ 10 = 2 倍）' ($t -match '2 倍')

    # (f) 達與未達都顯示實際倍數
    $t = Format-Results @((New-Rec -Task 'B1' -Tool 'voltdir-mft' -Runs (& $five 1) -Elevated $true), (New-Rec -Task 'B1' -Tool 'dotnet-enum' -Runs (& $five 12.5) -Elevated $true),
        (New-Rec -Task 'B2' -Tool 'voltdir-walk' -Runs (& $five 4)), (New-Rec -Task 'B2' -Tool 'dotnet-enum' -Runs (& $five 6)))
    & $check '(f) 達 10 倍並顯示 12.5 倍' ($t -match '達 10 倍（12\.5 倍）')
    & $check '(f) 未達 10 倍並顯示 1.5 倍' ($t -match '未達 10 倍（1\.5 倍）')

    # 沒有紀錄的任務列為未量測；雜訊警告顯示在備註
    $t = Format-Results @((New-Rec -Task 'B5' -Tool 'voltdir-walk' -Runs (& $five 1) -Noise 'python 占用 1 顆核心'), (New-Rec -Task 'B5' -Tool 'dotnet-enum' -Runs (& $five 2)))
    & $check '無紀錄的任務標示「未量測」' ($t -match '## B1 .*\r?\n\r?\n\*\*未量測')
    & $check '雜訊警告顯示在備註' ($t -match '雜訊警告：python')

    # 缺對照組不丟倍數
    $t = Format-Results @((New-Rec -Tool 'voltdir-walk' -Runs (& $five 1)))
    & $check '只有 voltdir 時標示缺對照組' ($t -match '缺對照組')

    # 紀錄驗證：n 與 runs 不符
    $bad = New-Rec -Tool 'voltdir-walk' -Runs (& $five 1); $bad.n = 9
    $msg = $null; try { Test-Record $bad 'x' } catch { $msg = $_.Exception.Message }
    & $check 'n 與 runs 不符時拒絕' ($msg -and $msg -match 'n=9')

    # (Step 5) 隱私：含使用者名稱的紀錄被拒絕
    $who = if ($env:USERNAME) { $env:USERNAME } else { 'someone' }
    $leak = New-Rec -Tool 'voltdir-walk' -Runs (& $five 1) -Notes "ran by $who" | ConvertTo-Json -Depth 5
    $msg = $null; try { Test-Privacy $leak 'x.json' } catch { $msg = $_.Exception.Message }
    & $check '隱私：含使用者名稱時拒絕' ($null -ne $msg)
    $leak = New-Rec -Tool 'voltdir-walk' -Runs (& $five 1) -Notes 'C:\Users\someone\bench' | ConvertTo-Json -Depth 5
    $msg = $null; try { Test-Privacy $leak 'x.json' } catch { $msg = $_.Exception.Message }
    & $check '隱私：含個人資料夾路徑時拒絕' ($null -ne $msg)
    $ok = $null; try { Test-Privacy (New-Rec -Tool 'voltdir-walk' -Runs (& $five 1) | ConvertTo-Json -Depth 5) 'x.json'; $ok = $true } catch { $ok = $false }
    & $check '隱私：乾淨的紀錄通過' $ok

    if ($script:fails -gt 0) { Write-Host "自我測試失敗：$($script:fails) 項"; exit 1 }
    Write-Host '自我測試全部通過'
}

if ($SelfTest) { Invoke-SelfTest; exit 0 }
if (-not $Out) { throw '需要 -Out <path>（或用 -SelfTest）。' }
$records = @(Read-Records $ResultsDir)
$text = Build-All $records
Set-Content -LiteralPath $Out -Value $text -Encoding UTF8
Write-Host "已產生 $Out（$($records.Count) 筆紀錄）"
