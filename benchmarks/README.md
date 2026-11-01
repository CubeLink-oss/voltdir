# 可重現的基準（`benchmarks\`）

這個資料夾的腳本讓你在自己的機器上重跑 README 的數字：產生測試資料、量 voltdir 與 PowerShell／.NET 對照組的耗時與峰值記憶體、把結果彙整成表。上一次的結果摘要在 [`RESULTS.md`](RESULTS.md)。

**目的是誠實地量出差距，不是替 voltdir 製造數字。** 對照組取最強的，比較慢的照實顯示。

## 需要什麼

- Windows、PowerShell 7（`pwsh`）。
- 已建置的 `target\release\voltdir.exe`（在儲存庫根目錄執行 `cargo build --release`）。
- 一個**不在 C 槽**的位置放 100 萬檔案的測試資料（約 10 GB）。
- 不需要安裝其他東西：`dotnet-enum` 與 `dotnet-enum-stream` 對照組用 PowerShell 內建的 `Add-Type` 編譯一小段 C#。

## 檔案

| 檔案 | 作用 |
|---|---|
| `New-BenchTree.ps1` | 產生 100 萬個檔案的決定性資料夾（固定種子），並寫出標準答案 `answers.json` |
| `Measure-Tools.ps1` | 量每個工具回答同一組問題的耗時與峰值記憶體，寫出結果紀錄；含雜訊偵測 |
| `Run-Elevated.ps1` | 系統管理員視窗用：量整顆磁碟（B1），可選擇同時量 100 萬檔案的樹 |
| `Build-ResultsTable.ps1` | 把結果紀錄彙整成 Markdown 表，並強制執行下面的量法規則 |
| `tools\Overview-*.ps1` | 三個對照組：`Get-ChildItem` 管線、精簡 .NET 列舉、邊走邊彙總的 .NET 串流 |
| `RESULTS.md` | 上一次量測的結果摘要 |

每支腳本都有 `-SelfTest`，不需要測試資料就能確認腳本本身正常。

## 重跑步驟

```powershell
# 0. 自我測試
pwsh -File benchmarks\New-BenchTree.ps1 -SelfTest
pwsh -File benchmarks\Measure-Tools.ps1 -SelfTest
pwsh -File benchmarks\Build-ResultsTable.ps1 -SelfTest

# 1. 產生 100 萬檔案的資料夾（約 2 分鐘；不能放在 C 槽；路徑已存在會拒絕）
pwsh -File benchmarks\New-BenchTree.ps1 -Root D:\voltdir-bench

# 2. 一般權限：100 萬檔案的樹（B5），以及掃一次加 5 個追問（B3）
pwsh -File benchmarks\Measure-Tools.ps1 -Task B5 -Root D:\voltdir-bench\tree -Tools voltdir-walk,dotnet-enum-stream,dotnet-enum,powershell-gci -N 5
pwsh -File benchmarks\Measure-Tools.ps1 -Task B3 -Root D:\voltdir-bench\tree -Tools voltdir-index,dotnet-enum-stream -N 5

# 3. 一般權限：整顆磁碟（B2；每次可能要一分鐘以上）
pwsh -File benchmarks\Measure-Tools.ps1 -Task B2 -Root C:\ -Tools voltdir-walk,dotnet-enum-stream -N 5

# 4. 系統管理員視窗：整顆磁碟（B1）
pwsh -NoProfile -File benchmarks\Run-Elevated.ps1

# 5. 產生結果表
pwsh -File benchmarks\Build-ResultsTable.ps1 -ResultsDir benchmarks\results -Out results.md
```

結果紀錄寫在 `benchmarks\results\`（被 git 忽略），大型暫存輸出寫在 `bench\`（也被忽略）。

## 量法規則（由 `Build-ResultsTable.ps1` 強制執行）

1. 每格至少 5 次，取中位數；少於 5 次顯示「樣本不足」。
2. 倍數只在同一個量測項目內計算（耗時、峰值記憶體、追問耗時），並標明量的是什麼。
3. 對照組取最強的：耗時取最快、記憶體取最低。
4. voltdir 比較慢時照實顯示，例如「voltdir 較慢（0.78 倍）」。
5. 系統管理員與一般權限、暖快取與冷快取的紀錄不得混在同一格。
6. 結果含使用者名稱、主機名稱或個人資料夾路徑時拒絕。

## 注意事項

- **量的是什麼：** 耗時是程序從啟動到結束的牆鐘時間。PowerShell 對照組包含 `pwsh` 啟動（約 0.3 秒），`dotnet-enum` 另含 `Add-Type` 編譯（約 1 秒）；大資料夾上可以忽略，小資料夾上不行。
- **峰值記憶體：** 程序的 `PeakWorkingSet64`，每 200 毫秒輪詢一次。PowerShell 對照組的數字大多是 `pwsh` 宿主本身。
- **雜訊：** 量測前後各取樣 5 秒 CPU，其他程序平均占用超過半顆核心時，紀錄會寫入程序名稱作為警告。看到警告時，請關掉那些程序後重量。
- **冷快取：** 預設是暖快取。要量冷快取，請用 `-Cache cold -Prepare '<清快取的命令>'`，腳本會在每次量測前執行那個命令；沒給命令時不會標成 cold。Windows 上清檔案系統快取需要系統管理員，做法請自行驗證。
- **每次量測後**，腳本會比對各工具算出的檔案數，不一致時寫入備註（量整顆磁碟時，磁碟內容在量測期間本來就會變）。
