<#
.SYNOPSIS
  產生 B5 用的大型決定性檔案樹，並同時寫出標準答案 answers.json（答案在建立過程中累計，不另外掃描）。
.DESCRIPTION
  在 <Root>\tree\ 產生 -Files 個檔案，答案寫在 <Root>\answers.json（agent 不應看到這個檔案）。
  樹的形狀：
  - 五個頂層資料夾 pkgs、build、logs、media、misc，各有若干「葉資料夾」（約每 200 個檔案一個）。
  - 一般檔案大小為 4096 的 1、2、3 倍（依序號循環），副檔名依序號循環（.js 的權重最大）。
  - media\big\ 內 12 個大檔 big_00.bin ... big_11.bin（32 MiB 起，每個多 4 MiB）。
  - misc\empties\ 內 1,000 個空檔案；misc\empty_dirs\ 內 50 個空資料夾。
  - 沒有隨機性（-Seed 只決定檔案內容的位元組），所以同樣參數永遠得到同樣的樹與答案。
  標準答案欄位：total_files、total_bytes、top5_dirs、top5_files、top_extension、empty_files、empty_dirs。
  「資料夾大小」指含底下所有內容的合計（不含掃描根自己）；路徑相對於 tree\。
  拒絕 C 槽路徑與已存在的路徑。
.EXAMPLE
  pwsh -File benchmarks\New-BenchTree.ps1 -Root D:\voltdir-bench
  pwsh -File benchmarks\New-BenchTree.ps1 -SelfTest
