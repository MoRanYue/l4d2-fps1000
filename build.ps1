# build.ps1 - build fps1000.dll (pure Rust cdylib, no C++ dependency)
# usage: powershell -ExecutionPolicy Bypass -File build.ps1
# output: dist\fps1000.dll (PLAPI 16), dist\fps1000_pl17.dll, dist\fps1000_pl18.dll
$root = $PSScriptRoot                 # D:\GITHUB\l4d2-fps\plugin
$core = Join-Path $root 'core'
$dist = Join-Path $root 'dist'
$out  = Join-Path $core 'target\i686-pc-windows-msvc\release\fps1000.dll'

New-Item -ItemType Directory -Force -Path $dist | Out-Null

# feature -> artifact name
$variants = @(
    @{ Feature = 'plapi16'; Name = 'fps1000.dll' },
    @{ Feature = 'plapi17'; Name = 'fps1000_pl17.dll' },
    @{ Feature = 'plapi18'; Name = 'fps1000_pl18.dll' }
)

# cargo and the MSVC linker write progress to stderr; keep that from aborting the script.
$ErrorActionPreference = 'Continue'
Push-Location $core
try {
    foreach ($v in $variants) {
        Write-Host ("== cargo build --release --no-default-features --features {0} ==" -f $v.Feature)
        & cargo build --release --offline --no-default-features --features $v.Feature 2>&1 |
            ForEach-Object { Write-Host $_ }
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($($v.Feature)): $LASTEXITCODE" }
        if (-not (Test-Path $out)) { throw "missing $out" }
        Copy-Item $out (Join-Path $dist $v.Name) -Force
    }
} finally {
    Pop-Location
}

Get-ChildItem (Join-Path $dist '*.dll') | Select-Object Name, Length, LastWriteTime | Format-Table -AutoSize