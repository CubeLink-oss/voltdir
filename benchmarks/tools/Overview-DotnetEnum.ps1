<#
.SYNOPSIS
  基準對照組之二（dotnet-enum）：精簡的 .NET 列舉（System.IO.Enumeration.FileSystemEnumerable），
  一次走訪算出與 Overview-Gci.ps1 相同的資訊，輸出一個 JSON 物件。
.DESCRIPTION
  這是「寫得好的 .NET」：只取檔案的長度與所在資料夾，不建立 FileInfo；
  EnumerationOptions 設為遞迴、略過讀不到的位置、略過重解析點（避免進入連結造成重複或迴圈）。
  C# 在本程序內以 Add-Type 編譯，編譯時間（約一秒）算在耗時內，說明見 benchmarks/README.md。
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

public static class OverviewEnum
{
    public static string Run(string root, int top, int topExt)
    {
        string rootFull = Path.GetFullPath(root).TrimEnd('\\');
        var opt = new EnumerationOptions
        {
            RecurseSubdirectories = true,
            IgnoreInaccessible = true,
            AttributesToSkip = FileAttributes.ReparsePoint,
            ReturnSpecialDirectories = false,
        };
        long count = 0, total = 0;
        var ext = new Dictionary<string, long>(StringComparer.OrdinalIgnoreCase);
        var dirs = new Dictionary<string, long>(StringComparer.OrdinalIgnoreCase);
        var tops = new List<KeyValuePair<long, string>>();
        long floor = 0;
        string lastDir = null; long lastSum = 0;

        var en = new FileSystemEnumerable<int>(rootFull, (ref FileSystemEntry e) =>
        {
            long len = e.Length;
            count++; total += len;
            string d = e.Directory.ToString();
            if (!string.Equals(d, lastDir, StringComparison.Ordinal))
            {
                if (lastDir != null) { long cur; dirs.TryGetValue(lastDir, out cur); dirs[lastDir] = cur + lastSum; }
                lastDir = d; lastSum = 0;
            }
            lastSum += len;
            string name = e.FileName.ToString();
            string x = Path.GetExtension(name).ToLowerInvariant();
            long c; ext.TryGetValue(x, out c); ext[x] = c + len;
            if (tops.Count < top || len > floor)
            {
                tops.Add(new KeyValuePair<long, string>(len, Path.Combine(d, name)));
                if (tops.Count > 4 * top)
                {
                    tops.Sort((a, b) => b.Key.CompareTo(a.Key));
                    tops.RemoveRange(top, tops.Count - top);
                    floor = tops[tops.Count - 1].Key;
                }
            }
            return 0;
        }, opt) { ShouldIncludePredicate = (ref FileSystemEntry e) => !e.IsDirectory };
        foreach (var _ in en) { }
        if (lastDir != null) { long cur; dirs.TryGetValue(lastDir, out cur); dirs[lastDir] = cur + lastSum; }

        var roll = new Dictionary<string, long>(StringComparer.OrdinalIgnoreCase);
        foreach (var kv in dirs)
        {
            string d = kv.Key;
            while (!string.IsNullOrEmpty(d) && d.Length > rootFull.Length)
            {
                long cur; roll.TryGetValue(d, out cur); roll[d] = cur + kv.Value;
                d = Path.GetDirectoryName(d);
            }
        }
        var rl = new List<KeyValuePair<string, long>>(roll);
        rl.Sort((a, b) => { int c = b.Value.CompareTo(a.Value); return c != 0 ? c : string.CompareOrdinal(a.Key, b.Key); });
        var el = new List<KeyValuePair<string, long>>(ext);
        el.Sort((a, b) => b.Value.CompareTo(a.Value));
        tops.Sort((a, b) => b.Key.CompareTo(a.Key));

        var sb = new StringBuilder();
        sb.Append("{\"total_files\":").Append(count).Append(",\"total_bytes\":").Append(total).Append(",\"top_dirs\":[");
        for (int i = 0; i < Math.Min(top, rl.Count); i++) { if (i > 0) sb.Append(','); sb.Append("{\"path\":").Append(Q(rl[i].Key)).Append(",\"size\":").Append(rl[i].Value).Append('}'); }
        sb.Append("],\"top_files\":[");
        for (int i = 0; i < Math.Min(top, tops.Count); i++) { if (i > 0) sb.Append(','); sb.Append("{\"path\":").Append(Q(tops[i].Value)).Append(",\"size\":").Append(tops[i].Key).Append('}'); }
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
[OverviewEnum]::Run($Root, $Top, $TopExt)
