# Seven bounded runs: six uninterrupted comparisons and one active pause.
# This is synthetic development evidence, not installed/media qualification.
param([Parameter(Mandatory)][string]$Binary)
$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'This workload uses Windows process counters.' }
. (Join-Path $PSScriptRoot 'recovery-acceptance.ps1')
$sourceBinary = (Resolve-Path -LiteralPath $Binary).Path
$binaryHash = (Get-FileHash -LiteralPath $sourceBinary -Algorithm SHA256).Hash
$recordDir = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot ('../bench-results/workload-' + [guid]::NewGuid().ToString('N'))))
New-Item -ItemType Directory -Path $recordDir | Out-Null
$binarySnapshot = Join-Path $recordDir 'candidate.exe'
Copy-Item -LiteralPath $sourceBinary -Destination $binarySnapshot
$utf8 = [Text.UTF8Encoding]::new($false)
$fixture = @{ files = 2056; bytes = 201326592; sources = 2; small_files = 1024; small_kib = 64; large_files = 4; large_mib = 8 }
$tuples = @(
    @{ workers = 1; scan_threads = 1; concurrent_scans = 1; device_readers = 1 },
    @{ workers = 2; scan_threads = 2; concurrent_scans = 1; device_readers = 2 },
    @{ workers = 4; scan_threads = 4; concurrent_scans = 2; device_readers = 4 }
)
$order = @(0, 1, 2, 2, 1, 0, 1)
# The active pause is the last run, so the plan, the loop and the progress line
# cannot disagree about how many runs there are or which one is interrupted.
$runCount = $order.Count
$activePauseRun = $runCount
$plan = [ordered]@{
    planned_at_utc = [DateTime]::UtcNow.ToString('o'); binary_sha256 = $binaryHash; binary_snapshot = $binarySnapshot
    conditions = 'non-isolated Windows host; sibling builds and normal native host-volume activity may overlap'
    qualification = 'synthetic development comparison; no recommended profiles, physical-media or installed qualification'
    fixture = $fixture; tuples = $tuples; run_order = $order; active_pause_run = $activePauseRun
    source_readers = 4; query_interval_ms = 25; idle_seconds = 30; check_restart = $true
    # Declared here and compared against what recovery-smoke.ps1 reports it
    # issued; deliberately not shared with it, so a harness that changed which
    # query it ran would fail this plan instead of rewriting it.
    foreground_query = 'content:recoveryneedle'; completeness_query = 'content:=recoveryneedle'
    maximum_generated_source_bytes = $runCount * $fixture.bytes
    thresholds = Get-RecoveryThresholds
    active_pause_thresholds = Get-RecoveryActivePauseThresholds
}
[IO.File]::WriteAllText((Join-Path $recordDir 'plan.json'), ($plan | ConvertTo-Json -Depth 8), $utf8)
$results = [Collections.Generic.List[object]]::new()
try {
    for ($run = 0; $run -lt $order.Count; $run++) {
        if ((Get-FileHash -LiteralPath $binarySnapshot -Algorithm SHA256).Hash -cne $binaryHash) {
            throw 'Workload candidate changed; refusing mixed-build evidence.'
        }
        $tuple = $tuples[$order[$run]]
        $checkPause = ($run + 1) -eq $activePauseRun
        Write-Host "Workload run $($run + 1)/$runCount`: $($tuple.workers)/$($tuple.scan_threads)/$($tuple.concurrent_scans)/$($tuple.device_readers); active pause=$checkPause"
        $raw = & (Join-Path $PSScriptRoot 'recovery-smoke.ps1') -Binary $binarySnapshot `
            -Files 1024 -SmallFileKiB 64 -SourceCount 2 -ContentWorkers $tuple.workers -ScanThreads $tuple.scan_threads `
            -ConcurrentScans $tuple.concurrent_scans -DeviceReaders $tuple.device_readers -SourceReaders 4 `
            -LargeFilesPerSource 4 -LargeFileMiB 8 -QueryIntervalMilliseconds 25 -IdleSeconds 30 -CheckRestart -CheckActivePause:$checkPause
        $json = $raw -join [Environment]::NewLine
        [IO.File]::WriteAllText((Join-Path $recordDir "run-$($run + 1).json"), $json, $utf8)
        $report = $json | ConvertFrom-Json
        if ((Get-FileHash -LiteralPath $binarySnapshot -Algorithm SHA256).Hash -cne $binaryHash) {
            throw 'Workload candidate changed during a run.'
        }
        $evaluation = Test-RecoveryMeasurement -Report $report -ExpectedHash $binaryHash -Tuple $tuple -Fixture $fixture -Queries @{
            foreground = $plan.foreground_query; completeness = $plan.completeness_query }
        $pauseEvaluation = if ($checkPause) { Test-RecoveryActivePause -Pause $report.active_pause -Tuple $tuple } else { $null }
        $passed = $evaluation.passed_synthetic_thresholds -and (-not $checkPause -or $pauseEvaluation.passed_synthetic_thresholds)
        $results.Add([pscustomobject]@{
            run = $run + 1; tuple = $tuple; includes_active_pause = $checkPause; passed_synthetic_thresholds = $passed
            evaluation = $evaluation; active_pause_evaluation = $pauseEvaluation
            crawl_seconds = $report.crawl.seconds; query_samples = $report.http_query_samples
            query_p95_ms = $report.http_query_p95_ms; query_p99_ms = $report.http_query_p99_ms
            idle_cpu_one_core_percent = $report.idle.cpu_one_core_percent
            idle_process_transfer_bytes = [double]$report.idle.process_read_bytes + [double]$report.idle.process_write_bytes
        })
        [IO.File]::WriteAllText((Join-Path $recordDir 'results.json'), (ConvertTo-Json -InputObject $results.ToArray() -Depth 8), $utf8)
        Write-Host "Run $($run + 1): pass=$passed; failed=$($evaluation.failed_checks -join ', '); active-pause=$($pauseEvaluation.failed_checks -join ', ')"
        # Both gates fail closed on an unreadable field; neither may swallow why.
        if ($evaluation.rejected_because) { Write-Host "Run $($run + 1) report rejected: $($evaluation.rejected_because)" }
        if ($pauseEvaluation.rejected_because) { Write-Host "Run $($run + 1) active pause rejected: $($pauseEvaluation.rejected_because)" }
    }
    $failed = @($results | Where-Object { -not $_.passed_synthetic_thresholds })
    $summary = @{ status = 'complete'; completed_runs = $results.Count; failed_runs = $failed.Count
        passed_synthetic_thresholds = $failed.Count -eq 0; binary_sha256 = $binaryHash }
    [IO.File]::WriteAllText((Join-Path $recordDir 'summary.json'), ($summary | ConvertTo-Json), $utf8)
} catch {
    $failedRun = $_
    $failedFixture = $null
    try { $failedFixture = $failedRun.Exception.Data['recovery_fixture_directory'] } catch { }
    $failure = @{ status = 'incomplete'; completed_runs = $results.Count; error = $failedRun.Exception.Message
        binary_sha256 = $binaryHash; fixture_directory = $failedFixture }
    [IO.File]::WriteAllText((Join-Path $recordDir 'failure.json'), ($failure | ConvertTo-Json), $utf8)
    throw $failedRun
} finally { Write-Host "Workload plan, binary, reports and outcomes retained: $recordDir" }
Write-Output (ConvertTo-Json -InputObject $results.ToArray() -Depth 8)
if ($failed.Count) { exit 1 }