#>
[CmdletBinding()]
param(
    [string]$Root,
    [long]$Files = 1000000,
    [int]$Seed = 7,
    [switch]$SelfTest
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

Add-Type -Language CSharp -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.IO;
using System.Threading.Tasks;

public static class BenchGen
{
    public static readonly string[] Groups = { "pkgs", "build", "logs", "media", "misc" };
    static readonly double[] Weights = { 0.40, 0.25, 0.15, 0.12, 0.08 };
    static readonly string[] Exts = { ".js", ".js", ".dll", ".log", ".txt", ".dat", ".png" };
    const long MiB = 1L << 20;
    public const int BigFiles = 12;
    public const int EmptyFiles = 1000;
    public const int EmptyDirs = 50;

    public class Result
    {
        public long TotalFiles, TotalBytes, EmptyFileCount, EmptyDirCount, Dirs;
        public string[] Top5Dirs, Top5Files;
        public string TopExtension;
    }

    sealed class Leaf { public int Group; public string Dir; public long First; public long Count; }

    static byte[] Pattern(int seed, int len)
    {
        var b = new byte[len];
        uint x = (uint)seed * 2654435761u + 12345u;
        for (int i = 0; i < len; i++) { x = x * 1664525u + 1013904223u; b[i] = (byte)(x >> 24); }
        return b;
    }

    public static Result Generate(string tree, long files, int seed)
    {
        long fixedFiles = BigFiles + EmptyFiles;
        if (files < fixedFiles + 100) throw new ArgumentException("Files must be at least " + (fixedFiles + 100));
        long regular = files - fixedFiles;
        int leafTotal = (int)((files + 199) / 200);

        // 每組的檔案配額與葉資料夾數（每組至少 2 個葉資料夾，pkgs 吃掉餘數）
        var quota = new long[5];
        var leaves = new int[5];
        long qSum = 0; int lSum = 0;
        for (int g = 1; g < 5; g++)
        {
            quota[g] = (long)Math.Round(regular * Weights[g]);
            leaves[g] = Math.Max(2, (int)(leafTotal * Weights[g]));
            qSum += quota[g]; lSum += leaves[g];
        }
        quota[0] = regular - qSum;
        leaves[0] = leafTotal - lSum;
        if (leaves[0] < 2 || quota[0] < leaves[0]) throw new ArgumentException("Files is too small for this layout");

        var list = new List<Leaf>();
        long next = 0;
        for (int g = 0; g < 5; g++)
        {
            long per = quota[g] / leaves[g], extra = quota[g] % leaves[g];
            for (int j = 0; j < leaves[g]; j++)
            {
                long c = per + (j < extra ? 1 : 0);
                list.Add(new Leaf { Group = g, Dir = Groups[g] + "\\d" + j.ToString("D5"), First = next, Count = c });
                next += c;
            }
        }

        Directory.CreateDirectory(tree);
        foreach (var g in Groups) Directory.CreateDirectory(Path.Combine(tree, g));

        var extBytes = new Dictionary<string, long>();
        var dirBytes = new Dictionary<string, long>();   // 每個資料夾（相對路徑）的合計
        var gate = new object();
        long totalBytes = 0;

        var buf = Pattern(seed, 3 * 4096);
        var opt = new ParallelOptions { MaxDegreeOfParallelism = Environment.ProcessorCount };
        Parallel.For(0, list.Count, opt, () => new Dictionary<string, long>(), (li, state, local) =>
        {
            var leaf = list[li];
            string dir = Path.Combine(tree, leaf.Dir);
            Directory.CreateDirectory(dir);
            long bytes = 0;
            for (long k = 0; k < leaf.Count; k++)
            {
                long g = leaf.First + k;
                string ext = Exts[g % 7];
                int size = 4096 * (int)(1 + g % 3);
                using (var fs = new FileStream(Path.Combine(dir, "f" + g.ToString("D7") + ext), FileMode.CreateNew, FileAccess.Write, FileShare.None, 4096))
                    fs.Write(buf, 0, size);
                bytes += size;
                long cur; local.TryGetValue(ext, out cur); local[ext] = cur + size;
            }
            lock (gate) { dirBytes[leaf.Dir] = bytes; totalBytes += bytes; }
            return local;
        }, local =>
        {
            lock (gate) foreach (var kv in local) { long cur; extBytes.TryGetValue(kv.Key, out cur); extBytes[kv.Key] = cur + kv.Value; }
        });

        // media\big：12 個大檔
        string bigDir = Path.Combine(tree, "media", "big");
        Directory.CreateDirectory(bigDir);
        var bigNames = new List<KeyValuePair<long, string>>();
        long bigTotal = 0;
        var chunk = Pattern(seed + 1, (int)MiB);
        for (int i = 0; i < BigFiles; i++)
        {
            long size = (32 + 4L * i) * MiB;
            string name = "big_" + i.ToString("D2") + ".bin";
            using (var fs = new FileStream(Path.Combine(bigDir, name), FileMode.CreateNew, FileAccess.Write, FileShare.None, 1 << 20))
                for (long w = 0; w < size; w += MiB) fs.Write(chunk, 0, (int)MiB);
            bigNames.Add(new KeyValuePair<long, string>(size, "media\\big\\" + name));
            bigTotal += size;
        }
        { long cur; extBytes.TryGetValue(".bin", out cur); extBytes[".bin"] = cur + bigTotal; }
        totalBytes += bigTotal;
        dirBytes["media\\big"] = bigTotal;

        // misc：空檔案與空資料夾
        string empties = Path.Combine(tree, "misc", "empties");
        Directory.CreateDirectory(empties);
        for (int i = 0; i < EmptyFiles; i++)
            using (new FileStream(Path.Combine(empties, "e" + i.ToString("D4") + ".dat"), FileMode.CreateNew, FileAccess.Write, FileShare.None, 1)) { }
        dirBytes["misc\\empties"] = 0;
        string emptyDirs = Path.Combine(tree, "misc", "empty_dirs");
        Directory.CreateDirectory(emptyDirs);
        for (int i = 0; i < EmptyDirs; i++) Directory.CreateDirectory(Path.Combine(emptyDirs, "x" + i.ToString("D2")));
        dirBytes["misc\\empty_dirs"] = 0;

        // 資料夾合計：頂層組 = 底下所有項目的總和
        var groupBytes = new long[5];
        foreach (var kv in dirBytes)
            for (int g = 0; g < 5; g++)
                if (kv.Key.StartsWith(Groups[g] + "\\", StringComparison.Ordinal)) groupBytes[g] += kv.Value;
        var all = new List<KeyValuePair<string, long>>(dirBytes);
        for (int g = 0; g < 5; g++) all.Add(new KeyValuePair<string, long>(Groups[g], groupBytes[g]));
        all.Sort((a, b) => { int c = b.Value.CompareTo(a.Value); return c != 0 ? c : string.CompareOrdinal(a.Key, b.Key); });
        for (int i = 0; i < 5; i++)
            if (all[i].Value == all[i + 1].Value)
                throw new InvalidOperationException("tie in directory sizes at rank " + (i + 1) + ": " + all[i].Key + " / " + all[i + 1].Key);

        bigNames.Sort((a, b) => b.Key.CompareTo(a.Key));
        string topExt = null; long topBytes = -1, second = -1;
        foreach (var kv in extBytes)
        {
            if (kv.Value > topBytes) { second = topBytes; topBytes = kv.Value; topExt = kv.Key; }
            else if (kv.Value > second) second = kv.Value;
        }
        if (topBytes == second) throw new InvalidOperationException("tie in extension sizes");

        var r = new Result
        {
            TotalFiles = files, TotalBytes = totalBytes, EmptyFileCount = EmptyFiles, EmptyDirCount = EmptyDirs,
            Dirs = all.Count + 1, TopExtension = topExt, Top5Dirs = new string[5], Top5Files = new string[5]
        };
        for (int i = 0; i < 5; i++) { r.Top5Dirs[i] = all[i].Key; r.Top5Files[i] = bigNames[i].Value; }
        return r;
    }
}
'@

function New-BenchTree([string]$RootPath, [long]$FileCount, [int]$SeedValue) {
    $full = [System.IO.Path]::GetFullPath($RootPath)
    if ($full -match '^[Cc]:') { throw '輸出資料夾不可位於 C 槽。' }
    if (Test-Path -LiteralPath $full) { throw "路徑已存在：$full（不會覆寫）。" }
    New-Item -ItemType Directory -Path $full | Out-Null
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $r = [BenchGen]::Generate((Join-Path $full 'tree'), $FileCount, $SeedValue)
    $sw.Stop()
    $answers = [ordered]@{
        total_files   = $r.TotalFiles
        total_bytes   = $r.TotalBytes
        top5_dirs     = @($r.Top5Dirs)
        top5_files    = @($r.Top5Files)
        top_extension = $r.TopExtension
        empty_files   = $r.EmptyFileCount
        empty_dirs    = $r.EmptyDirCount
    }
    $answers | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $full 'answers.json') -Encoding UTF8
    [pscustomobject]@{ Root = $full; Seconds = [math]::Round($sw.Elapsed.TotalSeconds, 1); Answers = $answers }
}

