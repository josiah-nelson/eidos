# Fixed gates for recovery-matrix.ps1. Passing is synthetic evidence only,
# never an installed, physical-media or recommended-profile qualification.
function Get-RecoveryThresholds {
    [ordered]@{
        crawl_seconds = 180
        query_samples_minimum = 100
        query_p95_ms = 150
        query_p99_ms = 250
        peak_resident_bytes = 512MB
        writer_hold_ms = 500
        idle_seconds_minimum = 15
        idle_cpu_one_core_percent = 1
        idle_process_transfer_bytes = 4MB
    }
}

function Test-RecoveryMeasurement {
    param(
        [Parameter(Mandatory)]$Report,
        [Parameter(Mandatory)][string]$ExpectedHash,
        [Parameter(Mandatory)]$Tuple
    )
    Set-StrictMode -Version Latest
    $limits = Get-RecoveryThresholds
    $checks = [ordered]@{}
    # Fail closed on missing fields, NaN, infinity, malformed numeric strings
    # or booleans disguised as numbers. Wire u64 fields are decimal strings.
    function Number($Value) {
        if ($null -eq $Value -or [Type]::GetTypeCode($Value.GetType()) -notin @(
                'String', 'Byte', 'SByte', 'Int16', 'UInt16', 'Int32', 'UInt32',
                'Int64', 'UInt64', 'Single', 'Double', 'Decimal')) {
            throw 'missing or nonnumeric scalar field'
        }
        $parsed = 0.0
        $text = [Convert]::ToString($Value, [Globalization.CultureInfo]::InvariantCulture)
        if (-not [double]::TryParse($text, [Globalization.NumberStyles]::Float,
                [Globalization.CultureInfo]::InvariantCulture, [ref]$parsed) -or
                [double]::IsNaN($parsed) -or [double]::IsInfinity($parsed)) {
            throw 'invalid numeric field'
        }
        $parsed
    }
    function Count($Value) {
        $parsed = Number $Value
        if ($parsed -lt 0 -or $parsed -ne [Math]::Truncate($parsed)) { throw 'invalid count field' }
        $parsed
    }
    function TrueBoolean($Value) { $Value -is [bool] -and $Value }
    try {
        $checks.binary = $Report.binary_sha256 -is [string] -and $Report.binary_sha256 -ceq $ExpectedHash
        $checks.fixture = (Number $Report.fixture_files) -eq 772 -and
            (Number $Report.fixture_bytes) -eq 11642404 -and
            (Number $Report.source_count) -eq 2 -and
            (Number $Report.small_files_per_source) -eq 384 -and
            (Number $Report.large_files_per_source) -eq 2 -and
            (Number $Report.large_file_mib) -eq 2
        $checks.settings = (Number $Report.content_workers) -eq $Tuple.workers -and
            (Number $Report.scan_threads) -eq $Tuple.scan_threads -and
            (Number $Report.concurrent_scans) -eq $Tuple.concurrent_scans -and
            (Number $Report.device_reader_limit) -eq $Tuple.device_readers -and
            (Number $Report.source_readers) -eq 4 -and
            (Number $Report.query_interval_ms) -eq 25
        $seconds = Number $Report.crawl.seconds
        $checks.crawl_and_indexed = $seconds -gt 0 -and $seconds -le $limits.crawl_seconds -and
            (Number $Report.files_indexed) -eq 772
        $checks.search_results = (TrueBoolean $Report.search_total.exact) -and
            (Count $Report.search_total.value) -eq 772
        $peakReaders = Count $Report.max_observed_device_reservations
        $checks.device_admission = $peakReaders -ge 1 -and $peakReaders -le $Tuple.device_readers -and
            (TrueBoolean $Report.resolved_device_observed) -and
            (TrueBoolean $Report.resolved_shared_device_observed)
        $sampleCount = Count $Report.http_query_samples
        $checks.query_samples = $sampleCount -ge $limits.query_samples_minimum -and
            (TrueBoolean $Report.crawl_query_tail_sample_sufficient)
        if ($Report.http_query_latencies_ms -isnot [Array] -or $Report.http_query_latencies_ms.Count -eq 0) {
            throw 'missing crawl query samples'
        }
        $samples = @($Report.http_query_latencies_ms | ForEach-Object {
            $sample = Number $_
            if ($sample -lt 0) { throw 'negative query latency' }
            $sample
        } | Sort-Object)
        $p95 = Number $Report.http_query_p95_ms
        $p99 = Number $Report.http_query_p99_ms
        $checks.query_measurements = $samples.Count -eq $sampleCount -and
            $p95 -eq $samples[[Math]::Ceiling($samples.Count * 0.95) - 1] -and
            $p99 -eq $samples[[Math]::Ceiling($samples.Count * 0.99) - 1]
        $checks.query_latency = $p95 -ge 0 -and $p95 -le $limits.query_p95_ms -and
            $p99 -ge $p95 -and $p99 -le $limits.query_p99_ms
        $peakResident = Count $Report.memory_after_idle.process.peak_resident_bytes
        $checks.memory = $peakResident -gt 0 -and $peakResident -le $limits.peak_resident_bytes
        $hold = Number $Report.catalog_writer_after_crawl.max_hold_ms
        $checks.writer_hold = $hold -ge 0 -and $hold -le $limits.writer_hold_ms
        $idleSeconds = Number $Report.idle.seconds
        $idleCpu = Number $Report.idle.cpu_one_core_percent
        $idleRead = Number $Report.idle.process_read_bytes
        $idleWrite = Number $Report.idle.process_write_bytes
        $checks.idle_duration = $idleSeconds -ge $limits.idle_seconds_minimum
        $checks.idle_cpu = $idleCpu -ge 0 -and $idleCpu -le $limits.idle_cpu_one_core_percent
        $checks.idle_transfers = $idleRead -ge 0 -and $idleWrite -ge 0 -and
            $idleRead -eq [Math]::Truncate($idleRead) -and $idleWrite -eq [Math]::Truncate($idleWrite) -and
            ($idleRead + $idleWrite) -le $limits.idle_process_transfer_bytes
        $restart = $Report.restart_after_idle
        $checks.restart = (TrueBoolean $restart.performed) -and
            (TrueBoolean $restart.pause_and_limits_preserved) -and
            (TrueBoolean $restart.resumed) -and (Number $restart.retained_search_hits) -eq 10 -and
            (TrueBoolean $restart.retained_search_total.exact) -and (Count $restart.retained_search_total.value) -eq 772
        $checks.valid_report = $true
    } catch {
        $checks.valid_report = $false
    }
    $failed = @($checks.Keys | Where-Object { -not $checks[$_] })
    [pscustomobject]@{
        passed_synthetic_thresholds = $failed.Count -eq 0
        failed_checks = $failed
        checks = $checks
        qualification = 'synthetic evidence only; no recommended profile or deployment qualification'
    }
}
