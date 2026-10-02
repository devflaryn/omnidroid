# Time `omnidroid aosp --cookie --place` on the warm device from nothing of the app installed to
# PS99's own loading screen, N times (tools/join_timer.py; the app uninstalled and the device left
# idle before each). Prints one line per run and the median.
#
#   powershell -File tools/warm_join_bench.ps1 [-Runs 3] [-Label after] [-Cookie <file>] [-Place 8737899170] [-Idle 20] [-After 3]
#
# -After: seconds the session is kept after the loading screen (then ended as Ctrl+C would end it).
param(
    [int]$Runs = 3,
    [string]$Label = "after",
    [string]$Apk = "$HOME\Desktop\Roblox-2.740.931.apk",
    [string]$Cookie = "$HOME\Desktop\cookies\HeZmI_ImYu1080.txt",
    [string]$Place = "8737899170",
    [int]$Idle = 20,
    [int]$After = 3,
    [string]$Exe = "target\release\omnidroid.exe"
)
[System.Threading.Thread]::CurrentThread.CurrentCulture = [System.Globalization.CultureInfo]::InvariantCulture
$results = @()
for ($i = 1; $i -le $Runs; $i++) {
    python tools/device_ctl.py "am force-stop com.roblox.client; pm uninstall com.roblox.client" 2>$null | Out-Null
    Start-Sleep -Seconds $Idle
    $name = "$Label-$i"
    $lines = python tools/join_timer.py --label $name --stop --after $After --limit 400 -- $Exe aosp --apk $Apk --cookie $Cookie --place $Place 2>&1
    $lines | Where-Object { "$_" -match "\[jt\]" } | ForEach-Object { "$_".Substring(0, [Math]::Min(140, "$_".Length)) }
    $hit = $lines | Where-Object { "$_" -match "result: in-game loading screen at ([0-9.]+)" } | Select-Object -First 1
    if ("$hit" -match "at ([0-9.]+) s") { $results += [double]$Matches[1] }
    Start-Sleep -Seconds 3
    python tools/device_ctl.py "am force-stop com.roblox.client" 2>$null | Out-Null
}
$sorted = $results | Sort-Object
"runs: " + (($results | ForEach-Object { $_.ToString("0.0") }) -join ", ")
if ($sorted.Count) { "median: " + $sorted[[int][Math]::Floor(($sorted.Count - 1) / 2)].ToString("0.0") + " s" }
