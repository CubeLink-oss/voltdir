<#
.SYNOPSIS
  Builds a deterministic demo workspace with known answers, for trying voltdir
  and for the ignored integration tests (set VOLTDIR_DEMO to its folder).

.DESCRIPTION
  Creates two sibling trees under -Root:
    monday\  a developer workspace (~1 GB, ~30,000 files)
    friday\  the same workspace after "a few days of work"

  Every file size is a multiple of 4096 bytes (or zero), so logical size and
  on-disk size agree on an uncompressed NTFS volume and every tool must give
  the same numbers. Nothing here depends on the machine it runs on.
  Writes answers.json next to the trees: the ground truth to score against.

.EXAMPLE
  .\scripts\New-DemoWorkspace.ps1 -Root D:\voltdir-demo
#>
param(
    [Parameter(Mandatory)][string]$Root
)
$ErrorActionPreference = 'Stop'
if (Test-Path $Root) { throw "Refusing to reuse existing path: $Root" }

Add-Type -TypeDefinition @'
using System;
using System.IO;
public static class DemoWriter {
    static readonly byte[] Block = Fill();
    static byte[] Fill() { var b = new byte[1 << 20]; new Random(7).NextBytes(b); return b; }
    public static void Write(string path, long size) {
        Directory.CreateDirectory(Path.GetDirectoryName(path));
        using (var f = new FileStream(path, FileMode.Create, FileAccess.Write, FileShare.None, 1 << 16)) {
            long left = size;
            while (left > 0) { int n = (int)Math.Min(left, Block.Length); f.Write(Block, 0, n); left -= n; }
        }
    }
    public static void Append(string path, long size) {
        using (var f = new FileStream(path, FileMode.Append, FileAccess.Write, FileShare.None, 1 << 16)) {
            long left = size;
            while (left > 0) { int n = (int)Math.Min(left, Block.Length); f.Write(Block, 0, n); left -= n; }
        }
    }
}
'@

$MiB = 1MB
function Mk($rel, $size, [datetime]$when) {
    $p = Join-Path $script:Tree $rel
    [DemoWriter]::Write($p, $size)
    if ($when) { (Get-Item -LiteralPath $p).LastWriteTimeUtc = $when }
}

# ---- Monday -----------------------------------------------------------
$script:Tree = Join-Path $Root 'monday'
$recent = [datetime]'2026-10-26T09:00:00Z'
$old    = [datetime]'2024-01-01T00:00:00Z'

# Two node_modules trees: lots of small files.
foreach ($app in @(@('app-web', 300), @('app-api', 200))) {
    for ($p = 0; $p -lt $app[1]; $p++) {
        $pkg = 'pkg-{0:d4}' -f $p
        for ($i = 0; $i -lt 40; $i++) {
            $ext = @('.js', '.js', '.js', '.json', '.map')[$i % 5]
            $sub = if ($i % 4 -eq 0) { 'lib' } else { '' }
            Mk "$($app[0])\node_modules\$pkg\$sub\f$i$ext" (4096 * (1 + ($i % 3))) $recent
        }
    }
}
for ($i = 0; $i -lt 30; $i++) { Mk "app-web\dist\bundle-$i.js" (1 * $MiB) $recent }

# Build objects.
for ($i = 0; $i -lt 8000; $i++) { Mk ("build\obj\{0:d2}\unit{1}.o" -f ($i % 50), $i) 8192 $recent }

# Logs: three big ones, thirty small ones.
Mk 'logs\server.log' (60 * $MiB) $recent
Mk 'logs\access.log' (45 * $MiB) $recent
Mk 'logs\debug.log'  (38 * $MiB) $recent
for ($i = 1; $i -le 30; $i++) { Mk ("logs\archive\2026-{0:d2}\app-{1}.log" -f (($i % 9) + 1), $i) $MiB $recent }

# Media.
for ($i = 1; $i -le 10; $i++) { Mk "media\training\raw\day$i.mp4" (24 * $MiB) $recent }

# The needle: a big file buried seven levels deep.
Mk 'data\exports\2025\q3\csv\staging\old\backup-final-DO-NOT-DELETE.bak' (72 * $MiB) $recent

# Old stuff nobody touched since 2024.
Mk 'old\vm-image.vhdx' (100 * $MiB) $old
for ($i = 0; $i -lt 200; $i++) { Mk "old\notes\n$i.txt" 4096 $old }

# Empty files and directories.
for ($i = 0; $i -lt 400; $i++) { Mk "tmp\empty-$i.tmp" 0 $recent }
for ($i = 0; $i -lt 15; $i++) { New-Item -ItemType Directory -Path (Join-Path $script:Tree "tmp\emptydir-$i") | Out-Null }

# ---- Friday = Monday + changes ------------------------------------------
Copy-Item -Recurse -Path (Join-Path $Root 'monday') -Destination (Join-Path $Root 'friday')
$script:Tree = Join-Path $Root 'friday'
$late = [datetime]'2026-10-30T17:00:00Z'

for ($i = 0; $i -lt 25; $i++) { Mk "app-api\.cache\blob-$i.bin" (8 * $MiB) $late }      # new, +200 MiB
[DemoWriter]::Append((Join-Path $script:Tree 'logs\server.log'), 30 * $MiB)           # grew, +30 MiB
(Get-Item -LiteralPath (Join-Path $script:Tree 'logs\server.log')).LastWriteTimeUtc = $late
Remove-Item -LiteralPath (Join-Path $script:Tree 'media\training\raw\day10.mp4')      # gone, -24 MiB
for ($i = 0; $i -lt 100; $i++) { Remove-Item -LiteralPath (Join-Path $script:Tree "tmp\empty-$i.tmp") }

# ---- Ground truth --------------------------------------------------------
$answers = [ordered]@{
    monday = [ordered]@{
        total_files          = 28675
        total_bytes          = 873021440
        top5_files           = @(
            'old\vm-image.vhdx',
            'data\exports\2025\q3\csv\staging\old\backup-final-DO-NOT-DELETE.bak',
            'logs\server.log', 'logs\access.log', 'logs\debug.log')
        files_over_30MiB_count = 5
        top_extension_by_bytes = '.mp4'
        empty_files          = 400
        empty_dirs           = 15
        untouched_since_2025 = 'old\vm-image.vhdx'
    }
    friday_vs_monday = [ordered]@{
        new_dir_bytes        = 200 * $MiB
        new_dir              = 'app-api\.cache'
        grown_file           = 'logs\server.log'
        grown_bytes          = 30 * $MiB
        deleted_file         = 'media\training\raw\day10.mp4'
        deleted_bytes        = 24 * $MiB
        empty_files_removed  = 100
    }
}
$answers | ConvertTo-Json -Depth 5 | Set-Content -Path (Join-Path $Root 'answers.json') -Encoding UTF8
"Done. Compare your tool's answers with $Root\answers.json"
