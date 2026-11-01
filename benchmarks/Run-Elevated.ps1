<#
.SYNOPSIS
  系統管理員視窗專用：量整顆磁碟（B1），也可選擇同時量 100 萬檔案的樹（B5 提權版）。
.DESCRIPTION
  在「系統管理員」身分的 PowerShell 視窗執行（本腳本不會也不能替你提權）：
      pwsh -NoProfile -File benchmarks\Run-Elevated.ps1
  它依序做：
  1. 確認目前是系統管理員身分；建立 bench\elevated\（被 git 忽略）與 benchmarks\results\。
  2. 執行 `voltdir info`，把輸出存到 bench\elevated\info.json，確認 mft 引擎可用。
  3. B1：對 -CRoot 量 voltdir-mft 與 -Baselines 列出的對照組，各暖機 1 次後量 -N 次。
     預設對照組是 dotnet-enum-stream（最快的 .NET 寫法）；加上 powershell-gci 會多花很多時間。
  4. 有給 -B5Root 時，再對那棵樹量 voltdir-walk 與同樣的對照組（變體 elevated）。
  結果寫到 benchmarks\results\*.json，用 Build-ResultsTable.ps1 彙整成表。
  量測期間請盡量不要做別的事；有其他程序占用 CPU 時，紀錄會加上雜訊警告。
.EXAMPLE
  pwsh -NoProfile -File benchmarks\Run-Elevated.ps1
  pwsh -NoProfile -File benchmarks\Run-Elevated.ps1 -B5Root D:\voltdir-bench\tree -Baselines dotnet-enum-stream,powershell-gci
#>
[CmdletBinding()]
param(
    [string]$CRoot = 'C:\',
    [string]$B5Root,
    [string[]]$Baselines = @('dotnet-enum-stream'),
    [int]$N = 5
)
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) { throw '請在「系統管理員」身分的 PowerShell 視窗執行。' }
$exe = Join-Path $repo 'target\release\voltdir.exe'
if (-not (Test-Path -LiteralPath $exe)) { throw "找不到 $exe；先在儲存庫執行 cargo build --release。" }
$pwsh = (Get-Command pwsh).Source
$baselineList = ($Baselines -join ',')

$work = Join-Path $repo 'bench\elevated'
New-Item -ItemType Directory -Force $work | Out-Null
New-Item -ItemType Directory -Force (Join-Path $PSScriptRoot 'results') | Out-Null

Write-Host '==> voltdir info'
$info = & $exe info
$info | Set-Content -LiteralPath (Join-Path $work 'info.json') -Encoding UTF8
$info
if (-not ((($info -join '') | ConvertFrom-Json).engines.mft.available)) { throw 'mft 引擎不可用，請看上面的 info 輸出。' }

Write-Host '==> B1（整顆磁碟，系統管理員）'
& $pwsh -NoProfile -File (Join-Path $PSScriptRoot 'Measure-Tools.ps1') -Task B1 -Root $CRoot -Tools "voltdir-mft,$baselineList" -N $N -WorkDir $work
if ($LASTEXITCODE -ne 0) { throw "B1 量測失敗（結束代碼 $LASTEXITCODE）。" }

if ($B5Root) {
    Write-Host '==> B5（100 萬檔案，系統管理員，變體 elevated）'
    & $pwsh -NoProfile -File (Join-Path $PSScriptRoot 'Measure-Tools.ps1') -Task B5 -Root $B5Root -Tools "voltdir-walk,$baselineList" -N $N -Variant elevated -WorkDir $work
    if ($LASTEXITCODE -ne 0) { throw "B5 量測失敗（結束代碼 $LASTEXITCODE）。" }
}
Write-Host "完成。結果在 $(Join-Path $PSScriptRoot 'results')；用 Build-ResultsTable.ps1 產生表格。"
