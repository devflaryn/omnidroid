#!/bin/bash
# One frame-rate reading off a running guest, with the evidence beside it.
#
#   ./sf-timestats.sh <label> [seconds]
#
#   ADBP      adb port of the instance          (default 16001)
#   LAYERPAT  grep -E pattern for the layer     (default: Roblox's game surface)
#   OUTDIR    where to write the run            (default: ./bench-runs)
#
# Writes <OUTDIR>/m_<label>_<time>/ containing timestats, top, and a
# screenshot from BEFORE and AFTER the sample. The screenshots are the point:
# a timestats number taken while a loader, a disconnect dialog or an autoexec
# GUI is on screen is not a frame rate, and three such readings were believed
# in this project before anyone looked. See MODES.md, "The 60 fps ceiling".
#
# ⚠ Move the executor's autoexec scripts aside first and put them back after.
# zaphub.lua draws a full-screen GUI and pins any reading to a flat ~60 fps at
# ~6% guest CPU no matter what the stack underneath is doing.
LABEL="$1"; SECS="${2:-30}"
SP="${OUTDIR:-$(cd "$(dirname "$0")" && pwd)/bench-runs}"
A="adb -s 127.0.0.1:${ADBP:-16001}"
PAT="${LAYERPAT:-SurfaceView\[com.roblox.client}"
OUT="$SP/m_${LABEL}_$(date +%H%M%S)"
mkdir -p "$OUT"
$A exec-out screencap -p > "$OUT/before.png"
LAYER=$($A shell dumpsys SurfaceFlinger --list | tr -d '\r' | grep -E "$PAT" | tail -1)
echo "layer: $LAYER" | tee "$OUT/info.txt"
$A shell dumpsys SurfaceFlinger --timestats -clear >/dev/null
$A shell dumpsys SurfaceFlinger --timestats -enable >/dev/null
$A shell dumpsys SurfaceFlinger --latency-clear >/dev/null 2>&1
sleep "$SECS"
$A shell dumpsys SurfaceFlinger --timestats -dump | tr -d '\r' > "$OUT/timestats.txt"
$A shell dumpsys SurfaceFlinger --timestats -disable >/dev/null
$A shell top -b -n 1 | tr -d '\r' | head -12 > "$OUT/top.txt"
$A exec-out screencap -p > "$OUT/after.png"
if [ -n "$LAYER" ]; then $A shell dumpsys SurfaceFlinger --latency "\"$LAYER\"" | tr -d '\r' > "$OUT/latency.txt"; fi
TF=$(grep -m1 "totalFrames" "$OUT/timestats.txt" | grep -o "[0-9]*"); DT=$(grep -m1 "displayOnTime" "$OUT/timestats.txt" | grep -o "[0-9]*"); MF=$(grep -m1 "missedFrames" "$OUT/timestats.txt" | grep -o "[0-9]*"); CC=$(grep -m1 "clientCompositionFrames" "$OUT/timestats.txt" | grep -o "[0-9]*")
FPS=$(awk -v f="$TF" -v t="$DT" 'BEGIN{ if (t>0) printf "%.1f", f*1000/t; else print "n/a"}')
echo "$LABEL: totalFrames=$TF displayOnTime_ms=$DT fps=$FPS missed=$MF clientComp=$CC" | tee -a "$OUT/info.txt"
grep -iE "cpu|jellyfish|roblox" "$OUT/top.txt" | head -4
# present-to-present from the latency dump: col 2 = actual present time (ns); skip pending (INT64_MAX) rows
if [ -s "$OUT/latency.txt" ]; then
  echo "latency rows: $(wc -l < "$OUT/latency.txt")  refresh period(ns): $(head -1 "$OUT/latency.txt")" | tee -a "$OUT/info.txt"
  awk 'NR>1 && NF>=2 && $2>0 && $2<9000000000000000000 {if (prev>0 && $2>prev) print ($2-prev)/1e6; prev=$2}' "$OUT/latency.txt" | sort -n | awk '{a[NR]=$1} END{ if (NR>0) printf "present->present ms: n=%d min=%.2f p10=%.2f p50=%.2f p90=%.2f max=%.2f\n", NR, a[1], a[int(NR*0.1)+1], a[int(NR*0.5)+1], a[int(NR*0.9)+1], a[NR]}' | tee -a "$OUT/info.txt"
fi
echo "saved: $OUT"
