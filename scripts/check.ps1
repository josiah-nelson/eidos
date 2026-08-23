# Full local check: format, lint, test, release build, web build.
# Usage: .\scripts\check.ps1 [-SkipWeb] [-SkipRelease]
param(
    [switch]$SkipWeb,
    [switch]$SkipRelease
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    $env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
}

function Step($name, $cmd) {
    Write-Host "==> $name" -ForegroundColor Cyan
    & $cmd
    if ($LASTEXITCODE -ne 0) { throw "$name failed ($LASTEXITCODE)" }
}

Step "cargo fmt --check"  { cargo fmt --check }
Step "cargo clippy"       { cargo clippy --all-targets -- -D warnings }
Step "cargo test"         { cargo test }
if (-not $SkipRelease) {
    Step "cargo build --release" { cargo build --release }
}
if (-not $SkipWeb) {
    Push-Location web
    try {
        if (-not (Test-Path node_modules)) { Step "npm ci" { npm ci } }
        Step "npm run lint"  { npm run lint }
        Step "npm test"      { npm test }
        Step "npm run build" { npm run build }
    } finally { Pop-Location }
}
Write-Host "All checks passed." -ForegroundColor Green
