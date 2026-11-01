<#
.SYNOPSIS
  基準對照組之三（dotnet-enum-stream）：邊走邊彙總的精簡 .NET，不收集全部檔案，也不為每個資料夾留一筆紀錄。
.DESCRIPTION
  與 Overview-DotnetEnum.ps1 回答同樣的問題，但記憶體策略相反：
  逐層遞迴，每個資料夾只用 FileSystemEnumerable 列舉它自己的項目（不遞迴），檔案立即折進
  該資料夾的合計，子資料夾回傳合計後折進父層；資料夾排行與最大檔案只保留前 N 名的小清單，
  副檔名只保留一個小字典。記憶體用量與樹的深度和副檔名種類成正比，不與檔案數成正比。
  為了不讓對照組因為單執行緒而吃虧，根目錄與其下兩層的子資料夾以 Parallel.ForEach 並行。
  C# 在本程序內以 Add-Type 編譯，編譯時間（約一秒）算在耗時內，說明見 benchmarks/README.md。
  這是「寫得很好的 .NET」，目的是讓記憶體與耗時的比較誠實：voltdir 要對照的是它，不是寫得鬆散的版本。
#>
param(
    [Parameter(Mandatory)][string]$Root,
    [int]$Top = 20,
    [int]$TopExt = 10
)
$ErrorActionPreference = 'Stop'
Add-Type -Language CSharp -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.IO;
using System.IO.Enumeration;
using System.Text;
using System.Threading.Tasks;

public static class OverviewEnumStream
{
    struct Item { public string Name; public bool IsDir; public long Length; }

    static int Top;
    static readonly object Gate = new object();
    static long count, total;
    static readonly Dictionary<string, long> Ext = new Dictionary<string, long>(StringComparer.OrdinalIgnoreCase);
    static readonly List<KeyValuePair<long, string>> TopFiles = new List<KeyValuePair<long, string>>();
    static readonly List<KeyValuePair<long, string>> TopDirs = new List<KeyValuePair<long, string>>();
    static long fileFloor, dirFloor;

    static void Offer(List<KeyValuePair<long, string>> list, ref long floor, long size, string path)
    {
        if (list.Count >= 4 * Top + 8 && size <= floor) return;
        list.Add(new KeyValuePair<long, string>(size, path));
        if (list.Count > 4 * Top + 8)
        {
            list.Sort((a, b) => { int c = b.Key.CompareTo(a.Key); return c != 0 ? c : string.CompareOrdinal(a.Value, b.Value); });
            list.RemoveRange(Top, list.Count - Top);
            floor = list[list.Count - 1].Key;
        }
    }

    // 回傳此資料夾（含所有子層）的位元組合計；只列舉一層
    static long Walk(string dir, int depth, bool isRoot)
    {
        var opt = new EnumerationOptions
        {
            RecurseSubdirectories = false,
            IgnoreInaccessible = true,
            AttributesToSkip = FileAttributes.ReparsePoint,
            ReturnSpecialDirectories = false,
        };
        var items = new FileSystemEnumerable<Item>(dir, (ref FileSystemEntry e) =>
            new Item { Name = e.FileName.ToString(), IsDir = e.IsDirectory, Length = e.IsDirectory ? 0 : e.Length }, opt);
        long sum = 0, nFiles = 0;
        var subs = new List<string>();
        var localExt = new Dictionary<string, long>(StringComparer.OrdinalIgnoreCase);
        string bigName = null; long bigLen = -1;
        foreach (var it in items)
        {
            if (it.IsDir) { subs.Add(Path.Combine(dir, it.Name)); continue; }
            nFiles++; sum += it.Length;
            string x = Path.GetExtension(it.Name).ToLowerInvariant();
            long c; localExt.TryGetValue(x, out c); localExt[x] = c + it.Length;
            if (it.Length > bigLen) { bigLen = it.Length; bigName = it.Name; }
            bool maybe;
            lock (Gate) { maybe = TopFiles.Count < Top || it.Length > fileFloor; }
            if (maybe) lock (Gate) { Offer(TopFiles, ref fileFloor, it.Length, Path.Combine(dir, it.Name)); }
        }
        long[] subSums = new long[subs.Count];
        if (depth < 2 && subs.Count > 1)
            Parallel.For(0, subs.Count, new ParallelOptions { MaxDegreeOfParallelism = Environment.ProcessorCount }, i => { subSums[i] = Walk(subs[i], depth + 1, false); });
        else
            for (int i = 0; i < subs.Count; i++) subSums[i] = Walk(subs[i], depth + 1, false);
        foreach (long s in subSums) sum += s;
        lock (Gate)
        {
            count += nFiles; total += 0;
            foreach (var kv in localExt) { long c; Ext.TryGetValue(kv.Key, out c); Ext[kv.Key] = c + kv.Value; }
            if (!isRoot) Offer(TopDirs, ref dirFloor, sum, dir);
        }
        return sum;
    }

    public static string Run(string root, int top, int topExt)
    {
        Top = top;
        string rootFull = Path.GetFullPath(root).TrimEnd('\\');
        long all = Walk(rootFull, 0, true);
        total = all;
        TopDirs.Sort((a, b) => { int c = b.Key.CompareTo(a.Key); return c != 0 ? c : string.CompareOrdinal(a.Value, b.Value); });
        TopFiles.Sort((a, b) => b.Key.CompareTo(a.Key));
        var el = new List<KeyValuePair<string, long>>(Ext);
        el.Sort((a, b) => b.Value.CompareTo(a.Value));

        var sb = new StringBuilder();
        sb.Append("{\"total_files\":").Append(count).Append(",\"total_bytes\":").Append(total).Append(",\"top_dirs\":[");
        for (int i = 0; i < Math.Min(top, TopDirs.Count); i++) { if (i > 0) sb.Append(','); sb.Append("{\"path\":").Append(Q(TopDirs[i].Value)).Append(",\"size\":").Append(TopDirs[i].Key).Append('}'); }
        sb.Append("],\"top_files\":[");
        for (int i = 0; i < Math.Min(top, TopFiles.Count); i++) { if (i > 0) sb.Append(','); sb.Append("{\"path\":").Append(Q(TopFiles[i].Value)).Append(",\"size\":").Append(TopFiles[i].Key).Append('}'); }
        sb.Append("],\"by_ext\":[");
        for (int i = 0; i < Math.Min(topExt, el.Count); i++) { if (i > 0) sb.Append(','); sb.Append("{\"ext\":").Append(Q(el[i].Key)).Append(",\"size\":").Append(el[i].Value).Append('}'); }
        sb.Append("]}");
        return sb.ToString();
    }

    static string Q(string s)
    {
        var sb = new StringBuilder("\"");
        foreach (char c in s)
        {
            if (c == '"' || c == '\\') sb.Append('\\').Append(c);
            else if (c < 0x20) sb.Append("\\u").Append(((int)c).ToString("x4"));
            else sb.Append(c);
        }
        return sb.Append('"').ToString();
    }
}
'@
[OverviewEnumStream]::Run($Root, $Top, $TopExt)
