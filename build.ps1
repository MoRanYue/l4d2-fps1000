# build.ps1 — 构建 fps1000.dll（纯 Rust cdylib，无 C++ 依赖）
# 用法：powershell -ExecutionPolicy Bypass -File build.ps1
# 产物：dist\fps1000.dll（PLAPI 16）、dist\fps1000_pl17.dll、dist\fps1000_pl18.dll
$ErrorActionPreference = 'Stop'

$root = $PSScriptRoot                 # D:\GITHUB\l4d2-fps\plugin
$core = Join-Path $root 'core'
$dist = Join-Path $root 'dist'
$out  = Join-Path $core 'target\i686-pc-windows-msvc\release\fps1000.dll'

New-Item -ItemType Directory -Force -Path $dist | Out-Null

# 版本 -> 产物名
$variants = @(
    @{ Feature = 'plapi16'; Name = 'fps1000.dll' },
    @{ Feature = 'plapi17'; Name = 'fps1000_pl17.dll' },
    @{ Feature = 'plapi18'; Name = 'fps1000_pl18.dll' }
)

Push-Location $core
try {
    foreach ($v in $variants) {
        Write-Host ("== cargo build --release --no-default-features --features {0} ==" -f $v.Feature)
        & cargo build --release --offline --no-default-features --features $v.Feature
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($($v.Feature)): $LASTEXITCODE" }
        if (-not (Test-Path $out)) { throw "missing $out" }
        Copy-Item $out (Join-Path $dist $v.Name) -Force
    }
} finally {
    Pop-Location
}

Get-ChildItem (Join-Path $dist '*.dll') | Select-Object Name, Length, LastWriteTime | Format-Table -AutoSize
