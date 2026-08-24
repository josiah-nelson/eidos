# Build the NTFS-in-VHDX test fixture for eidos-diskimg.
#
# Creates a small dynamic VHDX, initialises it GPT + NTFS, writes an invented
# directory tree with known sizes, detaches it, and compresses the image into
# crates/eidos-diskimg/tests/fixtures/. The tree, sizes, and names here are the
# contract that crates/eidos-diskimg/tests/ntfs_fixture.rs asserts against.
#
# Requires an elevated shell and the Hyper-V PowerShell module (New-VHD). Tests
# that need the fixture skip themselves when it is absent, so running this is
# optional.
#
# Usage: .\scripts\make-diskimg-fixture.ps1 [-SizeMB 64] [-Force]
param(
    [int]$SizeMB = 64,
    [switch]$Force
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$outDir = Join-Path $root "crates/eidos-diskimg/tests/fixtures"
$out = Join-Path $outDir "ntfs-gpt-dynamic.vhdx.zst"

if ((Test-Path -LiteralPath $out) -and -not $Force) {
    Write-Host "$out already exists; pass -Force to rebuild." -ForegroundColor Yellow
    return
}
if (-not (Get-Command New-VHD -ErrorAction SilentlyContinue)) {
    throw "New-VHD is unavailable: install the Hyper-V PowerShell module."
}
$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw "This script needs an elevated shell (attaching a VHDX requires it)."
}
if (-not (Get-Command tar -ErrorAction SilentlyContinue)) {
    throw "tar (bsdtar, for zstd compression) is unavailable."
}

New-Item -ItemType Directory -Force -Path $outDir | Out-Null
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("eidos-diskimg-" + [Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $work | Out-Null
$vhdx = Join-Path $work "ntfs-gpt-dynamic.vhdx"
$disk = $null

try {
    Write-Host "==> New-VHD $SizeMB MB (dynamic)" -ForegroundColor Cyan
    New-VHD -Path $vhdx -SizeBytes ($SizeMB * 1MB) -Dynamic | Out-Null
    $disk = Mount-VHD -Path $vhdx -Passthru | Get-Disk
    Initialize-Disk -Number $disk.Number -PartitionStyle GPT | Out-Null
    $primarySize = [math]::Floor($SizeMB * 0.65) * 1MB
    $part = New-Partition -DiskNumber $disk.Number -Size $primarySize -AssignDriveLetter
    $auxPart = New-Partition -DiskNumber $disk.Number -UseMaximumSize -AssignDriveLetter
    Format-Volume -Partition $part -FileSystem NTFS -NewFileSystemLabel "eidosfix" `
        -AllocationUnitSize 4096 -Confirm:$false | Out-Null
    Format-Volume -Partition $auxPart -FileSystem NTFS -NewFileSystemLabel "eidosaux" `
        -AllocationUnitSize 4096 -Confirm:$false | Out-Null
    $drive = "$($part.DriveLetter):"
    $auxDrive = "$($auxPart.DriveLetter):"
    Write-Host "==> writing tree to $drive" -ForegroundColor Cyan

    # Invented words only; sizes are asserted by the Rust tests.
    function Write-Fixture([string]$volume, [string]$relative, [int]$bytes) {
        $full = Join-Path $volume $relative
        New-Item -ItemType Directory -Force -Path (Split-Path -Parent $full) | Out-Null
        $payload = [byte[]]::new($bytes)
        for ($i = 0; $i -lt $bytes; $i++) { $payload[$i] = [byte](65 + ($i % 26)) }
        [IO.File]::WriteAllBytes($full, $payload)
    }

    Write-Fixture $drive "brindle.bin" 4096
    New-Item -ItemType Directory -Force -Path (Join-Path $drive "corpus") | Out-Null
    New-Item -ItemType HardLink -Path (Join-Path $drive "corpus\brindle-link.bin") `
        -Target (Join-Path $drive "brindle.bin") | Out-Null
    Write-Fixture $drive "corpus\alcove\ledger.txt" 37
    Write-Fixture $drive "corpus\alcove\quillon.dat" 100000
    Write-Fixture $drive "corpus\zephyr\marrow.log" 1
    # Unicode name (combining marks and non-Latin script) must survive intact.
    Write-Fixture $drive "corpus\zephyr\grünwald-πλούτος.txt" 11
    # Deep path: 24 nested segments below the root.
    $deep = "vellum"
    foreach ($seg in 1..23) { $deep = Join-Path $deep ("tier{0:d2}" -f $seg) }
    Write-Fixture $drive (Join-Path $deep "sable.txt") 64
    # An empty directory has no members below it but must still be inventoried.
    New-Item -ItemType Directory -Force -Path (Join-Path $drive "hollow") | Out-Null

    # A second filesystem makes the image-wide member budget observable.
    Write-Fixture $auxDrive "auxiliary\marker.txt" 23

    Write-Host "==> dismounting" -ForegroundColor Cyan
    Dismount-VHD -Path $vhdx
    $disk = $null
    Optimize-VHD -Path $vhdx -Mode Full -ErrorAction SilentlyContinue

    $raw = (Get-Item -LiteralPath $vhdx).Length
    Write-Host "==> compressing ($([math]::Round($raw / 1MB, 1)) MB raw)" -ForegroundColor Cyan
    # bsdtar's "raw" write format emits a bare zstd frame, not a tar archive.
    tar --format raw --zstd -cf $out -C $work (Split-Path -Leaf $vhdx)
    if ($LASTEXITCODE -ne 0) { throw "tar failed ($LASTEXITCODE)" }
    $packed = (Get-Item -LiteralPath $out).Length
    Write-Host ("Wrote {0} ({1:n0} bytes, {2:n0} raw)." -f $out, $packed, $raw) -ForegroundColor Green
    if ($packed -gt 2MB) {
        Write-Host "Fixture exceeds 2 MB; keep it out of git and generate on demand." -ForegroundColor Yellow
    }
} finally {
    if ($null -ne $disk) { Dismount-VHD -Path $vhdx -ErrorAction SilentlyContinue }
    Remove-Item -Recurse -Force -LiteralPath $work -ErrorAction SilentlyContinue
}
