# Six bounded, sequential comparisons. Retain all evidence, including failures.
# Never build binaries, touch installed services, or index an existing corpus.
param([Parameter(Mandatory)][string]$Binary)
$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'This matrix uses Windows process counters.' }
. (Join-Path $PSScriptRoot 'recovery-acceptance.ps1')
$candidatePath = (Resolve-Path -LiteralPath $Binary).Path
$candidateHash = (Get-FileHash -LiteralPath $candidatePath -Algorithm SHA256).Hash
$recordDir = Join-Path $PSScriptRoot ('../bench-results/matrix-' + [guid]::NewGuid().ToString('N'))
$recordDir = [IO.Path]::GetFullPath($recordDir)
New-Item -ItemType Directory -Path $recordDir | Out-Null
# Keep one executable for every launch, including the forced restarts. A build
# in the original target directory cannot silently change half of a run.
$snapshotPath = Join-Path $recordDir 'candidate.exe'
Copy-Item -LiteralPath $candidatePath -Destination $snapshotPath
if ((Get-FileHash -LiteralPath $snapshotPath -Algorithm SHA256).Hash -cne $candidateHash) {
    throw 'Candidate changed while taking the matrix snapshot; no runs started.'
}
$candidatePath = $snapshotPath
$utf8 = [Text.UTF8Encoding]::new($false)
$tuples = @(
    [pscustomobject]@{ workers = 1; scan_threads = 1; concurrent_scans = 1; device_readers = 1 },
    [pscustomobject]@{ workers = 2; scan_threads = 2; concurrent_scans = 1; device_readers = 2 },
    [pscustomobject]@{ workers = 4; scan_threads = 4; concurrent_scans = 2; device_readers = 4 }
)
$order = @(0, 1, 2, 2, 1, 0)
$plan = [ordered]@{
    planned_at_utc = [DateTime]::UtcNow.ToString('o')
    binary_sha256 = $candidateHash
    binary_snapshot = $snapshotPath
    qualification = 'bounded synthetic development comparison; not physical-media or installed qualification'
    conditions = 'host not isolated; reviewer builds and native host-volume activity may overlap'
    thresholds = Get-RecoveryThresholds
    tuples = $tuples
    run_order = $order
    source_count = 2; small_files_per_source = 384; large_files_per_source = 2; large_file_mib = 2
    source_readers = 4; query_interval_ms = 25; idle_seconds = 15; check_restart = $true
    maximum_generated_source_bytes = 6 * 11642404
}
[IO.File]::WriteAllText((Join-Path $recordDir 'plan.json'), ($plan | ConvertTo-Json -Depth 8), $utf8)
$results = [Collections.Generic.List[object]]::new()
try {
    for ($run = 0; $run -lt $order.Count; $run++) {
        if ((Get-FileHash -LiteralPath $candidatePath -Algorithm SHA256).Hash -cne $candidateHash) {
            throw 'Candidate binary changed during the matrix; refusing mixed-build evidence.'
        }
        $tuple = $tuples[$order[$run]]
        Write-Host "Matrix run $($run + 1)/6: workers/scan/concurrent/device = $($tuple.workers)/$($tuple.scan_threads)/$($tuple.concurrent_scans)/$($tuple.device_readers)"
        $raw = & (Join-Path $PSScriptRoot 'recovery-smoke.ps1') -Binary $candidatePath `
            -Files 384 -SourceCount 2 -ContentWorkers $tuple.workers -ScanThreads $tuple.scan_threads `
            -ConcurrentScans $tuple.concurrent_scans -DeviceReaders $tuple.device_readers -SourceReaders 4 `
            -LargeFilesPerSource 2 -LargeFileMiB 2 -QueryIntervalMilliseconds 25 -IdleSeconds 15 -CheckRestart
        $json = $raw -join [Environment]::NewLine
        [IO.File]::WriteAllText((Join-Path $recordDir "run-$($run + 1).json"), $json, $utf8)
        $report = $json | ConvertFrom-Json
        if ((Get-FileHash -LiteralPath $candidatePath -Algorithm SHA256).Hash -cne $candidateHash) {
            throw 'Candidate snapshot changed during a run; refusing mixed-build evidence.'
        }
        $evaluation = Test-RecoveryMeasurement -Report $report -ExpectedHash $candidateHash -Tuple $tuple
        $results.Add([pscustomobject]@{
            run = $run + 1; tuple = $tuple; evaluation = $evaluation
            crawl_seconds = $report.crawl.seconds; query_samples = $report.http_query_samples
            query_p95_ms = $report.http_query_p95_ms; query_p99_ms = $report.http_query_p99_ms
            idle_cpu_one_core_percent = $report.idle.cpu_one_core_percent
            idle_process_transfer_bytes = [double]$report.idle.process_read_bytes + [double]$report.idle.process_write_bytes
        })
        [IO.File]::WriteAllText((Join-Path $recordDir 'results.json'), (ConvertTo-Json -InputObject $results.ToArray() -Depth 8), $utf8)
        Write-Host "Run $($run + 1): synthetic thresholds pass=$($evaluation.passed_synthetic_thresholds); failed=$($evaluation.failed_checks -join ', ')"
        if ($evaluation.rejected_because) { Write-Host "Run $($run + 1) report rejected: $($evaluation.rejected_because)" }
    }
    $failedRuns = @($results | Where-Object { -not $_.evaluation.passed_synthetic_thresholds })
    $summary = @{ status = 'complete'; completed_runs = $results.Count; failed_runs = $failedRuns.Count
        passed_synthetic_thresholds = $failedRuns.Count -eq 0; binary_sha256 = $candidateHash }
    [IO.File]::WriteAllText((Join-Path $recordDir 'summary.json'), ($summary | ConvertTo-Json), $utf8)
} catch {
    $failed = $_
    # An abort that carries no fixture annotation still has to leave a record.
    $fixtureLink = $null
    try { $fixtureLink = $failed.Exception.Data['recovery_fixture_directory'] } catch { }
    $failure = @{ status = 'incomplete'; error = $failed.Exception.Message; completed_runs = $results.Count
        binary_sha256 = $candidateHash; fixture_directory = $fixtureLink }
    [IO.File]::WriteAllText((Join-Path $recordDir 'failure.json'), ($failure | ConvertTo-Json), $utf8)
    throw $failed
} finally {
    Write-Host "Matrix plan, raw reports and outcomes retained: $recordDir"
}
Write-Output (ConvertTo-Json -InputObject $results.ToArray() -Depth 8)
if ($failedRuns.Count -gt 0) { exit 1 }
