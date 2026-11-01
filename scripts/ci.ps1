<#
.SYNOPSIS
  本機 CI：fmt、clippy、test、release build、授權檢查、基準腳本自我測試、skill 檢查。任一步失敗即以非 0 結束。
  缺少 rustfmt 或 clippy 時給出明確訊息並失敗（不會自行安裝）。
#>
$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root

function Invoke-Step {
    param([string]$Name, [scriptblock]$Body)
    Write-Host "==> $Name"
    & $Body
    if ($LASTEXITCODE -ne 0) {
        Write-Host "失敗：$Name（結束代碼 $LASTEXITCODE）"
        Pop-Location
        exit 1
    }
}

function Assert-Component {
    param([string]$Label, [string]$Sub, [string]$Component)
    cargo $Sub --version *> $null
    if ($LASTEXITCODE -ne 0) {
        Write-Host "缺少 $Label：請執行 'rustup component add $Component' 後再試。本腳本不會自行安裝。"
        Pop-Location
        exit 1
    }
}

try {
    Assert-Component 'rustfmt' 'fmt' 'rustfmt'
    Assert-Component 'clippy' 'clippy' 'clippy'

    Invoke-Step 'cargo fmt --check' { cargo fmt --check }
    Invoke-Step 'cargo clippy -- -D warnings' { cargo clippy -- -D warnings }
    Invoke-Step 'cargo test' { cargo test }
    Invoke-Step 'cargo build --release' { cargo build --release }
    Invoke-Step 'check-licenses 自我測試' { & "$PSScriptRoot\check-licenses.ps1" -SelfTest }
    Invoke-Step 'check-licenses' { & "$PSScriptRoot\check-licenses.ps1" }
    foreach ($script in 'Build-ResultsTable', 'New-BenchTree', 'Measure-Tools') {
        Invoke-Step "benchmarks\$script -SelfTest" { pwsh -NoProfile -File "$root\benchmarks\$script.ps1" -SelfTest }
    }
    Invoke-Step 'skill：SKILL.md 字元數（agent 每次都要讀，上限 6000）' {
        $n = (Get-Content -LiteralPath "$root\skills\voltdir\SKILL.md" -Raw -Encoding UTF8).Length
        if ($n -gt 6000) { Write-Host "SKILL.md 有 $n 個字元，超過 6000；把細節移到 reference.md"; cmd /c exit 1 } else { Write-Host "SKILL.md $n 個字元（上限 6000）"; cmd /c exit 0 }
    }
    Invoke-Step 'skill：外掛設定是合法的 JSON' {
        try {
            $p = Get-Content -LiteralPath "$root\.claude-plugin\plugin.json" -Raw | ConvertFrom-Json
            $m = Get-Content -LiteralPath "$root\.claude-plugin\marketplace.json" -Raw | ConvertFrom-Json
            if ($p.name -ne 'voltdir' -or $m.plugins[0].name -ne 'voltdir') { throw '外掛名稱不是 voltdir' }
            Write-Host '外掛設定正常'; cmd /c exit 0
        } catch { Write-Host "外掛設定錯誤：$($_.Exception.Message)"; cmd /c exit 1 }
    }
    Write-Host 'CI 全部通過'
}
finally { Pop-Location }
