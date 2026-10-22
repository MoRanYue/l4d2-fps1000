# Builds dist-diag\fps1000-<version>-diag-plapi{16,17,18}.zip from the instrumented DLLs.
# Each archive extracts into the dedicated server root, same layout as the release packages:
#   addons\fps1000\DIAGNOSE.txt
#   addons\fps1000\bin\fps1000.dll
#   addons\fps1000\bin\fps1000.2.l4d2.dll
param([string]$Version = "0.1.2")

$root = Split-Path -Parent $PSScriptRoot
$src = Join-Path $root "dist-diag"
if (-not (Test-Path $src)) { throw "missing $src" }

Add-Type -AssemblyName System.IO.Compression | Out-Null
Add-Type -AssemblyName System.IO.Compression.FileSystem | Out-Null

$diag = @"
fps1000 $Version - DIAGNOSTIC BUILD (not a release)

This build behaves exactly like the normal plugin, but every second it appends one line to
fps1000.log (next to srcds.exe) describing what the per-frame wait actually did.

INSTALL
  1. Extract this archive into your dedicated server directory (the one that contains srcds.exe).
     The archive only adds addons/fps1000/ and never overwrites metaplugins.ini.
  2. Make sure Metamod:Source is installed, then append this line to
     left4dead2/addons/metamod/metaplugins.ini:
       addons/fps1000/bin/fps1000
  3. Start the server. fps1000.log should contain:
       [fps1000] install: OK dedicated.dll=0x... IAT[Sleep]=0x... saved=0x... stub=0x...
       [fps1000] diag: build=$Version plapi=... pid=... qpc_freq=...
       [fps1000] timer: request=...us wakes after ...ms (spread ...us)

WHAT TO SEND BACK
  - The whole fps1000.log after the problem has shown up (do not trim it).
  - The wall-clock time when the problem happened (the log counts seconds since start as t=...).
  - The output of `stats` (or the sv fps value you read) at that moment.
  - The server's fps_max setting (0 or a number) and how it was set (command line, cfg, or a plugin).
  - `meta version` and `meta list` output.
  - Windows version (winver) and CPU model.

WHAT THE LOG MEANS
  [diag] t=SEC ... fps=... period p50/p90/p99/min/max ... wait p50/p99/min/max req=...us
    period = time between two frame sleeps; wait = how long the plugin's own wait took.
    Healthy: period p50 between 1.00 and 1.03 ms, p99 below ~1.2 ms, simFactor between
    0.99 and 1.01.
  counters flat=/sleep_fb=/early=/spin=/spin_avg=
    flat     = frames that used the calibrated wait (the normal path)
    sleep_fb = frames that had to call the real Sleep() (timer unavailable)
    early    = frames whose wait ended past the deadline (the frame ran long)
    spin     = frames finished by spinning, spin_avg = average time spent spinning
  engine hostFrames=/simFrames=/host_frametime=/simFactor=
    hostFrames/s should stay near the sv fps you see; simFactor is sim time / wall time and
    must stay at 1.00 - if it drifts above 1.00 the game runs fast.
  timer_resolution=... and throttled=
    the timer resolution Windows reports for the process, and whether Windows has set the
    PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION bit that silently revokes it.

To uninstall, remove the line from metaplugins.ini and delete addons/fps1000/.
"@
$diagBytes = [System.Text.Encoding]::ASCII.GetBytes(($diag -replace "`r`n", "`n"))

$stamp = [DateTimeOffset]::new(2026, 10, 22, 0, 0, 0, [TimeSpan]::Zero)
$results = @()

foreach ($p in 16, 17, 18) {
    $dll = Join-Path $src ("fps1000-diag-plapi{0}.dll" -f $p)
    if (-not (Test-Path $dll)) { throw "missing $dll" }
    $dllBytes = [System.IO.File]::ReadAllBytes($dll)
    $zip = Join-Path $src ("fps1000-{0}-diag-plapi{1}.zip" -f $Version, $p)
    if (Test-Path $zip) { Remove-Item $zip -Force }

    $fs = [System.IO.File]::Open($zip, [System.IO.FileMode]::Create)
    $za = New-Object System.IO.Compression.ZipArchive($fs, [System.IO.Compression.ZipArchiveMode]::Create, $true)
    $add = {
        param($name, $bytes)
        $e = $za.CreateEntry($name, [System.IO.Compression.CompressionLevel]::Optimal)
        $e.LastWriteTime = $stamp
        if ($null -ne $bytes) {
            $s = $e.Open()
            $s.Write($bytes, 0, $bytes.Length)
            $s.Dispose()
        }
    }
    & $add 'addons/' $null
    & $add 'addons/fps1000/' $null
    & $add 'addons/fps1000/DIAGNOSE.txt' $diagBytes
    & $add 'addons/fps1000/bin/' $null
    & $add 'addons/fps1000/bin/fps1000.dll' $dllBytes
    & $add 'addons/fps1000/bin/fps1000.2.l4d2.dll' $dllBytes
    $za.Dispose()
    $fs.Dispose()

    $results += [pscustomobject]@{
        Zip    = Split-Path -Leaf $zip
        Bytes  = (Get-Item $zip).Length
        SHA256 = (Get-FileHash $zip -Algorithm SHA256).Hash
    }
}

$results | Format-Table -AutoSize | Out-String -Width 200
