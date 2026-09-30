# One object per guest host process (omni-linux-run*): name (its --nice-name, "system" for the
# root), priv (MiB, private bytes), exec (MiB of executable private commit: translation caches),
# noexec (priv - exec), prop (MiB committed in allocations that start with an Android prop area --
# `PROP` magic at +8). Read-only (VirtualQueryEx / ReadProcessMemory). Used by perf_boot.ps1.
if (-not ("VmExec2" -as [type])) {
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class VmExec2 {
  [StructLayout(LayoutKind.Sequential)]
  public struct MBI { public IntPtr BaseAddress; public IntPtr AllocationBase; public uint AllocationProtect; public ushort PartitionId;
    public IntPtr RegionSize; public uint State; public uint Protect; public uint Type; }
  [DllImport("kernel32.dll")] public static extern IntPtr OpenProcess(uint a, bool i, int pid);
  [DllImport("kernel32.dll")] public static extern bool CloseHandle(IntPtr h);
  [DllImport("kernel32.dll")] public static extern IntPtr VirtualQueryEx(IntPtr h, IntPtr addr, out MBI mbi, IntPtr len);
  [DllImport("kernel32.dll")] public static extern bool ReadProcessMemory(IntPtr h, IntPtr a, byte[] b, IntPtr n, out IntPtr r);
  // [exec bytes, prop bytes]
  public static long[] Census(int pid) {
    IntPtr h = OpenProcess(0x0410, false, pid);
    if (h == IntPtr.Zero) return new long[] { -1, -1 };
    var alloc = new System.Collections.Generic.Dictionary<long, long[]>(); // base -> [committed, first committed addr, readable]
    long addr = 0, exec = 0; MBI m;
    while (VirtualQueryEx(h, new IntPtr(addr), out m, new IntPtr(Marshal.SizeOf(typeof(MBI)))) != IntPtr.Zero) {
      long size = m.RegionSize.ToInt64();
      if (m.State == 0x1000 && m.Type == 0x20000) {
        if ((m.Protect & 0xF0) != 0) exec += size;
        else {
          long b = m.AllocationBase.ToInt64(); long[] r;
          if (!alloc.TryGetValue(b, out r)) { r = new long[3]; alloc[b] = r; }
          r[0] += size;
          if (r[1] == 0 && (m.Protect & 0x06) != 0 && (m.Protect & 0x100) == 0) { r[1] = m.BaseAddress.ToInt64(); }
        }
      }
      addr = m.BaseAddress.ToInt64() + size; if (size <= 0) break;
    }
    long prop = 0; var buf = new byte[16]; IntPtr rd;
    foreach (var kv in alloc) {
      long[] r = kv.Value;
      if (r[1] == 0 || r[0] < 65536) continue;
      if (ReadProcessMemory(h, new IntPtr(r[1]), buf, new IntPtr(16), out rd) && BitConverter.ToUInt32(buf, 8) == 0x504f5250u) prop += r[0];
    }
    CloseHandle(h);
    return new long[] { exec, prop };
  }
}
"@
}
Get-CimInstance Win32_Process -Filter "Name like 'omni-linux-run%'" | ForEach-Object {
  $p = Get-Process -Id $_.ProcessId -ErrorAction SilentlyContinue
  if ($p) {
    $name = if ($_.CommandLine -match '--nice-name=(\S+)') { $Matches[1].Trim('"') } else { "system" }
    $c = [VmExec2]::Census($_.ProcessId)
    [pscustomobject]@{ pid = $_.ProcessId; name = $name; priv = $p.PrivateMemorySize64 / 1MB; exec = $c[0] / 1MB; noexec = $p.PrivateMemorySize64 / 1MB - $c[0] / 1MB; prop = $c[1] / 1MB }
  }
}