function Invoke-SelfTest {
    $script:fails = 0
    $check = {
        param([string]$name, [bool]$ok)
        if ($ok) { Write-Host "PASS: $name" } else { Write-Host "FAIL: $name"; $script:fails++ }
    }
    $base = Join-Path (Split-Path -Parent $PSScriptRoot) 'target\fixtures'
    New-Item -ItemType Directory -Force $base | Out-Null
    $root = Join-Path $base ('benchtree-selftest-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
    try {
        $res = New-BenchTree $root 2000 7
        $ans = $res.Answers
        $tree = Join-Path $root 'tree'

        # 獨立計算：只用 .NET 列舉，不用產生器的任何結果
        $count = 0L; $sum = 0L; $zero = 0L
        $ext = @{}; $dirSum = @{}; $fileLens = [System.Collections.Generic.List[object]]::new()
        $prefix = $tree.TrimEnd('\') + '\'
        foreach ($f in [System.IO.Directory]::EnumerateFiles($tree, '*', [System.IO.SearchOption]::AllDirectories)) {
            $len = ([System.IO.FileInfo]$f).Length
            $count++; $sum += $len; if ($len -eq 0) { $zero++ }
            $e = [System.IO.Path]::GetExtension($f).ToLowerInvariant()
            $ext[$e] = [long]($(if ($ext.ContainsKey($e)) { $ext[$e] } else { 0 })) + $len
            $rel = $f.Substring($prefix.Length)
            $fileLens.Add([pscustomobject]@{ rel = $rel; len = $len })
            $d = [System.IO.Path]::GetDirectoryName($rel)
            while ($d) {
                $dirSum[$d] = [long]($(if ($dirSum.ContainsKey($d)) { $dirSum[$d] } else { 0 })) + $len
                $d = [System.IO.Path]::GetDirectoryName($d)
            }
        }
        $emptyDirs = 0
        foreach ($d in [System.IO.Directory]::EnumerateDirectories($tree, '*', [System.IO.SearchOption]::AllDirectories)) {
            if (-not [System.IO.Directory]::EnumerateFileSystemEntries($d).GetEnumerator().MoveNext()) { $emptyDirs++ }
        }
        $topExt = ($ext.GetEnumerator() | Sort-Object Value -Descending | Select-Object -First 1).Key
        $topFiles = @($fileLens | Sort-Object @{ e = 'len'; Descending = $true } | Select-Object -First 5 | ForEach-Object { $_.rel })
        $topDirs = @($dirSum.GetEnumerator() | Sort-Object @{ e = { $_.Value }; Descending = $true }, @{ e = { $_.Key } } | Select-Object -First 5 | ForEach-Object { $_.Key })

        & $check 'total_files 與獨立計數相同' ($count -eq $ans.total_files -and $count -eq 2000)
        & $check 'total_bytes 與獨立加總相同' ($sum -eq $ans.total_bytes)
        & $check 'empty_files 與獨立計數相同' ($zero -eq $ans.empty_files -and $zero -eq 1000)
        & $check 'empty_dirs 與獨立計數相同' ($emptyDirs -eq $ans.empty_dirs -and $emptyDirs -eq 50)
        & $check 'top_extension 與獨立計算相同' ($topExt -eq $ans.top_extension)
        & $check '前 5 大檔案與獨立計算相同（含順序）' ((($topFiles -join '|') -eq ($ans.top5_files -join '|')))
        & $check '前 5 大資料夾與獨立計算相同（含順序）' ((($topDirs -join '|') -eq ($ans.top5_dirs -join '|')))
        & $check '所有檔案大小為 4096 的倍數' (-not ($fileLens | Where-Object { $_.len % 4096 -ne 0 }))
        & $check 'answers.json 寫在 tree 之外' ((Test-Path (Join-Path $root 'answers.json')) -and -not (Test-Path (Join-Path $tree 'answers.json')))

        $msg = $null; try { New-BenchTree $root 2000 7 | Out-Null } catch { $msg = $_.Exception.Message }
        & $check '拒絕已存在的路徑' ($msg -match '已存在')
        $msg = $null; try { New-BenchTree 'C:\voltdir-bench-should-not-exist' 2000 7 | Out-Null } catch { $msg = $_.Exception.Message }
        & $check '拒絕 C 槽路徑' ($msg -match 'C 槽' -and -not (Test-Path 'C:\voltdir-bench-should-not-exist'))
    } finally {
        if ((Test-Path -LiteralPath $root) -and $root.StartsWith($base)) { Remove-Item -LiteralPath $root -Recurse -Force }
    }
    if ($script:fails -gt 0) { Write-Host "自我測試失敗：$($script:fails) 項"; exit 1 }
    Write-Host '自我測試全部通過'
}

if ($SelfTest) { Invoke-SelfTest; exit 0 }
if (-not $Root) { throw '需要 -Root <path>（或用 -SelfTest）。' }
$r = New-BenchTree $Root $Files $Seed
Write-Host ("已產生 {0:N0} 個檔案於 {1}\tree，耗時 {2} 秒；答案：{1}\answers.json" -f $Files, $r.Root, $r.Seconds)
