# Which allocations of a host process hold RESIDENT pages of all zeros (read-only).
#
# For every private committed allocation (by allocation base): its committed bytes, how much of it
# is resident (QueryWorkingSetEx -- asks, faults nothing in), and how much of the resident part is
# all-zero 4 KiB pages (ReadProcessMemory of resident pages only). The top allocations by resident
# zeros are listed with their protection, a guess at their kind, and their first bytes:
#   heap-segment  -- an NT heap segment (HEAP_SEGMENT's 0xFFEEFFEE signature at +0x10)
#   exec          -- executable (a code cache)
#   wc            -- write-combined (the GPU driver)
#   guest?        -- a 64 KiB allocation (a guest granule; guest pieces of other sizes are "other")
#   other         -- the rest: heap large blocks, guest pieces, thread blocks, driver heaps
# and totals by kind.
#   powershell -File tools\zeroscan.ps1 -ProcessId 1234 [-Top 40]
param([Parameter(Mandatory)][int]$ProcessId, [int]$Top = 40)
Add-Type -TypeDefinition @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public static class ZeroScan {
  [StructLayout(LayoutKind.Sequential)]
  public struct MBI { public IntPtr BaseAddress; public IntPtr AllocationBase; public uint AllocationProtect; public ushort PartitionId;
    public IntPtr RegionSize; public uint State; public uint Protect; public uint Type; }
  [StructLayout(LayoutKind.Sequential)]
  public struct WSEX { public IntPtr VirtualAddress; public ulong Attr; }
  [DllImport("kernel32.dll")] public static extern IntPtr OpenProcess(uint a, bool i, int pid);
  [DllImport("kernel32.dll")] public static extern bool CloseHandle(IntPtr h);
  [DllImport("kernel32.dll")] public static extern IntPtr VirtualQueryEx(IntPtr h, IntPtr addr, out MBI mbi, IntPtr len);
  [DllImport("kernel32.dll")] public static extern bool ReadProcessMemory(IntPtr h, IntPtr a, byte[] b, IntPtr n, out IntPtr r);
  [DllImport("psapi.dll")] public static extern bool QueryWorkingSetEx(IntPtr h, [In, Out] WSEX[] pv, int cb);
  public class Alloc { public long Base; public long Committed; public long Resident; public long Zero; public uint Protect; public string Head = ""; public bool Segment; }
  public static List<Alloc> Run(int pid) {
    IntPtr h = OpenProcess(0x0400 | 0x0010, false, pid);
    if (h == IntPtr.Zero) throw new Exception("OpenProcess failed");
    var by = new Dictionary<long, Alloc>();
    long addr = 0; MBI m; var buf = new byte[4096];
    while (VirtualQueryEx(h, new IntPtr(addr), out m, new IntPtr(Marshal.SizeOf(typeof(MBI)))) != IntPtr.Zero) {
      long size = m.RegionSize.ToInt64(); long b = m.BaseAddress.ToInt64();
      if (m.State == 0x1000 && m.Type == 0x20000 && (m.Protect & 0x100) == 0 && m.Protect != 1) {
        long ab = m.AllocationBase.ToInt64();
        Alloc a; if (!by.TryGetValue(ab, out a)) { a = new Alloc { Base = ab, Protect = m.Protect }; by[ab] = a; }
        a.Committed += size;
        if ((m.Protect & 0xF0) != 0) a.Protect = m.Protect;
        long pages = size / 4096; var q = new WSEX[pages];
        for (long i = 0; i < pages; i++) q[i].VirtualAddress = new IntPtr(b + i * 4096);
        QueryWorkingSetEx(h, q, (int)(pages * Marshal.SizeOf(typeof(WSEX))));
        for (long i = 0; i < pages; i++) {
          if ((q[i].Attr & 1) == 0) continue;
          a.Resident += 4096;
          IntPtr rd; if (!ReadProcessMemory(h, new IntPtr(b + i * 4096), buf, new IntPtr(4096), out rd)) continue;
          if (b + i * 4096 == ab) {
            a.Head = BitConverter.ToString(buf, 0, 32).Replace("-", "");
            a.Segment = BitConverter.ToUInt32(buf, 0x10) == 0xFFEEFFEE;
          }
          bool z = true; for (int j = 0; j < 4096; j += 8) if (BitConverter.ToInt64(buf, j) != 0) { z = false; break; }
          if (z) a.Zero += 4096;
        }
      }
      addr = b + size; if (addr <= 0) break;
    }
    CloseHandle(h);
    return new List<Alloc>(by.Values);
  }
}
"@
function Kind($a) {
  if (($a.Protect -band 0xF0) -ne 0) { return "exec" }
  if (($a.Protect -band 0x400) -ne 0) { return "wc" }
  if ($a.Segment) { return "heap-segment" }
  if ($a.Committed -eq 65536) { return "guest?" }
  return "other"
}
$all = [ZeroScan]::Run($ProcessId)
$mib = { param($b) "{0,8:N1}" -f ($b / 1MB) }
"pid $ProcessId -- private committed / resident / resident-zero MiB by kind:"
$all | Group-Object { Kind $_ } | Sort-Object { ($_.Group | Measure-Object Zero -Sum).Sum } -Descending | ForEach-Object {
  $c = ($_.Group | Measure-Object Committed -Sum).Sum; $r = ($_.Group | Measure-Object Resident -Sum).Sum; $z = ($_.Group | Measure-Object Zero -Sum).Sum
  "  {0,-14} {1} {2} {3}   ({4} allocations)" -f $_.Name, (& $mib $c), (& $mib $r), (& $mib $z), $_.Count
}
"top $Top allocations by resident zeros (base, committed, resident, zero MiB, protect, kind, first 16 bytes):"
$all | Sort-Object Zero -Descending | Select-Object -First $Top | ForEach-Object {
  "  0x{0:x12} {1} {2} {3}  0x{4:x3} {5,-13} {6}" -f $_.Base, (& $mib $_.Committed), (& $mib $_.Resident), (& $mib $_.Zero), $_.Protect, (Kind $_), $_.Head
}
# Allocations of one size that recur (a per-process or per-thread structure shows up as a group).
"recurring sizes with resident zeros (count x committed KiB: resident-zero MiB total):"
$all | Where-Object { $_.Zero -gt 0 } | Group-Object Committed | Where-Object { $_.Count -ge 4 } | Sort-Object { ($_.Group | Measure-Object Zero -Sum).Sum } -Descending | Select-Object -First 10 | ForEach-Object {
  "  {0,5} x {1,8:N0} KiB: {2}" -f $_.Count, ([long]$_.Name / 1KB), (& $mib (($_.Group | Measure-Object Zero -Sum).Sum))
}
