# Run a measurement script over a sequence of arms, e.g. an ABBA order so drift cancels:
#   powershell -File tools\perf_ab_seq.ps1 -Csv C:\od-unified\perf\ab\phase0.csv -Seq "pw,main,main,pw" `
#     -Arms "pw=C:\odw\pw\target\release\deps\r_roblox-X.exe;main=...\r_roblox-Y.exe"
# An arm may carry its own environment after '|', comma-separated:
#     -Arms "full=<exe>|OMNI_PROP_FULL_COPY=1;tail=<exe>" -Script perf_boot.ps1
param(
  [Parameter(Mandatory)][string]$Seq,
  [Parameter(Mandatory)][string]$Arms,
  [Parameter(Mandatory)][string]$Csv,
  [string]$Script = "perf_ab.ps1",
  [string]$ExtraEnv = ""
)
$map = @{}; $envs = @{}
foreach ($kv in $Arms.Split(";")) {
  if (-not $kv) { continue }
  $p = $kv.Split("=", 2); $rest = $p[1].Split("|", 2)
  $map[$p[0]] = $rest[0]
  $envs[$p[0]] = (@($ExtraEnv) + $(if ($rest.Count -gt 1) { $rest[1].Split(",") } else { @() }) | Where-Object { $_ }) -join ";"
}
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
foreach ($arm in $Seq.Split(",")) {
  $os = Get-CimInstance Win32_OperatingSystem
  Write-Output ("{0} arm {1}: free commit {2:N1} GB" -f (Get-Date -Format HH:mm:ss), $arm, ($os.FreeVirtualMemory / 1MB))
  $a = @("-NoProfile", "-ExecutionPolicy", "Bypass", "-File", (Join-Path $here $Script), "-Arm", $arm, "-Exe", $map[$arm], "-Csv", $Csv)
  if ($envs[$arm]) { $a += @("-ExtraEnv", $envs[$arm]) }
  & powershell @a
}
Write-Output "SEQ DONE $(Get-Date -Format o)"
