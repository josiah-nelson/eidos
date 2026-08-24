# Build the eidos Windows installer package.
#
#   .\installer\build.ps1                # web UI + release exe + MSI
#   .\installer\build.ps1 -SkipWeb       # reuse web\dist
#   .\installer\build.ps1 -SkipRust      # reuse target\release\eidos.exe
#
# Requires: Node.js, Rust, .NET SDK 8+ (WiX v7 is restored from NuGet; the
# OSMF EULA is accepted in the project file). Output: installer\out\eidos.msi
param(
    [switch]$SkipWeb,
    [switch]$SkipRust,
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
Write-Host "eidos $version -> MSI ProductVersion $msiVersion"

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

$binDir = Join-Path $root "target\release"
if (-not (Test-Path (Join-Path $binDir "eidos.exe"))) { throw "missing $binDir\eidos.exe" }

$out = Join-Path $PSScriptRoot "out"
New-Item -ItemType Directory -Force -Path $out | Out-Null
# WiX's incremental build keys on the authoring, not on the bound payload
# path: a changed BinDir can silently reuse the previous eidos.exe. Always
# bind from scratch.
foreach ($stale in @("Eidos.Msi\obj", "Eidos.Msi\bin")) {
    Remove-Item -Recurse -Force (Join-Path $PSScriptRoot $stale) -ErrorAction SilentlyContinue
}
Step "dotnet build Eidos.Msi" {
    dotnet build (Join-Path $PSScriptRoot "Eidos.Msi\Eidos.Msi.wixproj") `
        -c $Configuration -nologo -v minimal `
        -p:Version=$msiVersion -p:BinDir=$binDir -p:OutDir="$out\"
}
Write-Host "MSI: $out\eidos.msi" -ForegroundColor Green

# The setup UI (.NET Framework 4.7.2 WPF bootstrapper application).
$baDir = Join-Path $PSScriptRoot "Eidos.Setup.Ui\bin\$Configuration\net472"
Step "dotnet build Eidos.Setup.Ui" {
    dotnet build (Join-Path $PSScriptRoot "Eidos.Setup.Ui\Eidos.Setup.Ui.csproj") `
        -c $Configuration -nologo -v minimal -p:Version=$msiVersion
}
foreach ($required in @("eidos-setup-ui.exe", "WixToolset.BootstrapperApplicationApi.dll", "mbanative.dll")) {
    if (-not (Test-Path (Join-Path $baDir $required))) { throw "setup UI build is missing $required in $baDir" }
}

# The bundle: eidos-setup.exe = Burn engine + UI + eidos.msi.
foreach ($stale in @("Eidos.Bundle\obj", "Eidos.Bundle\bin")) {
    Remove-Item -Recurse -Force (Join-Path $PSScriptRoot $stale) -ErrorAction SilentlyContinue
}
Step "dotnet build Eidos.Bundle" {
    dotnet build (Join-Path $PSScriptRoot "Eidos.Bundle\Eidos.Bundle.wixproj") `
        -c $Configuration -nologo -v minimal `
        -p:Version=$msiVersion -p:MsiPath="$out\eidos.msi" -p:BaDir=$baDir -p:OutDir="$out\"
}
Write-Host "Setup: $out\eidos-setup.exe" -ForegroundColor Green
