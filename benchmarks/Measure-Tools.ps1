<#
.SYNOPSIS
  量測「工具本身」回答同一組問題的耗時與峰值記憶體，輸出 voltdir.bench/1 紀錄。
.DESCRIPTION
  每個工具都在獨立程序中執行同一件事：一次走訪 -Root，算出總計、前 20 大資料夾、前 20 大檔案、
  依副檔名的前 10 名（資訊量與 voltdir 的 report --top-files/--top-dirs/--by-ext 對應）：
    voltdir-walk    voltdir.exe report <Root> ... --engine walk
    voltdir-mft     voltdir.exe report <Root> ... --engine mft   （需要系統管理員與整顆磁碟機根目錄）
    powershell-gci  tools\Overview-Gci.ps1       （單一 Get-ChildItem 串流管線）
    dotnet-enum     tools\Overview-DotnetEnum.ps1（FileSystemEnumerable 精簡列舉）
    dotnet-enum-stream  tools\Overview-DotnetEnumStream.ps1（邊走邊彙總、記憶體不隨檔案數成長）
  每個工具先跑 -Warmup 次暖機（不計入），再跑 -N 次量測；耗時為程序的牆鐘時間（含直譯器啟動與
  dotnet-enum 的 Add-Type 編譯），峰值記憶體為該程序的 PeakWorkingSet64（輪詢）。
  量測前後各取樣 5 秒的 CPU，若有其他程序平均占用超過半顆核心，紀錄會寫入 noise_warning（只寫程序名稱）。
  -Cache cold 需要同時給 -Prepare（每次量測前執行的清快取命令，不計時、不暖機），紀錄才會標 cold；
  只要求 cold 卻沒給命令時警告並標 warm，不會硬標 cold。可行做法與未驗證之處見 README。
  支援 B1、B2、B3、B5；B4（比較兩個時間點）沒有工具層級的量法。
  B3 的一個樣本是一整個工作階段：掃一次，再問 5 個不同的問題（規則見 benchmarks/README.md）。
.EXAMPLE
  pwsh -File benchmarks\Measure-Tools.ps1 -Task B5 -Root D:\voltdir-bench\tree -Tools voltdir-walk,dotnet-enum-stream,powershell-gci -N 5
  pwsh -File benchmarks\Measure-Tools.ps1 -SelfTest
