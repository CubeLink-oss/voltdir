<#
.SYNOPSIS
  檢查 cargo metadata 列出的所有套件授權是否都在允許清單內。
.DESCRIPTION
  授權表達式以 OR 組合時，只要任一選項（其中的 AND 需全部允許）在清單內即通過。
  授權欄位為空、或沒有任何選項在清單內者視為失敗。
  -SelfTest 以假資料驗證：含 GPL 套件必須失敗，含 MIT/Apache 套件必須通過。
#>
[CmdletBinding()]
param(
    [switch]$SelfTest
)

$ErrorActionPreference = 'Stop'

$Allowed = @('MIT', 'Apache-2.0', 'BSD-2-Clause', 'BSD-3-Clause', 'ISC', 'Unlicense', 'Zlib')

function Test-LicenseExpression {
    param([string]$Expression)
    if ([string]::IsNullOrWhiteSpace($Expression)) { return $false }
    # 舊式 "MIT/Apache-2.0" 視同 OR；括號直接去除（本專案不使用巢狀運算）
    $normalized = ($Expression -replace '[()]', ' ') -replace '/', ' OR '
    foreach ($alt in ($normalized -split '\s+OR\s+')) {
        $terms = $alt -split '\s+AND\s+' | ForEach-Object { ($_ -replace '\s+WITH\s+.*$', '').Trim() } | Where-Object { $_ }
        if ($terms.Count -gt 0 -and -not ($terms | Where-Object { $Allowed -cnotcontains $_ })) {
            return $true
        }
    }
    return $false
}

function Get-LicenseViolations {
    param($Metadata)
    $bad = @()
    foreach ($pkg in $Metadata.packages) {
        if (-not (Test-LicenseExpression $pkg.license)) {
            $lic = if ([string]::IsNullOrWhiteSpace($pkg.license)) { '(空)' } else { $pkg.license }
            $bad += "$($pkg.name) $($pkg.version): $lic"
        }
    }
    return $bad
}

if ($SelfTest) {
    $fail = 0
    $badData = '{"packages":[{"name":"ok-mit","version":"1.0.0","license":"MIT"},{"name":"evil-gpl","version":"2.0.0","license":"GPL-3.0-or-later"}]}' | ConvertFrom-Json
    $goodData = '{"packages":[{"name":"a","version":"1.0.0","license":"MIT OR Apache-2.0"},{"name":"b","version":"1.0.0","license":"Apache-2.0"},{"name":"c","version":"1.0.0","license":"MIT/Unlicense"},{"name":"d","version":"1.0.0","license":"(MIT OR GPL-3.0) AND ISC"}]}' | ConvertFrom-Json
    $emptyData = '{"packages":[{"name":"nolic","version":"0.1.0","license":null}]}' | ConvertFrom-Json
    $gplOr = '{"packages":[{"name":"gpl-and-mit","version":"1.0.0","license":"GPL-3.0 AND MIT"}]}' | ConvertFrom-Json

    $v = @(Get-LicenseViolations $badData)
    if ($v.Count -eq 1 -and $v[0] -like 'evil-gpl*') { Write-Host 'PASS: GPL 套件被拒絕' } else { Write-Host "FAIL: GPL 資料應失敗，實際 $($v -join '; ')"; $fail++ }
    $v = @(Get-LicenseViolations $goodData)
    if ($v.Count -eq 0) { Write-Host 'PASS: MIT/Apache 套件通過' } else { Write-Host "FAIL: 良好資料不應失敗: $($v -join '; ')"; $fail++ }
    $v = @(Get-LicenseViolations $emptyData)
    if ($v.Count -eq 1) { Write-Host 'PASS: 空授權被拒絕' } else { Write-Host 'FAIL: 空授權應失敗'; $fail++ }
    $v = @(Get-LicenseViolations $gplOr)
    if ($v.Count -eq 1) { Write-Host 'PASS: GPL AND MIT 被拒絕' } else { Write-Host 'FAIL: GPL AND MIT 應失敗'; $fail++ }

    if ($fail -gt 0) { Write-Host "自我測試失敗 ($fail)"; exit 1 }
    Write-Host '自我測試全部通過'
    exit 0
}

$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    $json = cargo metadata --format-version 1 --locked 2>$null
    if ($LASTEXITCODE -ne 0) { Write-Host 'cargo metadata 失敗'; exit 1 }
    $meta = $json | ConvertFrom-Json
    $violations = @(Get-LicenseViolations $meta)
    Write-Host "已檢查 $(@($meta.packages).Count) 個套件（含本專案）"
    if ($violations.Count -gt 0) {
        Write-Host '以下套件授權不在允許清單內：'
        $violations | ForEach-Object { Write-Host "  $_" }
        exit 1
    }
    Write-Host '授權檢查通過'
    exit 0
}
finally { Pop-Location }
