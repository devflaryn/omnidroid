# Paired A/B statistics from a perf_live.ps1 CSV (pair,arm,...): per metric, the medians of A and B,
# the median paired delta (B - A) with its min/max and in how many pairs B > A, and a noise band --
# the spread of A-vs-A differences between consecutive pairs' A phases (what "no change" looks like).
#   powershell -File tools\perf_pairs.ps1 -Csv C:\od-unified\perf\live\<run>.csv
param([Parameter(Mandatory)][string]$Csv)
[System.Threading.Thread]::CurrentThread.CurrentCulture = [System.Globalization.CultureInfo]::InvariantCulture
$rows = Import-Csv $Csv
function Med($x) { $s = @($x | Sort-Object); if ($s.Count -eq 0) { return [double]::NaN }; if ($s.Count % 2) { $s[[int][math]::Floor($s.Count / 2)] } else { ($s[$s.Count / 2 - 1] + $s[$s.Count / 2]) / 2 } }
$pairs = @($rows | Select-Object -ExpandProperty pair -Unique)
foreach ($m in "fps", "top_ms", "top2_ms", "app_ms", "all_ms", "sys_ms", "priv_gb", "app_priv_gb", "ws_gb") {
  $d = @(); $va = @(); $vb = @()
  foreach ($i in $pairs) {
    $ra = $rows | Where-Object { $_.pair -eq $i -and $_.arm -eq "A" } | Select-Object -First 1
    $rb = $rows | Where-Object { $_.pair -eq $i -and $_.arm -eq "B" } | Select-Object -First 1
    if ($ra -and $rb -and [double]$ra.fps -gt 0 -and [double]$rb.fps -gt 0) { $d += [double]$rb.$m - [double]$ra.$m; $va += [double]$ra.$m; $vb += [double]$rb.$m }
  }
  if ($d.Count -eq 0) { continue }
  $noise = @(); for ($k = 1; $k -lt $va.Count; $k++) { $noise += [math]::Abs($va[$k] - $va[$k - 1]) }
  $ma = Med $va; $md = Med $d
  "{0,-12} A {1,8:N3}  B {2,8:N3}  d(B-A) med {3,8:N3} ({4,6:N1}%)  min {5,8:N3} max {6,8:N3}  B>A {7}/{8}  |A-A| med {9,7:N3}" -f $m, $ma, (Med $vb), $md, (100 * $md / [math]::Max(1e-9, [math]::Abs($ma))), ($d | Measure-Object -Minimum).Minimum, ($d | Measure-Object -Maximum).Maximum, @($d | Where-Object { $_ -gt 0 }).Count, $d.Count, (Med $noise)
}
