# Pure report/schema tests; no executable, fixture or installed service runs.
$ErrorActionPreference = 'Stop'
$tokens = $null
$parseErrors = $null
[System.Management.Automation.Language.Parser]::ParseFile((Join-Path $PSScriptRoot 'recovery-workload.ps1'),
    [ref]$tokens, [ref]$parseErrors) | Out-Null
if ($parseErrors.Count) { throw "Workload syntax errors: $($parseErrors -join '; ')" }
. (Join-Path $PSScriptRoot 'recovery-acceptance.ps1')
$tuple = @{ workers = 2; scan_threads = 2; concurrent_scans = 1; device_readers = 2 }
$fixture = @{ files = 2056; bytes = 201326592; sources = 2; small_files = 1024; small_kib = 64; large_files = 4; large_mib = 8 }
$report = @{
    binary_sha256 = 'synthetic'; fixture_files = 2056; fixture_bytes = 201326592; source_count = 2
    small_files_per_source = 1024; small_file_kib = 64; large_files_per_source = 4; large_file_mib = 8
    content_workers = 2; scan_threads = 2; concurrent_scans = 1; device_reader_limit = 2; source_readers = 4
    query_interval_ms = 25; crawl = @{ seconds = 180 }; files_indexed = '2056'
    foreground_query = 'content:recoveryneedle'; completeness_query = 'content:=recoveryneedle'
    search_total = @{ exact = $true; value = '2056' }; max_observed_device_reservations = 2
    resolved_device_observed = $true; resolved_shared_device_observed = $true
    http_query_samples = 100; http_query_latencies_ms = @(10) * 100
    crawl_query_tail_sample_sufficient = $true; http_query_p95_ms = 10; http_query_p99_ms = 10
    memory_after_idle = @{ process = @{ peak_resident_bytes = '536870912' } }
    catalog_writer_after_crawl = @{ max_hold_ms = 500 }
    idle = @{ seconds = 30; cpu_one_core_percent = 1; process_read_bytes = 4MB; process_write_bytes = 0 }
    restart_after_idle = @{ performed = $true; pause_and_limits_preserved = $true; resumed = $true
        retained_search_hits = 10; retained_search_total = @{ exact = $true; value = '2056' } }
} | ConvertTo-Json -Depth 8 | ConvertFrom-Json
$script:fixtureCases = 0
function CheckFixture([scriptblock]$Assert) { & $Assert; $script:fixtureCases++ }
CheckFixture {
    if (-not (Test-RecoveryMeasurement -Report $report -ExpectedHash 'synthetic' -Tuple $tuple -Fixture $fixture).passed_synthetic_thresholds) {
        throw 'Larger fixture boundary must pass'
    }
}
CheckFixture {
    $report.small_file_kib = 4
    if ((Test-RecoveryMeasurement -Report $report -ExpectedHash 'synthetic' -Tuple $tuple -Fixture $fixture).passed_synthetic_thresholds) {
        throw 'Wrong small-file workload must fail'
    }
    $report.small_file_kib = 64
}
$queries = @{ foreground = 'content:recoveryneedle'; completeness = 'content:=recoveryneedle' }
CheckFixture {
    if (-not (Test-RecoveryMeasurement -Report $report -ExpectedHash 'synthetic' -Tuple $tuple -Fixture $fixture -Queries $queries).passed_synthetic_thresholds) {
        throw 'Separate ranked foreground and exact completeness queries must pass'
    }
}
CheckFixture {
    $report.completeness_query = 'content:recoveryneedle'
    if ((Test-RecoveryMeasurement -Report $report -ExpectedHash 'synthetic' -Tuple $tuple -Fixture $fixture -Queries $queries).passed_synthetic_thresholds) {
        throw 'A capped ranked completeness query must fail the declared workload'
    }
    $report.completeness_query = 'content:=recoveryneedle'
}
CheckFixture {
    $report.restart_after_idle.retained_search_total.value = '772'
    if ((Test-RecoveryMeasurement -Report $report -ExpectedHash 'synthetic' -Tuple $tuple -Fixture $fixture).passed_synthetic_thresholds) {
        throw 'Old small-fixture restart total must fail'
    }
    $report.restart_after_idle.retained_search_total.value = '2056'
}
# The boundary report sits exactly on every published active-pause threshold.
$pauseLimits = Get-RecoveryActivePauseThresholds
$largeFile = @{ size = "$($pauseLimits.large_file_bytes)" }
$pauseJson = @{
    schema = Get-RecoveryActivePauseSchema
    performed = $true; in_flight_at_pause = 2; queued_before_pause = '3'; queued_after_drain = '1'
    observed_files = @($largeFile); in_flight_files_at_pause = @($largeFile)
    pause_response_ms = $pauseLimits.response_ms
    extraction_drain_seconds = $pauseLimits.extraction_drain_seconds
    hold_seconds = $pauseLimits.hold_seconds_minimum; total_seconds = 8.15
    extraction_stayed_stopped = $true; resumed = $true
} | ConvertTo-Json -Depth 5
$script:pauseCases = 0
function CheckPause([scriptblock]$Mutate, [string]$Failure = '') {
    $pause = $pauseJson | ConvertFrom-Json
    & $Mutate $pause
    $result = Test-RecoveryActivePause -Pause $pause -Tuple $tuple
    if ($Failure) {
        if ($result.passed_synthetic_thresholds -or $Failure -notin $result.failed_checks) { throw "Expected active-pause failure: $Failure" }
        # A gate that closes on an unreadable field must name the field.
        if ($Failure -eq 'valid_report' -and -not $result.rejected_because) {
            throw 'A rejected active-pause report must say what could not be read'
        }
        if ($Failure -ne 'valid_report' -and $result.rejected_because) {
            throw 'A readable active-pause report must not claim a rejection'
        }
    } elseif (-not $result.passed_synthetic_thresholds -or $result.rejected_because) {
        throw 'Active-pause boundary must pass'
    }
    $script:pauseCases++
}
CheckPause { param($p) }
CheckPause { param($p) $p.performed = 'true' } 'performed'
CheckPause { param($p) $p.in_flight_at_pause = 0 } 'active_backlog'
CheckPause { param($p) $p.in_flight_at_pause = 3 } 'active_backlog'
CheckPause { param($p) $p.in_flight_at_pause = 0.5 } 'valid_report'
CheckPause { param($p) $p.queued_before_pause = '2' } 'active_backlog'
CheckPause { param($p) $p.queued_after_drain = '0' } 'active_backlog'
CheckPause { param($p) $p.observed_files[0].size = "$($pauseLimits.large_file_bytes - 1)" } 'large_file_before_pause'
CheckPause { param($p) $p.observed_files = 'one file' } 'valid_report'
# The evidence the whole claim rests on: what was still extracting once the
# pause was acknowledged, not what was seen before the request was sent.
CheckPause { param($p) $p.in_flight_files_at_pause[0].size = "$($pauseLimits.large_file_bytes - 1)" } 'large_file_at_pause'
CheckPause { param($p) $p.in_flight_files_at_pause = @() } 'valid_report'
CheckPause { param($p) $p.in_flight_files_at_pause = @($largeFile, $largeFile, $largeFile) } 'large_file_at_pause'
CheckPause { param($p) $p.pause_response_ms = $pauseLimits.response_ms + 0.1 } 'response'
CheckPause { param($p) $p.extraction_drain_seconds = $pauseLimits.extraction_drain_seconds + 0.1 } 'drain'
CheckPause { param($p) $p.hold_seconds = $pauseLimits.hold_seconds_minimum - 0.1 } 'held'
CheckPause { param($p) $p.extraction_stayed_stopped = 'true' } 'held'
CheckPause { param($p) $p.resumed = $false } 'resumed'
CheckPause { param($p) $p.pause_response_ms = 'NaN' } 'valid_report'
CheckPause { param($p) $p.hold_seconds = @(3) } 'valid_report'
CheckPause { param($p) $p.total_seconds = 3 } 'timing'
# A schema-1 record keeps its counters but is named as historical, never
# waived through this gate to keep an older measurement passing.
CheckPause { param($p) $p.PSObject.Properties.Remove('schema') } 'valid_report'
CheckPause { param($p) $p.schema = 1 } 'schema'
CheckPause { param($p) $p.PSObject.Properties.Remove('in_flight_files_at_pause') } 'valid_report'
Write-Output "Recovery workload: $script:fixtureCases fixture/query and $script:pauseCases active-pause cases passed."
