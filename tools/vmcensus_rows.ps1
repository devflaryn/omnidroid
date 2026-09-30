# One object per guest host process (omni-linux-run*): name (its --nice-name, "system" for the
# root), priv (MiB, private bytes), exec (MiB of executable private commit: translation caches),
# noexec (priv - exec). Read-only (VirtualQueryEx). Used by perf_boot.ps1.
if (-not ("VmExec" -as [type])) {
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class VmExec {
  [StructLayout(LayoutKind.Sequential)]
  public struct MBI { public IntPtr BaseAddress; public IntPtr AllocationBase; public uint AllocationProtect; public ushort PartitionId;
    public IntPtr RegionSize; public uint State; public uint Protect; public uint Type; }
  [DllImport("kernel32.dll")] public static extern IntPtr OpenProcess(uint a, bool i, int pid);
  [DllImport("kernel32.dll")] public static extern bool CloseHandle(IntPtr h);
  [DllImport("kernel32.dll")] public static extern IntPtr VirtualQueryEx(IntPtr h, IntPtr addr, out MBI mbi, IntPtr len);
  public static long ExecPrivate(int pid) {
    IntPtr h = OpenProcess(0x0400, false, pid);
    if (h == IntPtr.Zero) return -1;
    long addr = 0, exec = 0; MBI m;
    while (VirtualQueryEx(h, new IntPtr(addr), out m, new IntPtr(Marshal.SizeOf(typeof(MBI)))) != IntPtr.Zero) {
      long size = m.RegionSize.ToInt64();
      if (m.State == 0x1000 && m.Type == 0x20000 && (m.Protect & 0xF0) != 0) exec += size;
      addr = m.BaseAddress.ToInt64() + size; if (size <= 0) break;
    }
    CloseHandle(h);
    return exec;
  }
}
"@
}
Get-CimInstance Win32_Process -Filter "Name like 'omni-linux-run%'" | ForEach-Object {
  $p = Get-Process -Id $_.ProcessId -ErrorAction SilentlyContinue
  if ($p) {
    $name = if ($_.CommandLine -match '--nice-name=(\S+)') { $Matches[1].Trim('"') } else { "system" }
    $exec = [VmExec]::ExecPrivate($_.ProcessId) / 1MB
    [pscustomobject]@{ pid = $_.ProcessId; name = $name; priv = $p.PrivateMemorySize64 / 1MB; exec = $exec; noexec = $p.PrivateMemorySize64 / 1MB - $exec }
  }
}
