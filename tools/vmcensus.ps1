# Where a host process's private commit is, by kind of region (read-only; VirtualQueryEx sweep):
#   stacks   -- allocations holding a PAGE_GUARD page (thread stacks), committed part
#   exec     -- private executable (code caches)
#   big      -- other private allocations >= 16 MiB (guest space pieces, mimalloc arenas, driver pools)
#   mid      -- 1..16 MiB
#   small    -- < 1 MiB (heaps' segments, per-thread tables)
#   powershell -File tools\vmcensus.ps1 -ProcessId 1234
param([Parameter(Mandatory)][int]$ProcessId, [int]$Top = 12)
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class VmCensus {
  [StructLayout(LayoutKind.Sequential)]
  public struct MBI { public IntPtr BaseAddress; public IntPtr AllocationBase; public uint AllocationProtect; public ushort PartitionId;
    public IntPtr RegionSize; public uint State; public uint Protect; public uint Type; }
  [DllImport("kernel32.dll")] public static extern IntPtr OpenProcess(uint a, bool i, int pid);
  [DllImport("kernel32.dll")] public static extern bool CloseHandle(IntPtr h);
  [DllImport("kernel32.dll")] public static extern IntPtr VirtualQueryEx(IntPtr h, IntPtr addr, out MBI mbi, IntPtr len);
  // Returns rows: allocationBase, committedPrivate, hasGuard(0/1), anyExec(0/1)
  public static System.Collections.Generic.List<long[]> Sweep(int pid) {
    var rows = new System.Collections.Generic.Dictionary<long, long[]>();
    IntPtr h = OpenProcess(0x0400 | 0x0010, false, pid);
    if (h == IntPtr.Zero) throw new Exception("OpenProcess failed");
    long addr = 0; MBI m;
    while (VirtualQueryEx(h, new IntPtr(addr), out m, new IntPtr(Marshal.SizeOf(typeof(MBI)))) != IntPtr.Zero) {
      long size = m.RegionSize.ToInt64();
      if (m.State == 0x1000 && m.Type == 0x20000) { // MEM_COMMIT, MEM_PRIVATE
        long b = m.AllocationBase.ToInt64();
        long[] r; if (!rows.TryGetValue(b, out r)) { r = new long[4]; r[0] = b; rows[b] = r; }
        r[1] += size;
        if ((m.Protect & 0x100) != 0) r[2] = 1;
        if ((m.Protect & 0xF0) != 0) r[3] = 1;
      }
      addr = m.BaseAddress.ToInt64() + size;
      if (size <= 0) break;
    }
    CloseHandle(h);
    return new System.Collections.Generic.List<long[]>(rows.Values);
  }
}
"@
$rows = [VmCensus]::Sweep($ProcessId)
$k = @{ stacks = 0L; exec = 0L; big = 0L; mid = 0L; small = 0L }; $n = @{ stacks = 0; exec = 0; big = 0; mid = 0; small = 0 }
foreach ($r in $rows) {
  $c = if ($r[2] -eq 1) { "stacks" } elseif ($r[3] -eq 1) { "exec" } elseif ($r[1] -ge 16MB) { "big" } elseif ($r[1] -ge 1MB) { "mid" } else { "small" }
  $k[$c] += $r[1]; $n[$c]++
}
$total = ($rows | ForEach-Object { $_[1] } | Measure-Object -Sum).Sum
"pid $ProcessId private committed {0:N0} MiB in {1} allocations" -f ($total / 1MB), $rows.Count
foreach ($c in "stacks", "exec", "big", "mid", "small") { "  {0,-7} {1,7:N0} MiB  {2,6} allocations" -f $c, ($k[$c] / 1MB), $n[$c] }
"  largest:"
$rows | Sort-Object { $_[1] } -Descending | Select-Object -First $Top | ForEach-Object { "    0x{0:x12} {1,7:N1} MiB{2}{3}" -f $_[0], ($_[1] / 1MB), $(if ($_[2]) { " guard" } else { "" }), $(if ($_[3]) { " exec" } else { "" }) }
"  mid by size (top):"
$rows | Where-Object { $_[1] -ge 1MB -and $_[1] -lt 16MB -and -not $_[2] -and -not $_[3] } | Group-Object { [long]([math]::Round($_[1] / 256KB)) * 256KB } | Sort-Object { [long]$_.Name * $_.Count } -Descending | Select-Object -First 8 | ForEach-Object { "    ~{0,8:N2} MiB x {1,5} = {2,6:N1} MiB" -f ([long]$_.Name / 1MB), $_.Count, ([long]$_.Name * $_.Count / 1MB) }
# Size histogram of the small ones (per-thread structures show up as a spike at one size).
"  small by size (top):"
$rows | Where-Object { $_[1] -lt 1MB -and -not $_[2] -and -not $_[3] } | Group-Object { $_[1] } | Sort-Object { [long]$_.Name * $_.Count } -Descending | Select-Object -First 6 | ForEach-Object { "    {0,8:N0} KiB x {1,5} = {2,6:N1} MiB" -f ([long]$_.Name / 1KB), $_.Count, ([long]$_.Name * $_.Count / 1MB) }