#>
[CmdletBinding()]
param(
    [ValidateSet('B1', 'B2', 'B3', 'B4', 'B5')][string]$Task,
    [string]$Root,
    [string[]]$Tools,
    [int]$N = 5,
    [int]$Warmup = 1,
    [ValidateSet('warm', 'cold')][string]$Cache = 'warm',
    [string]$OutDir = (Join-Path $PSScriptRoot 'results'),
    [string]$WorkDir = (Join-Path (Split-Path -Parent $PSScriptRoot) 'bench\tools'),
    [string]$Exe = (Join-Path (Split-Path -Parent $PSScriptRoot) 'target\release\voltdir.exe'),
    [string]$Variant,
    # 冷快取用：每次量測前（不計時）執行的 PowerShell 命令，作用等同 hyperfine 的 --prepare；見 README
    [string]$Prepare,
    [int]$PollMs = 200,
    [switch]$SelfTest
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Schema = 'voltdir.bench/1'
$AllTools = @('voltdir-walk', 'voltdir-mft', 'voltdir-index', 'powershell-gci', 'dotnet-enum', 'dotnet-enum-stream')

function Test-Elevated {
    ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Get-Median([double[]]$v) {
    $s = @($v | Sort-Object); $n = $s.Count
    if ($n % 2 -eq 1) { [double]$s[($n - 1) / 2] } else { ([double]$s[$n / 2 - 1] + [double]$s[$n / 2]) / 2 }
}

function Format-Arg([string]$a) {
    # Start-Process 不會替參數加引號；結尾是反斜線時要加倍，否則會吃掉結尾的引號
    if ($a -match '[\s"]' -or $a.EndsWith('\')) { '"' + ($a -replace '(\\+)$', '$1$1') + '"' } else { $a }
}

# 在獨立程序中執行並量測牆鐘時間與峰值工作集。
function Invoke-Measured([string]$File, [string[]]$ArgList, [string]$StdoutPath, [int]$PollMs) {
    $err = [System.IO.Path]::ChangeExtension($StdoutPath, '.err')
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $p = Start-Process -FilePath $File -ArgumentList ($ArgList | ForEach-Object { Format-Arg $_ }) -WindowStyle Hidden -PassThru `
        -RedirectStandardOutput $StdoutPath -RedirectStandardError $err
    $peak = 0L
    while (-not $p.HasExited) {
        try { $p.Refresh(); if ($p.PeakWorkingSet64 -gt $peak) { $peak = $p.PeakWorkingSet64 } } catch {}
        Start-Sleep -Milliseconds $PollMs
    }
    $p.WaitForExit()
    $sw.Stop()
    [pscustomobject]@{ seconds = $sw.Elapsed.TotalSeconds; peak_mb = $peak / 1MB; exit = $p.ExitCode; stdout = $StdoutPath; stderr = $err }
}

# 每次量測前執行準備命令（不計時）。失敗就中止，避免把「準備失敗」的數字當成冷快取。
function Invoke-Prepare([string]$Command) {
    & (Get-Command pwsh).Source -NoProfile -NonInteractive -Command $Command
    if ($LASTEXITCODE -ne 0) { throw "準備命令失敗（結束代碼 $LASTEXITCODE）：$Command" }
}

function Get-ToolCommand([string]$Tool, [string]$RootPath) {
    $pwsh = (Get-Command pwsh).Source
    switch ($Tool) {
        { $_ -in 'voltdir-walk', 'voltdir-mft' } {
            $engine = if ($Tool -eq 'voltdir-walk') { 'walk' } else { 'mft' }
            @{ File = $Exe; Args = @('report', $RootPath, '--top-files', '20', '--top-dirs', '20', '--by-ext', '10', '--engine', $engine, '-q'); Ok = @(0, 2); Files = { param($j) $j.totals.files } }
        }
        'voltdir-index' {
            # 串流掃描並只寫索引，不寫快照；單獨量測時索引寫在暫存資料夾
            @{ File = $Exe; Args = @('overview', $RootPath, '--save-index', (Join-Path $WorkDir 'single-index.idx'), '--engine', 'walk', '-q'); Ok = @(0, 2); Files = { param($j) $j.totals.files } }
        }
        'powershell-gci' {
            @{ File = $pwsh; Args = @('-NoProfile', '-NonInteractive', '-File', (Join-Path $PSScriptRoot 'tools\Overview-Gci.ps1'), '-Root', $RootPath); Ok = @(0); Files = { param($j) $j.total_files } }
        }
        'dotnet-enum' {
            @{ File = $pwsh; Args = @('-NoProfile', '-NonInteractive', '-File', (Join-Path $PSScriptRoot 'tools\Overview-DotnetEnum.ps1'), '-Root', $RootPath); Ok = @(0); Files = { param($j) $j.total_files } }
        }
        'dotnet-enum-stream' {
            @{ File = $pwsh; Args = @('-NoProfile', '-NonInteractive', '-File', (Join-Path $PSScriptRoot 'tools\Overview-DotnetEnumStream.ps1'), '-Root', $RootPath); Ok = @(0); Files = { param($j) $j.total_files } }
        }
        default { throw "未知的工具 '$Tool'（可用：$($AllTools -join '、')）。" }
    }
}

# B3（掃一次、追問 5 次）的工作階段：回傳要依序執行的命令。
#  - voltdir：overview --save-snapshot（掃描並寫快照與索引），再 5 個不同的追問（走索引）。
#  - voltdir-index：overview --save-index（串流掃描，只寫索引、不寫快照），再 5 個走索引的追問。
#  - powershell-gci、dotnet-enum：每個追問都要重走一次樹，所以同一個命令連跑 6 次。
#  - dotnet-enum-stream：一次走訪就算出所有答案，追問不必再走樹，所以只跑 1 次（追問耗時為 0）。
function Get-SessionPlan([string]$Tool, [string]$RootPath, [string]$Snap) {
    $first = if ($Tool -eq 'voltdir-index') { $null } else { Get-ToolCommand $Tool $RootPath }
    switch ($Tool) {
        { $_ -in 'voltdir-walk', 'voltdir-mft' } {
            $engine = if ($Tool -eq 'voltdir-walk') { 'walk' } else { 'mft' }
            $one = @{ File = $Exe; Args = @('overview', $RootPath, '--save-snapshot', $Snap, '--engine', $engine, '-q'); Ok = @(0, 2); Files = { param($j) $j.totals.files } }
            $q = { param([string[]]$a) @{ File = $Exe; Args = (@('report', '--snapshot', $Snap, '-q') + $a); Ok = @(0, 2); Files = $null } }
            @($one,
                (& $q @('--top-dirs', '20', '--dir-depth', '3')),
                (& $q @('--by-ext', '15')),
                (& $q @('--many-files', '15')),
                (& $q @('--top-files', '20')),
                @{ File = $Exe; Args = @('overview', '--snapshot', $Snap, '--rank', 'logical', '-q'); Ok = @(0, 2); Files = $null })
        }
        'voltdir-index' {
            $idx = "$Snap.idx"
            $q = { param([string[]]$a) @{ File = $Exe; Args = (@('report', '--index', $idx, '-q') + $a); Ok = @(0, 2); Files = $null } }
            @(@{ File = $Exe; Args = @('overview', $RootPath, '--save-index', $idx, '--engine', 'walk', '-q'); Ok = @(0, 2); Files = { param($j) $j.totals.files } },
                (& $q @('--top-dirs', '20', '--dir-depth', '3')),
                (& $q @('--by-ext', '15')),
                (& $q @('--many-files', '15')),
                (& $q @('--top-files', '20')),
                @{ File = $Exe; Args = @('overview', '--index', $idx, '--rank', 'logical', '-q'); Ok = @(0, 2); Files = $null })
        }
        'dotnet-enum-stream' { @($first) }
        default { @($first, $first, $first, $first, $first, $first) }
    }
}

# 取樣 5 秒內各程序的平均 CPU（以核心數計），回傳超過門檻者的「名稱 (x.x 核心)」。
function Get-CpuNoise([int]$Seconds = 5, [double]$Threshold = 0.5) {
    $self = $PID
    $snap = {
        $h = @{}
        foreach ($p in [System.Diagnostics.Process]::GetProcesses()) {
            try { $h[$p.Id] = @{ name = $p.ProcessName; cpu = $p.TotalProcessorTime.TotalSeconds } } catch {}
        }
        $h
    }
    $a = & $snap
    Start-Sleep -Seconds $Seconds
    $b = & $snap
    $busy = @{}
    foreach ($id in $b.Keys) {
        if ($id -eq $self -or -not $a.ContainsKey($id) -or $a[$id].name -ne $b[$id].name) { continue }
        $use = ($b[$id].cpu - $a[$id].cpu) / $Seconds
        if ($use -gt $Threshold) {
            $nm = $b[$id].name
            if (-not $busy.ContainsKey($nm) -or $busy[$nm] -lt $use) { $busy[$nm] = $use }
        }
    }
    @($busy.GetEnumerator() | Sort-Object Value -Descending | ForEach-Object { '{0} ({1:0.0} 核心)' -f $_.Key, $_.Value })
}

function New-ToolRecords {
    param([string]$TaskId, [string]$Tool, [double[]]$Seconds, [double[]]$PeakMb, [bool]$Elevated, [string]$NoiseWarning, [string]$VoltdirVersion, [string]$Notes, [string]$CacheState, [string]$VariantName, [double[]]$Followup = $null)
    $env = [ordered]@{
        os = [System.Environment]::OSVersion.VersionString
        cpu_threads = [System.Environment]::ProcessorCount
        voltdir = $VoltdirVersion
        cache = $CacheState
    }
    $metrics = @(@('tool_seconds', 's', $Seconds), @('peak_ws_mb', 'MB', $PeakMb))
    if ($null -ne $Followup) { $metrics += , @('followup_seconds', 's', $Followup) }
    foreach ($m in $metrics) {
        $runs = [double[]]@($m[2] | ForEach-Object { [math]::Round($_, 2) })
        $o = [ordered]@{
            schema = $Schema; task = $TaskId; tool = $Tool; kind = 'tool'; metric = $m[0]; unit = $m[1]
            n = $runs.Count; median = [math]::Round((Get-Median $runs), 2); runs = $runs; elevated = $Elevated
            noise_warning = $(if ($NoiseWarning) { $NoiseWarning } else { $null }); env = $env; notes = $Notes
        }
        if ($VariantName) { $o.variant = $VariantName }
        [pscustomobject]$o
    }
}

function Write-Record($rec, [string]$Dir) {
    New-Item -ItemType Directory -Force $Dir | Out-Null
    $perm = if ($rec.elevated) { 'elevated' } else { 'standard' }
    $variantPart = if ($rec.PSObject.Properties['variant']) { "-$($rec.variant)" } else { '' }
    $path = Join-Path $Dir "$($rec.task)$variantPart-$($rec.tool)-$($rec.metric)-$perm.json"
    $rec | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $path -Encoding UTF8
    $path
}

function Invoke-Task {
    if ($Task -eq 'B4') { throw "$Task 的工具層級量測尚未定義（支援 B1、B2、B3、B5）；agent 層級的量測見 benchmarks/README.md。" }
    if (-not $Root) { throw '需要 -Root。' }
    if (-not $Tools) { throw '需要 -Tools。' }
    $toolList = @($Tools | ForEach-Object { $_ -split ',' } | Where-Object { $_ })
    foreach ($t in $toolList) { if ($t -notin $AllTools) { throw "未知的工具 '$t'。" } }
    $elevated = Test-Elevated
    if ($Task -eq 'B1' -and -not $elevated) { throw 'B1 是系統管理員情境，但目前不是系統管理員身分。' }
    if ($Task -eq 'B2' -and $elevated) { throw 'B2 是一般權限情境，但目前是系統管理員身分。' }
    if ($WorkDir -match '^[Cc]:') { throw '暫存輸出資料夾不可位於 C 槽。' }
    if (($toolList | Where-Object { $_ -like 'voltdir-*' }) -and -not (Test-Path -LiteralPath $Exe)) { throw "找不到 $Exe；先執行 cargo build --release。" }
    New-Item -ItemType Directory -Force $WorkDir | Out-Null
    $version = if (Test-Path -LiteralPath $Exe) { (& $Exe --version) } else { 'n/a' }
    $notes = "每個工具暖機 $Warmup 次（不計）後量測 $N 次；耗時為程序牆鐘時間（含直譯器啟動）。"
    $cacheState = 'warm'
    $warmup = $Warmup
    if ($Cache -eq 'cold') {
        if ($Prepare) {
            $cacheState = 'cold'; $warmup = 0
            $notes = "每個工具量測 $N 次，每次之前先執行準備命令（不計時）；不暖機。耗時為程序牆鐘時間（含直譯器啟動）。"
        } else {
            Write-Warning '要求 cold 但沒有提供 -Prepare（清除快取的命令），無法做到冷快取；紀錄標為 warm。'
            $notes += ' 要求 cold 但沒有準備命令，實際為 warm。'
        }
    }

    $isB3 = $Task -eq 'B3'
    $snapPath = Join-Path $WorkDir 'b3-snapshot.ndjson'
    $noise = @(Get-CpuNoise)
    $fileCounts = @{}
    $results = @{}
    foreach ($t in $toolList) {
        $plan = @(if ($isB3) { Get-SessionPlan $t $Root $snapPath } else { Get-ToolCommand $t $Root })
        $cmd = $plan[0]
        $out = Join-Path $WorkDir "$Task-$t.json"
        $secs = @(); $peaks = @(); $follow = @()
        for ($i = -$warmup + 1; $i -le $N; $i++) {
            if ($cacheState -eq 'cold') { Invoke-Prepare $Prepare }
            if ($isB3) { Remove-Item -LiteralPath $snapPath, "$snapPath.idx" -Force -ErrorAction SilentlyContinue }
            $total = 0.0; $peak = 0.0; $fu = 0.0
            for ($k = 0; $k -lt $plan.Count; $k++) {
                $c = $plan[$k]
                $o = if ($k -eq 0) { $out } else { Join-Path $WorkDir "$Task-$t-q$k.json" }
                $r = Invoke-Measured $c.File $c.Args $o $PollMs
                if ($r.exit -notin $c.Ok) { throw "$t 失敗（結束代碼 $($r.exit)，第 $($k + 1)/$($plan.Count) 個命令）：$((Get-Content -LiteralPath $r.stderr -Raw -ErrorAction SilentlyContinue))" }
                $total += $r.seconds; if ($r.peak_mb -gt $peak) { $peak = $r.peak_mb }
                if ($k -gt 0) { $fu += $r.seconds }
            }
            if ($isB3) { Remove-Item -LiteralPath $snapPath, "$snapPath.idx" -Force -ErrorAction SilentlyContinue }
            if ($i -le 0) { Write-Host ("{0} 暖機：{1:0.00} s" -f $t, $total); continue }
            $secs += $total; $peaks += $peak; $follow += $fu
            Write-Host ("{0} 第 {1}/{2} 次：{3:0.00} s（追問 {4:0.00} s），峰值 {5:0} MB" -f $t, $i, $N, $total, $fu, $peak)
        }
        $j = Get-Content -LiteralPath $out -Raw -Encoding UTF8 | ConvertFrom-Json
        $fileCounts[$t] = & $cmd.Files $j
        $results[$t] = @{ secs = $secs; peaks = $peaks; follow = $follow }
    }
    $noise += @(Get-CpuNoise)
    $noiseText = if ($noise.Count) { 'CPU 被其他程序占用：' + ((@($noise | Select-Object -Unique)) -join '；') } else { $null }

    $distinct = @($fileCounts.Values | Select-Object -Unique)
    $countNote = if ($distinct.Count -gt 1) { " 警告：各工具算出的檔案數不同（$(($fileCounts.GetEnumerator() | ForEach-Object { '{0}={1}' -f $_.Key, $_.Value }) -join '，')），資料可能在量測期間變動。" } else { " 各工具算出的檔案數一致（$($distinct[0])）。" }
    if ($distinct.Count -gt 1) { Write-Warning $countNote }
    foreach ($t in $toolList) {
        foreach ($rec in New-ToolRecords -TaskId $Task -Tool $t -Seconds $results[$t].secs -PeakMb $results[$t].peaks -Elevated $elevated `
                -NoiseWarning $noiseText -VoltdirVersion $version -Notes ($notes + $countNote + $(if ($isB3) { ' B3：一個樣本是一整個工作階段（掃一次加 5 個追問；沒有追問索引的對照組每個追問重走一次樹，一次走訪算完的對照組追問為 0）。' } else { '' })) -CacheState $cacheState -VariantName $Variant `
                -Followup $(if ($isB3) { $results[$t].follow } else { $null })) {
            Write-Host ("寫入 " + (Write-Record $rec $OutDir))
        }
    }
}

function Invoke-SelfTest {
    $script:fails = 0
    $check = {
        param([string]$name, [bool]$ok)
        if ($ok) { Write-Host "PASS: $name" } else { Write-Host "FAIL: $name"; $script:fails++ }
    }
    # 中位數與離群值
    & $check '中位數忽略離群值（1,1,1,1,100 → 1）' ((Get-Median @(1, 100, 1, 1, 1)) -eq 1)
    & $check '偶數筆取中間兩個的平均' ((Get-Median @(1, 2, 3, 10)) -eq 2.5)

    # 真的量一個睡 0.05 秒的假工具
    $work = Join-Path (Split-Path -Parent $PSScriptRoot) 'target\fixtures\measure-selftest'
    New-Item -ItemType Directory -Force $work | Out-Null
    $pwsh = (Get-Command pwsh).Source
    $secs = @(); $peaks = @()
    foreach ($i in 1..5) {
        $r = Invoke-Measured $pwsh @('-NoProfile', '-NonInteractive', '-Command', 'Start-Sleep -Milliseconds 50') (Join-Path $work 'fake.out') 50
        $secs += $r.seconds; $peaks += $r.peak_mb
    }
    & $check '假工具的耗時大於 0.04 秒且合理' ((($secs | Measure-Object -Minimum).Minimum -gt 0.04) -and (($secs | Measure-Object -Maximum).Maximum -lt 30))
    & $check '假工具的峰值記憶體大於 0' ((($peaks | Measure-Object -Minimum).Minimum) -gt 0)

    $recs = @(New-ToolRecords -TaskId 'B5' -Tool 'powershell-gci' -Seconds $secs -PeakMb $peaks -Elevated (Test-Elevated) -NoiseWarning $null -VoltdirVersion 'selftest' -Notes 'selftest' -CacheState 'warm' -VariantName $null)
    & $check '每個工具產生兩筆紀錄（tool_seconds 與 peak_ws_mb）' ($recs.Count -eq 2 -and $recs[0].metric -eq 'tool_seconds' -and $recs[1].metric -eq 'peak_ws_mb')
    & $check 'n 等於實際次數' ($recs[0].n -eq 5 -and @($recs[0].runs).Count -eq 5)
    & $check 'elevated 由目前身分自動填入' ($recs[0].elevated -eq (Test-Elevated))
    $json = $recs | ConvertTo-Json -Depth 6
    $leak = $false
    foreach ($bad in @($env:USERNAME, $env:COMPUTERNAME)) { if ($bad -and $bad.Length -ge 3 -and $json.IndexOf($bad, [StringComparison]::OrdinalIgnoreCase) -ge 0) { $leak = $true } }
    & $check 'env 與紀錄不含主機名稱或使用者名稱' (-not $leak)
    & $check '紀錄不含 C:\Users 路徑' ($json -notmatch '(?i)[a-z]:\\{1,2}users')

    # 產生的檔案能通過 Task 1 的彙整
    $res = Join-Path $work 'results'
    if (Test-Path -LiteralPath $res) { Get-ChildItem -LiteralPath $res -File | Remove-Item -Force }
    foreach ($r in $recs) { Write-Record $r $res | Out-Null }
    $md = Join-Path $work 'out.md'
    $null = & $pwsh -NoProfile -File (Join-Path $PSScriptRoot 'Build-ResultsTable.ps1') -ResultsDir $res -Out $md 2>&1
    & $check '紀錄通過 Build-ResultsTable 的驗證並產生表' (($LASTEXITCODE -eq 0) -and (Test-Path -LiteralPath $md) -and ((Get-Content -LiteralPath $md -Raw) -match 'powershell-gci'))

    # 冷快取：準備命令每次量測前都會被執行；失敗會中止；cache 欄位能記錄 cold
    $marker = Join-Path $work 'prepare.log'
    if (Test-Path -LiteralPath $marker) { Remove-Item -LiteralPath $marker -Force }
    foreach ($i in 1..3) { Invoke-Prepare "Add-Content -LiteralPath '$marker' -Value x" }
    & $check '準備命令每次都被執行（3 次）' ((Get-Content -LiteralPath $marker).Count -eq 3)
    $msg = $null; try { Invoke-Prepare 'exit 7' } catch { $msg = $_.Exception.Message }
    & $check '準備命令失敗時中止' ($msg -match '準備命令失敗')
    $cold = @(New-ToolRecords -TaskId 'B5' -Tool 'dotnet-enum' -Seconds $secs -PeakMb $peaks -Elevated (Test-Elevated) -NoiseWarning $null -VoltdirVersion 'selftest' -Notes 'selftest' -CacheState 'cold' -VariantName $null)
    & $check '紀錄能記錄 cache=cold' ($cold[0].env.cache -eq 'cold')
    $res5 = Join-Path $work 'results-cold'
    if (Test-Path -LiteralPath $res5) { Get-ChildItem -LiteralPath $res5 -File | Remove-Item -Force }
    foreach ($r in $cold) { Write-Record $r $res5 | Out-Null }
    $null = & $pwsh -NoProfile -File (Join-Path $PSScriptRoot 'Build-ResultsTable.ps1') -ResultsDir $res5 -Out (Join-Path $work 'cold.md') 2>&1
    & $check 'cold 紀錄通過 Build-ResultsTable' ($LASTEXITCODE -eq 0)

    # B3、B4 與 C 槽暫存的防呆
    $msg = $null; try { $script:Task = 'B4'; Invoke-Task } catch { $msg = $_.Exception.Message }
    & $check 'B4 尚未定義時明確拒絕' ($msg -match '尚未定義')
    # B3 的工作階段計畫：voltdir 是 1 次掃描加 5 個追問；沒有索引的對照組同一個命令跑 6 次；一次走訪算完的對照組只跑 1 次
    $script:Exe = 'voltdir.exe'
    $plan = @(Get-SessionPlan 'voltdir-walk' 'X:
' 'X:\s.ndjson')
    & $check 'B3 voltdir 計畫：1 次 overview --save-snapshot 加 5 個追問' (($plan.Count -eq 6) -and ($plan[0].Args -contains '--save-snapshot') -and ($plan[1..5] | ForEach-Object { $_.Args -contains '--snapshot' }) -notcontains $false)
    & $check 'B3 追問互不相同' ((@($plan[1..5] | ForEach-Object { $_.Args -join ' ' }) | Select-Object -Unique).Count -eq 5)
    & $check 'B3 沒有索引的對照組每個追問重走一次樹（6 次）' (@(Get-SessionPlan 'dotnet-enum' 'X:
' 'X:\s').Count -eq 6)
    $ip = @(Get-SessionPlan 'voltdir-index' 'X:' 'X:\s')
    & $check 'B3 voltdir-index 計畫：1 次 --save-index 加 5 個走 --index 的追問，沒有快照' (($ip.Count -eq 6) -and ($ip[0].Args -contains '--save-index') -and ($ip[0].Args -notcontains '--save-snapshot') -and (($ip[1..5] | ForEach-Object { $_.Args -contains '--index' }) -notcontains $false))
    & $check 'B3 一次走訪算完的對照組只走 1 次' (@(Get-SessionPlan 'dotnet-enum-stream' 'X:
' 'X:\s').Count -eq 1)
    $fr = @(New-ToolRecords -TaskId 'B3' -Tool 'voltdir-walk' -Seconds @(1, 1, 1, 1, 1) -PeakMb @(10, 10, 10, 10, 10) -Elevated $false -NoiseWarning $null -VoltdirVersion 'selftest' -Notes 'selftest' -CacheState 'warm' -VariantName $null -Followup @(0.1, 0.1, 0.1, 0.1, 0.1))
    & $check 'B3 紀錄多一個 followup_seconds 指標' (($fr.Count -eq 3) -and ($fr.metric -contains 'followup_seconds'))

    # 雜訊偵測：自己燒一顆核心 2 秒，應被偵測到（用一個子程序，名稱為 pwsh）
    $burner = Start-Process $pwsh -ArgumentList @('-NoProfile', '-NonInteractive', '-Command', '$e=[Diagnostics.Stopwatch]::StartNew(); while($e.Elapsed.TotalSeconds -lt 4){ 1+1 | Out-Null }') -WindowStyle Hidden -PassThru
    Start-Sleep -Milliseconds 300
    $noise = @(Get-CpuNoise -Seconds 2 -Threshold 0.5)
    $burner.WaitForExit()
    & $check '雜訊偵測抓得到占用一顆核心的程序' (@($noise | Where-Object { $_ -match 'pwsh' }).Count -ge 1)

    if ($script:fails -gt 0) { Write-Host "自我測試失敗：$($script:fails) 項"; exit 1 }
    Write-Host '自我測試全部通過'
}

if ($SelfTest) { Invoke-SelfTest; exit 0 }
if (-not $Task) { throw '需要 -Task（或用 -SelfTest）。' }
Invoke-Task
