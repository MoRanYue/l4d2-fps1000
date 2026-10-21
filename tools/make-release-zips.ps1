# Package the built DLLs into release archives that already contain the addons tree.
#
#   powershell -ExecutionPolicy Bypass -File tools\make-release-zips.ps1
#
# Produces dist\fps1000-<version>-plapi{16,17,18}.zip. Each archive extracts into the dedicated
# server root and creates:
#
#   addons\fps1000\INSTALL.txt
#   addons\fps1000\bin\fps1000.dll
#   addons\fps1000\bin\fps1000.2.l4d2.dll
#
# metaplugins.ini is intentionally not shipped, so an existing plugin list is never overwritten.

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem

$root = Split-Path -Parent $PSScriptRoot
$dist = Join-Path $root 'dist'

$version = (Select-String -Path (Join-Path $root 'core\Cargo.toml') -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
if (-not $version) { throw 'cannot read version from core\Cargo.toml' }

$install = @"
fps1000 $version - Metamod:Source plugin for the Left 4 Dead 2 Windows dedicated server
==============================================================================

1. Extract this archive into your dedicated server root folder
   (the one that contains srcds.exe). You should end up with:

       addons\fps1000\bin\fps1000.dll
       addons\fps1000\bin\fps1000.2.l4d2.dll

2. Make sure Metamod:Source is installed, then append this line to
   addons\metamod\metaplugins.ini:

       addons/fps1000/bin/fps1000

3. Restart the server and check left4dead2\console.log for:

       [fps1000] install: OK dedicated.dll=0x... IAT[Sleep]=0x... saved=0x... stub=0x...

4. After boot, make sure fps_max is not limiting you:

       sm_cvar fps_max 0

   An empty server hibernates (sv_hibernate_when_empty 1) and stays near
   50 FPS until a player joins; set it to 0 to measure on an idle server.

The server settles at roughly 850-1000 FPS, depending on how closely the
host's high-resolution timer can land on 1 ms (the plugin logs the value it
picked). It deliberately never goes faster: the engine floors host_frametime
at 1 ms and advances the simulation by that value, so a faster loop would
make the game run faster than real time.

To uninstall, remove the line from metaplugins.ini and delete
addons\fps1000.
"@
$installBytes = [System.Text.Encoding]::ASCII.GetBytes(($install -replace "`r`n", "`n"))

$variants = @(
    @{ Feature = 'plapi16'; Dll = 'fps1000.dll' },
    @{ Feature = 'plapi17'; Dll = 'fps1000_pl17.dll' },
    @{ Feature = 'plapi18'; Dll = 'fps1000_pl18.dll' }
)
$stamp = [DateTimeOffset]::new(2026, 10, 20, 0, 0, 0, [TimeSpan]::Zero)

foreach ($v in $variants) {
    $dllPath = Join-Path $dist $v.Dll
    if (-not (Test-Path $dllPath)) { throw "missing $dllPath (run build.ps1 first)" }
    $zip = Join-Path $dist ("fps1000-{0}-{1}.zip" -f $version, $v.Feature)
    Remove-Item $zip -Force -ErrorAction SilentlyContinue

    $dllBytes = [System.IO.File]::ReadAllBytes($dllPath)
    $fs = [System.IO.File]::Open($zip, [System.IO.FileMode]::Create)
    $za = New-Object System.IO.Compression.ZipArchive($fs, [System.IO.Compression.ZipArchiveMode]::Create)
    $add = {
        param($name, $bytes)
        $e = $za.CreateEntry($name, [System.IO.Compression.CompressionLevel]::Optimal)
        $e.LastWriteTime = $stamp
        if ($null -ne $bytes) {
            $s = $e.Open(); $s.Write($bytes, 0, $bytes.Length); $s.Dispose()
        }
    }
    & $add 'addons/' $null
    & $add 'addons/fps1000/' $null
    & $add 'addons/fps1000/INSTALL.txt' $installBytes
    & $add 'addons/fps1000/bin/' $null
    & $add 'addons/fps1000/bin/fps1000.dll' $dllBytes
    & $add 'addons/fps1000/bin/fps1000.2.l4d2.dll' $dllBytes
    $za.Dispose(); $fs.Dispose()

    "{0}  {1} bytes  {2}" -f (Split-Path $zip -Leaf), (Get-Item $zip).Length, (Get-FileHash $zip -Algorithm SHA256).Hash
}
