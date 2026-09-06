# Dependency-free tests: invented report only, no service/process/file writes.
$ErrorActionPreference = 'Stop'
foreach ($script in @('recovery-acceptance.ps1', 'recovery-matrix.ps1', 'recovery-smoke.ps1')) {
    $tokens = $null
    $parseErrors = $null
    [System.Management.Automation.Language.Parser]::ParseFile((Join-Path $PSScriptRoot $script),
        [ref]$tokens, [ref]$parseErrors) | Out-Null
    if ($parseErrors.Count) { throw "$script syntax errors: $($parseErrors -join '; ')" }
}
. (Join-Path $PSScriptRoot 'recovery-acceptance.ps1')
$tuple = [pscustomobject]@{ workers = 4; scan_threads = 4; concurrent_scans = 2; device_readers = 4 }
$fixture = @{
    binary_sha256 = 'synthetic-hash'; fixture_files = 772; fixture_bytes = 11642404
    source_count = 2; small_files_per_source = 384; large_files_per_source = 2; large_file_mib = 2
    content_workers = 4; scan_threads = 4; concurrent_scans = 2; device_reader_limit = 4; source_readers = 4
    query_interval_ms = 25; crawl = @{ seconds = 7 }; files_indexed = '772'
    search_total = @{ exact = $true; value = '772' }
    max_observed_device_reservations = 4; resolved_device_observed = $true; resolved_shared_device_observed = $true
    http_query_samples = 100; crawl_query_tail_sample_sufficient = $true; http_query_p95_ms = 150; http_query_p99_ms = 250
    http_query_latencies_ms = @(@(10) * 94 + @(150) * 4 + @(250) * 2)
    memory_after_idle = @{ process = @{ peak_resident_bytes = '536870912' } }
    catalog_writer_after_crawl = @{ max_hold_ms = 500 }
    idle = @{ seconds = 15; cpu_one_core_percent = 1; process_read_bytes = 4MB; process_write_bytes = 0 }
    restart_after_idle = @{ performed = $true; pause_and_limits_preserved = $true; resumed = $true; retained_search_hits = 10
        retained_search_total = @{ exact = $true; value = '772' } }
} | ConvertTo-Json -Depth 8
$script:caseCount = 0
function Check([scriptblock]$Mutate, [string]$ExpectedFailure = '') {
    $sample = $fixture | ConvertFrom-Json
    & $Mutate $sample
    $result = Test-RecoveryMeasurement -Report $sample -ExpectedHash 'synthetic-hash' -Tuple $tuple
    if ($ExpectedFailure) {
        if ($result.passed_synthetic_thresholds -or $ExpectedFailure -notin $result.failed_checks) {
            throw "Expected failure '$ExpectedFailure': $($result | ConvertTo-Json -Depth 5 -Compress)"
        }
    } elseif (-not $result.passed_synthetic_thresholds) { throw 'Boundary fixture must pass' }
    $script:caseCount++
}
Check { param($r) }
Check { param($r) $r.binary_sha256 = 'different-build' } 'binary'
Check { param($r) $r.binary_sha256 = @('synthetic-hash') } 'binary'
Check { param($r) $r.fixture_files = 771 } 'fixture'
Check { param($r) $r.source_readers = 2 } 'settings'
Check { param($r) $r.crawl.seconds = 180.1 } 'crawl_and_indexed'
Check { param($r) $r.files_indexed = '771' } 'crawl_and_indexed'
Check { param($r) $r.search_total.value = '771' } 'search_results'
Check { param($r) $r.search_total.exact = 'true' } 'search_results'
Check { param($r) $r.max_observed_device_reservations = 5 } 'device_admission'
Check { param($r) $r.resolved_shared_device_observed = $false } 'device_admission'
Check { param($r) $r.http_query_samples = 99 } 'query_samples'
Check { param($r) $r.http_query_samples = 100.5 } 'valid_report'
Check { param($r) $r.max_observed_device_reservations = 1.5 } 'valid_report'
Check { param($r) $r.http_query_samples = 101 } 'query_measurements'
Check { param($r) $r.http_query_p95_ms = 149 } 'query_measurements'
Check { param($r) $r.http_query_latencies_ms = @() } 'valid_report'
Check { param($r) $r.http_query_latencies_ms[0] = -1 } 'valid_report'
Check { param($r) $r.http_query_latencies_ms[0] = $null } 'valid_report'
Check { param($r) $r.http_query_p99_ms = 250.1 } 'query_latency'
Check { param($r) $r.memory_after_idle.process.peak_resident_bytes = '536870913' } 'memory'
Check { param($r) $r.catalog_writer_after_crawl.max_hold_ms = 500.1 } 'writer_hold'
Check { param($r) $r.idle.seconds = 14.9 } 'idle_duration'
Check { param($r) $r.idle.cpu_one_core_percent = 1.01 } 'idle_cpu'
Check { param($r) $r.idle.process_write_bytes = 1 } 'idle_transfers'
Check { param($r) $r.idle.process_write_bytes = -1 } 'idle_transfers'
Check { param($r) $r.idle.process_read_bytes = 1.5 } 'idle_transfers'
Check { param($r) $r.restart_after_idle.resumed = $false } 'restart'
Check { param($r) $r.restart_after_idle.performed = 'true' } 'restart'
Check { param($r) $r.restart_after_idle.retained_search_total.value = '771' } 'restart'
Check { param($r) $r.PSObject.Properties.Remove('idle') } 'valid_report'
Check { param($r) $r.memory_after_idle.process.peak_resident_bytes = $null } 'valid_report'
Check { param($r) $r.http_query_p95_ms = 'NaN' } 'valid_report'
Check { param($r) $r.idle.cpu_one_core_percent = $true } 'valid_report'
Check { param($r) $r.idle.cpu_one_core_percent = @(0.5) } 'valid_report'
Check { param($r) $r.http_query_p95_ms = 'not-a-number' } 'valid_report'
Check { param($r) $r.idle.cpu_one_core_percent = [double]::PositiveInfinity } 'valid_report'
Check { param($r) $r.crawl.seconds = 180; $r.idle.cpu_one_core_percent = '0.5' }
# JSON numbers must not depend on the machine's decimal separator.
$previousCulture = [Threading.Thread]::CurrentThread.CurrentCulture
try {
    [Threading.Thread]::CurrentThread.CurrentCulture = [Globalization.CultureInfo]::GetCultureInfo('fr-FR')
    Check { param($r) $r.crawl.seconds = 7.25; $r.idle.cpu_one_core_percent = 0.5 }
} finally { [Threading.Thread]::CurrentThread.CurrentCulture = $previousCulture }
Write-Output "Recovery acceptance: $script:caseCount boundary/failure cases passed."
