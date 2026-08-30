# Build the eidos Windows installer.
#
#   .\installer\build.ps1                      # web UI + release exe + every artifact
#   .\installer\build.ps1 -SkipWeb -SkipRust   # reuse web\dist and target\release\eidos.exe
#   .\installer\build.ps1 -SkipWeb -SkipRust -SkipBundle      # stop before the bundle (CI signs first)
#   .\installer\build.ps1 -SkipWeb -SkipRust -SkipMsi -SkipUi # bundle only, from signed parts
#   .\installer\build.ps1 -SkipWeb -SkipRust -BinDir target\debug
#
# eidos-setup.exe is the unified setup: it carries eidos.msi and, as an
# optional package, eidos-collector.msi. eidos-collector-setup.exe is the
# administrator/fleet-only collector setup. Each -Skip switch turns off
# exactly one artifact; the unified bundle needs both MSIs built first.
#
# Requires: Node.js, Rust, .NET SDK 8+ (WiX v7 is restored from NuGet; the
# OSMF EULA is accepted in the project files). Output: installer\out\.
param(
    [switch]$SkipWeb,
    [switch]$SkipRust,
    [switch]$SkipMsi,
    [switch]$SkipUi,
    [switch]$SkipBundle,
    [switch]$SkipCollectorMsi,
    [switch]$SkipCollectorBundle,
    [string]$BinDir = "",
    [string]$Configuration = "Release"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

function Step($name, $cmd) {
    Write-Host "==> $name" -ForegroundColor Cyan
    & $cmd
    if ($LASTEXITCODE -ne 0) { throw "$name failed ($LASTEXITCODE)" }
}

# Windows Installer versions are numeric (major.minor.build); drop any
# pre-release suffix from the workspace version.
$cargo = Get-Content (Join-Path $root "Cargo.toml") -Raw
$version = [regex]::Match($cargo, '(?m)^version\s*=\s*"([^"]+)"').Groups[1].Value
$msiVersion = ($version -split '-')[0]
Write-Host "eidos $version -> installer version $msiVersion"

if (-not $SkipWeb) {
    Push-Location (Join-Path $root "web")
    try {
        if (-not (Test-Path node_modules)) { Step "npm ci" { npm ci } }
        Step "npm run build" { npm run build }
    } finally { Pop-Location }
}

if (-not $SkipRust) {
    $env:EIDOS_REQUIRE_WEB = "1"
    Step "cargo build --release" { cargo build --locked --release --bin eidos }
}

if ($BinDir -eq "") { $BinDir = Join-Path $root "target\release" }
$BinDir = (Resolve-Path $BinDir).Path
if (-not (Test-Path (Join-Path $BinDir "eidos.exe"))) { throw "missing $BinDir\eidos.exe" }

$out = Join-Path $PSScriptRoot "out"
New-Item -ItemType Directory -Force -Path $out | Out-Null
$baDir = Join-Path $PSScriptRoot "Eidos.Setup.Ui\bin\$Configuration\net472"

# WiX's incremental build keys on the authoring, not on the bound payload
# path: a changed BinDir can silently reuse the previous eidos.exe. Always
# bind from scratch.
function Clean($project) {
    foreach ($stale in @("$project\obj", "$project\bin")) {
        Remove-Item -Recurse -Force (Join-Path $PSScriptRoot $stale) -ErrorAction SilentlyContinue
    }
}

if (-not $SkipMsi) {
    Clean "Eidos.Msi"
    Step "dotnet build Eidos.Msi" {
        dotnet build (Join-Path $PSScriptRoot "Eidos.Msi\Eidos.Msi.wixproj") `
            -c $Configuration -nologo -v minimal `
            -p:Version=$msiVersion -p:BinDir=$BinDir -p:OutDir="$out\"
    }
    Write-Host "MSI: $out\eidos.msi" -ForegroundColor Green
}

if (-not $SkipUi) {
    Step "dotnet build Eidos.Setup.Ui" {
        dotnet build (Join-Path $PSScriptRoot "Eidos.Setup.Ui\Eidos.Setup.Ui.csproj") `
            -c $Configuration -nologo -v minimal -p:Version=$msiVersion
    }
    Write-Host "Setup UI: $baDir\eidos-setup-ui.exe" -ForegroundColor Green
}

if (-not $SkipCollectorMsi) {
    Clean "Eidos.Collector.Msi"
    Step "dotnet build Eidos.Collector.Msi" {
        dotnet build (Join-Path $PSScriptRoot "Eidos.Collector.Msi\Eidos.Collector.Msi.wixproj") `
            -c $Configuration -nologo -v minimal `
            -p:Version=$msiVersion -p:BinDir=$BinDir -p:OutDir="$out\"
    }
    Write-Host "Collector MSI: $out\eidos-collector.msi" -ForegroundColor Green
}

if (-not $SkipBundle) {
    if (-not (Test-Path (Join-Path $out "eidos.msi"))) { throw "missing $out\eidos.msi" }
    # The unified setup carries the collector package as an optional
    # second package in its chain.
    if (-not (Test-Path (Join-Path $out "eidos-collector.msi"))) { throw "missing $out\eidos-collector.msi (build the collector MSI first)" }
    foreach ($required in @("eidos-setup-ui.exe", "WixToolset.BootstrapperApplicationApi.dll", "mbanative.dll")) {
        if (-not (Test-Path (Join-Path $baDir $required))) { throw "setup UI build is missing $required in $baDir" }
    }
    Clean "Eidos.Bundle"
    Step "dotnet build Eidos.Bundle" {
        dotnet build (Join-Path $PSScriptRoot "Eidos.Bundle\Eidos.Bundle.wixproj") `
            -c $Configuration -nologo -v minimal `
            -p:Version=$msiVersion -p:MsiPath="$out\eidos.msi" -p:CollectorMsiPath="$out\eidos-collector.msi" -p:BaDir=$baDir -p:OutDir="$out\"
    }
    Write-Host "Setup: $out\eidos-setup.exe" -ForegroundColor Green
}

if (-not $SkipCollectorBundle) {
    if (-not (Test-Path (Join-Path $out "eidos-collector.msi"))) { throw "missing $out\eidos-collector.msi" }
    Clean "Eidos.Collector.Bundle"
    Step "dotnet build Eidos.Collector.Bundle" {
        dotnet build (Join-Path $PSScriptRoot "Eidos.Collector.Bundle\Eidos.Collector.Bundle.wixproj") `
            -c $Configuration -nologo -v minimal `
            -p:Version=$msiVersion -p:MsiPath="$out\eidos-collector.msi" -p:OutDir="$out\"
    }
    Write-Host "Collector setup: $out\eidos-collector-setup.exe" -ForegroundColor Green
}
