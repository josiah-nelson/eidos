# Fixed gates for recovery-matrix.ps1. Passing is synthetic evidence only,
# never an installed, physical-media or recommended-profile qualification.
function ConvertTo-RecoveryNumber {
    param($Value, [switch]$Count)
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
    if ($Count -and ($parsed -lt 0 -or $parsed -ne [Math]::Truncate($parsed))) { throw 'invalid count field' }
    $parsed
}

function Get-RecoveryActivePauseThresholds {
    [ordered]@{
        response_ms = 150
        extraction_drain_seconds = 5
        hold_seconds_minimum = 3
    }
}

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
        [Parameter(Mandatory)]$Tuple,
        $Fixture = @{ files = 772; bytes = 11642404; sources = 2; small_files = 384; small_kib = 0; large_files = 2; large_mib = 2 },
        $Queries = $null
    )
    Set-StrictMode -Version Latest
    $limits = Get-RecoveryThresholds
    $checks = [ordered]@{}
    $rejection = $null
    # Fail closed on missing fields, NaN, infinity, malformed numeric strings
    # or booleans disguised as numbers. Wire u64 fields are decimal strings.
    function Number($Value) {
        ConvertTo-RecoveryNumber $Value
    }
    function Count($Value) {
        ConvertTo-RecoveryNumber $Value -Count
    }
    function TrueBoolean($Value) { $Value -is [bool] -and $Value }
    try {
        $checks.binary = $Report.binary_sha256 -is [string] -and $Report.binary_sha256 -ceq $ExpectedHash
        if ($null -ne $Queries) {
            $checks.queries = $Report.foreground_query -is [string] -and
                $Report.foreground_query -ceq $Queries.foreground -and
                $Report.completeness_query -is [string] -and
                $Report.completeness_query -ceq $Queries.completeness
        }
        $checks.fixture = (Number $Report.fixture_files) -eq $Fixture.files -and
            (Number $Report.fixture_bytes) -eq $Fixture.bytes -and
            (Number $Report.source_count) -eq $Fixture.sources -and
            (Number $Report.small_files_per_source) -eq $Fixture.small_files -and
            (Number $Report.large_files_per_source) -eq $Fixture.large_files -and
            (Number $Report.large_file_mib) -eq $Fixture.large_mib -and
            ($Fixture.small_kib -eq 0 -or (Number $Report.small_file_kib) -eq $Fixture.small_kib)
        $checks.settings = (Number $Report.content_workers) -eq $Tuple.workers -and
            (Number $Report.scan_threads) -eq $Tuple.scan_threads -and
            (Number $Report.concurrent_scans) -eq $Tuple.concurrent_scans -and
            (Number $Report.device_reader_limit) -eq $Tuple.device_readers -and
            (Number $Report.source_readers) -eq 4 -and
            (Number $Report.query_interval_ms) -eq 25
        $seconds = Number $Report.crawl.seconds
        $checks.crawl_and_indexed = $seconds -gt 0 -and $seconds -le $limits.crawl_seconds -and
            (Number $Report.files_indexed) -eq $Fixture.files
        $checks.search_results = (TrueBoolean $Report.search_total.exact) -and
            (Count $Report.search_total.value) -eq $Fixture.files
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
            (TrueBoolean $restart.retained_search_total.exact) -and (Count $restart.retained_search_total.value) -eq $Fixture.files
        $checks.valid_report = $true
    } catch {
        # A closed gate must say what it could not read, not only that it closed.
        $checks.valid_report = $false
        $rejection = $_.Exception.Message
    }
    $failed = @($checks.Keys | Where-Object { -not $checks[$_] })
    [pscustomobject]@{
        passed_synthetic_thresholds = $failed.Count -eq 0
        failed_checks = $failed
        rejected_because = $rejection
        checks = $checks
        qualification = 'synthetic evidence only; no recommended profile or deployment qualification'
    }
}

function Test-RecoveryActivePause {
    param([Parameter(Mandatory)]$Pause, [Parameter(Mandatory)]$Tuple)
    Set-StrictMode -Version Latest
    $limits = Get-RecoveryActivePauseThresholds
    $checks = [ordered]@{}
    $rejection = $null
    try {
        $checks.performed = $Pause.performed -is [bool] -and $Pause.performed
        $inFlight = ConvertTo-RecoveryNumber $Pause.in_flight_at_pause -Count
        $queuedBefore = ConvertTo-RecoveryNumber $Pause.queued_before_pause -Count
        $queuedAfter = ConvertTo-RecoveryNumber $Pause.queued_after_drain -Count
        $largeFile = $false
        if ($Pause.observed_files -isnot [Array]) { throw 'missing observed files' }
        foreach ($file in $Pause.observed_files) {
            if ((ConvertTo-RecoveryNumber $file.size -Count) -ge 1MB) { $largeFile = $true }
        }
        $checks.active_backlog = $inFlight -ge 1 -and $inFlight -le $Tuple.workers -and
            $queuedBefore -gt $Tuple.workers -and $queuedAfter -gt 0 -and $largeFile
        $response = ConvertTo-RecoveryNumber $Pause.pause_response_ms
        $drain = ConvertTo-RecoveryNumber $Pause.extraction_drain_seconds
        $hold = ConvertTo-RecoveryNumber $Pause.hold_seconds
        $total = ConvertTo-RecoveryNumber $Pause.total_seconds
        $checks.response = $response -ge 0 -and $response -le $limits.response_ms
        $checks.drain = $drain -ge 0 -and $drain -le $limits.extraction_drain_seconds
        $checks.held = $hold -ge $limits.hold_seconds_minimum -and
            $Pause.extraction_stayed_stopped -is [bool] -and $Pause.extraction_stayed_stopped
        $checks.timing = $total -ge ($response / 1000 + $drain + $hold)
        $checks.resumed = $Pause.resumed -is [bool] -and $Pause.resumed
        $checks.valid_report = $true
    } catch {
        # A closed gate must say what it could not read, not only that it closed.
        $checks.valid_report = $false
        $rejection = $_.Exception.Message
    }
    $failed = @($checks.Keys | Where-Object { -not $checks[$_] })
    [pscustomobject]@{
        passed_synthetic_thresholds = $failed.Count -eq 0
        failed_checks = $failed
        rejected_because = $rejection
        checks = $checks
    }
}
