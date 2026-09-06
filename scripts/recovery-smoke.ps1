# Bounded synthetic measurement, NOT release/physical-disk qualification.
# Launches only the specified binary with new temporary data on loopback.
# Retains fixture, logs and report; never touches an installed service.
param(
    [string]$Binary = (Join-Path $PSScriptRoot "../target/debug/eidos.exe"),
    [ValidateRange(32, 4096)][int]$Files = 1024,
    [ValidateRange(5, 30)][int]$IdleSeconds = 15,
    [ValidateRange(1, 64)][int]$DeviceReaders = 2,
    [ValidateRange(1, 2)][int]$SourceCount = 1,
    [ValidateRange(1, 8)][int]$ContentWorkers = 2,
    [ValidateRange(1, 8)][int]$ScanThreads = 2,
    [ValidateRange(1, 2)][int]$ConcurrentScans = 1,
    [ValidateRange(1, 8)][int]$SourceReaders = 2,
    [ValidateRange(0, 4)][int]$LargeFilesPerSource = 0,
    [ValidateRange(1, 8)][int]$LargeFileMiB = 2,
    [ValidateRange(25, 1000)][int]$QueryIntervalMilliseconds = 250,
    [switch]$CheckRestart
)
$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'This measurement script uses Windows process I/O counters.' }
$candidatePath = (Resolve-Path -LiteralPath $Binary).Path
$fixtureDir = Join-Path ([IO.Path]::GetTempPath()) ("eidos-recovery-" + [guid]::NewGuid().ToString('N'))
$dataDir = Join-Path $fixtureDir 'data'
$logDir = Join-Path $fixtureDir 'logs'
New-Item -ItemType Directory -Path $dataDir, $logDir | Out-Null
$utf8 = [Text.UTF8Encoding]::new($false)
$payload = ("recoveryneedle synthetic indexing fixture apple maple river cloud`n" * 64)
# Everything that can still fail runs inside the protected lifecycle below,
# so a fixture-generation, hashing or launch failure also records failure.json
# beside its retained fixture and names that fixture to the caller.
$sourceBytes = 0L
$totalFiles = 0
$sha = $null
$candidate = $null
function Counters {
    $p = Get-CimInstance Win32_Process -Filter "ProcessId = $($candidate.Id)"
    if (-not $p) { throw 'Synthetic service exited unexpectedly.' }
    [pscustomobject]@{
        cpu_s = ([double]$p.KernelModeTime + [double]$p.UserModeTime) / 10000000
        read_bytes = [double]$p.ReadTransferCount; write_bytes = [double]$p.WriteTransferCount
        read_ops = [double]$p.ReadOperationCount; write_ops = [double]$p.WriteOperationCount
        working_set_bytes = [double]$p.WorkingSetSize
    }
}
function Delta($before, $after, [double]$seconds) {
    [ordered]@{
        seconds = $seconds
        cpu_s = $after.cpu_s - $before.cpu_s
        cpu_one_core_percent = 100 * ($after.cpu_s - $before.cpu_s) / $seconds
        process_read_bytes = $after.read_bytes - $before.read_bytes
        process_write_bytes = $after.write_bytes - $before.write_bytes
        process_read_ops = $after.read_ops - $before.read_ops
        process_write_ops = $after.write_ops - $before.write_ops
        working_set_bytes_start = $before.working_set_bytes
        working_set_bytes_end = $after.working_set_bytes
    }
}
function Await-CandidateHealth {
    $deadline = [DateTime]::UtcNow.AddSeconds(30)
    $health = $null
    while ([DateTime]::UtcNow -lt $deadline) {
        if ($candidate.HasExited) { throw "Candidate failed startup; see $logDir" }
        try { $health = Invoke-RestMethod "$baseUrl/api/health" -TimeoutSec 2; break } catch { Start-Sleep -Milliseconds 100 }
    }
    if (-not $health) { throw 'Candidate did not become healthy.' }
    # A port race must never send mutations to an unrelated service.
    if ([IO.Path]::GetFullPath($health.catalog_path) -ne (Join-Path $dataDir 'catalog.db')) {
        throw 'Endpoint is not the temporary candidate; refusing to mutate it.'
    }
    $health
}
try {
    $sourceDirs = @(for ($root = 0; $root -lt $SourceCount; $root++) {
        $sourceDir = Join-Path $fixtureDir "source-$root"
        New-Item -ItemType Directory -Path $sourceDir | Out-Null
        for ($i = 0; $i -lt $Files; $i++) {
            $text = "document $i`n" + $payload
            [IO.File]::WriteAllText((Join-Path $sourceDir "document-$i.txt"), $text, $utf8)
            $sourceBytes += $utf8.GetByteCount($text)
        }
        for ($i = 0; $i -lt $LargeFilesPerSource; $i++) {
            # ASCII, bounded at 8 MiB per file / four files per root; no real corpus.
            $length = $LargeFileMiB * 1MB
            $text = ($payload * [int][Math]::Ceiling($length / $payload.Length)).Substring(0, $length)
            [IO.File]::WriteAllText((Join-Path $sourceDir "large-$i.txt"), $text, $utf8)
            $sourceBytes += $utf8.GetByteCount($text)
        }
        $sourceDir
    })
    $totalFiles = $SourceCount * ($Files + $LargeFilesPerSource)
    $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
    $listener.Start()
    $port = $listener.LocalEndpoint.Port
    $listener.Stop()
    $baseUrl = "http://127.0.0.1:$port"
    $arguments = @('serve', '--bind', "127.0.0.1:$port", '--data-dir', ('"' + $dataDir + '"'),
        '--log-dir', ('"' + $logDir + '"'), '--scan-threads', "$ScanThreads", '--content-workers', "$ContentWorkers",
        '--no-auto-reconcile', '--no-fleet', '--no-update-check')
    $sha = (Get-FileHash -LiteralPath $candidatePath -Algorithm SHA256).Hash
    $candidate = Start-Process -FilePath $candidatePath -ArgumentList $arguments -PassThru -WindowStyle Hidden
    $health = Await-CandidateHealth
    $saved = & $candidatePath resources --url $baseUrl --scan-threads $ScanThreads --concurrent-scans $ConcurrentScans --minimum-free-mib 1024 --json
    if ($LASTEXITCODE -ne 0) { throw 'Resource CLI round trip failed.' }
    if (($saved | ConvertFrom-Json).limits.scan_threads -ne $ScanThreads -or ($saved | ConvertFrom-Json).limits.concurrent_scans -ne $ConcurrentScans) { throw 'Resource limits were not saved.' }
    $deviceSaved = & $candidatePath resources --url $baseUrl --device-readers $DeviceReaders --json
    if ($LASTEXITCODE -ne 0 -or ($deviceSaved | ConvertFrom-Json).budget.readers_per_device -ne $DeviceReaders) {
        throw 'Device-limit CLI round trip failed.'
    }
    if ($CheckRestart) {
        $pool = Invoke-RestMethod "$baseUrl/api/content/workers" -Method Post -ContentType 'application/json' -TimeoutSec 10 -Body (
            @{ workers = $ContentWorkers } | ConvertTo-Json)
        if ([int]$pool.workers -ne $ContentWorkers) { throw 'Pool size was not saved.' }
    }
    # First touch of the on-demand sampler, against a genuinely cold cache; it
    # reads only the candidate's own process. One call must answer: the endpoint
    # waits for the refresh it starts rather than telling a caller to retry.
    $cold = Invoke-RestMethod "$baseUrl/api/memory" -TimeoutSec 5
    if (-not $cold.process -or $cold.process.pid -ne $candidate.Id) {
        throw 'A cold memory request did not return a sample of the temporary candidate.'
    }
    $sourceIds = @(for ($root = 0; $root -lt $SourceCount; $root++) {
        $added = Invoke-RestMethod "$baseUrl/api/sources" -Method Post -ContentType 'application/json' -TimeoutSec 10 -Body (
            @{ name = "recovery-fixture-$root"; root_path = $sourceDirs[$root]; scan = $false } | ConvertTo-Json)
        $id = $added.source.id
        $policy = Invoke-RestMethod "$baseUrl/api/sources/$id/content" -Method Post -ContentType 'application/json' -TimeoutSec 10 -Body (
            @{ enabled = $true; concurrency = $SourceReaders } | ConvertTo-Json)
        if ($policy.source.content_concurrency -ne $SourceReaders) { throw 'Source reader limit was not saved.' }
        $id
    })
    # Creation/explicit policy setup is excluded from the scan-to-drain clock.
    $before = Counters
    $clock = [Diagnostics.Stopwatch]::StartNew()
    foreach ($id in $sourceIds) {
        $null = Invoke-RestMethod "$baseUrl/api/sources/$id/scan" -Method Post -TimeoutSec 10
    }
    $latencies = [Collections.Generic.List[double]]::new()
    $deviceSamples = 0
    $maxDeviceReaders = 0
    $sawResolvedDevice = $false
    $sawSharedDevice = $false
    do {
        if ($clock.Elapsed.TotalSeconds -gt 180) { throw 'Synthetic content crawl did not drain in 180 seconds.' }
        $queryClock = [Diagnostics.Stopwatch]::StartNew()
        $null = Invoke-RestMethod "$baseUrl/api/search?q=content%3Arecoveryneedle&limit=10" -TimeoutSec 15
        $latencies.Add($queryClock.Elapsed.TotalMilliseconds)
        $devices = Invoke-RestMethod "$baseUrl/api/devices" -TimeoutSec 5
        $deviceSamples++
        $sawResolvedDevice = $sawResolvedDevice -or (-not $devices.budget.unresolved_shared_fallback -and $devices.budget.devices.Count -gt 0)
        foreach ($device in $devices.budget.devices) {
            if (-not $devices.budget.unresolved_shared_fallback -and $device.sources.Count -eq $SourceCount) { $sawSharedDevice = $true }
            $readers = [int]$device.content_readers + [int]$device.scan_threads
            $maxDeviceReaders = [Math]::Max($maxDeviceReaders, [Math]::Max($readers, [int]$device.peak_readers))
            if ($readers -gt $DeviceReaders -or [int]$device.peak_readers -gt $DeviceReaders) {
                throw 'Observed device reader reservations exceeded the saved ceiling.'
            }
        }
        $sources = @(foreach ($id in $sourceIds) { Invoke-RestMethod "$baseUrl/api/sources/$id" -TimeoutSec 10 })
        $unfinished = @($sources | Where-Object { $_.source.state -ne 'complete' }).Count
        if ($unfinished -gt 0) { Start-Sleep -Milliseconds $QueryIntervalMilliseconds }
    } while ($unfinished -gt 0)
    $elapsed = $clock.Elapsed.TotalSeconds
    $after = Counters
    $activity = Invoke-RestMethod "$baseUrl/api/activity" -TimeoutSec 10
    if ([long]$activity.jobs.queued -ne 0 -or [long]$activity.jobs.running -ne 0 -or [long]$activity.workers.pending_publish -ne 0) {
        throw 'Source reported complete before the pipeline drained.'
    }
    $search = Invoke-RestMethod "$baseUrl/api/search?q=content%3Arecoveryneedle&limit=10&count=exact" -TimeoutSec 15
    if ($search.hits.Count -ne 10 -or -not $search.total.exact -or [long]$search.total.value -ne $totalFiles -or
        [long]$activity.workers.files_indexed -ne $totalFiles -or [long]$activity.workers.files_failed -ne 0) {
        throw 'The completed synthetic fixture is not searchable as expected.'
    }
    # Let trailing commits/merges settle; no API polling during idle sample.
    Start-Sleep -Seconds 5
    $idleSourcesBefore = @(foreach ($id in $sourceIds) { Invoke-RestMethod "$baseUrl/api/sources/$id" -TimeoutSec 10 })
    $idleActivityBefore = Invoke-RestMethod "$baseUrl/api/activity" -TimeoutSec 10
    $idleBefore = Counters
    $idleClock = [Diagnostics.Stopwatch]::StartNew()
    Start-Sleep -Seconds $IdleSeconds
    $idleAfter = Counters
    $idleElapsed = $idleClock.Elapsed.TotalSeconds
    $idleActivity = Invoke-RestMethod "$baseUrl/api/activity" -TimeoutSec 10
    $idleSourcesAfter = @(foreach ($id in $sourceIds) { Invoke-RestMethod "$baseUrl/api/sources/$id" -TimeoutSec 10 })
    # Sample after the unpolled idle window, never during it. One CLI call must
    # be enough, and it must describe the process now: nothing has asked for a
    # sample since before the crawl, so the request has to wait for its own
    # refresh instead of returning that pre-crawl reading. Exercises the actual
    # CLI and the exact-string API counters.
    $memoryJson = & $candidatePath resources --url $baseUrl --memory --json
    if ($LASTEXITCODE -ne 0) { throw 'Memory CLI round trip failed.' }
    $memory = $memoryJson | ConvertFrom-Json
    if (-not $memory.process -or $memory.stale -or [long]$memory.sample_age_s -gt 5 -or $memory.process.pid -ne $candidate.Id -or [long]$memory.process.resident_bytes -le 0) {
        throw 'Memory API did not report a current sample of the temporary candidate in one call.'
    }
    $deviceJson = & $candidatePath resources --url $baseUrl --devices --json
    if ($LASTEXITCODE -ne 0) { throw 'Device diagnostics CLI failed.' }
    $devicesAfterIdle = $deviceJson | ConvertFrom-Json
    $restart = @{ performed = $false }
    if ($CheckRestart) {
        # Test a forced restart AFTER drain, not an installer upgrade or a
        # mid-file pause. All mutations still target only our captured process.
        $pauseClock = [Diagnostics.Stopwatch]::StartNew()
        $paused = Invoke-RestMethod "$baseUrl/api/content/pause" -Method Post -TimeoutSec 10
        $pauseMs = $pauseClock.Elapsed.TotalMilliseconds
        if (-not $paused.paused -or [int]$paused.in_flight -ne 0) { throw 'Drained candidate did not pause.' }
        $originalPid = $candidate.Id
        Stop-Process -InputObject $candidate -Force
        if (-not $candidate.WaitForExit(5000)) { throw 'Captured candidate did not exit.' }
        # Different startup defaults prove durable settings won over defaults.
        $restartArguments = $arguments.Clone()
        $restartArguments[[Array]::IndexOf($restartArguments, '--scan-threads') + 1] = if ($ScanThreads -eq 8) { '1' } else { '8' }
        $restartArguments[[Array]::IndexOf($restartArguments, '--content-workers') + 1] = if ($ContentWorkers -eq 8) { '1' } else { '8' }
        $restartClock = [Diagnostics.Stopwatch]::StartNew()
        $candidate = Start-Process -FilePath $candidatePath -ArgumentList $restartArguments -PassThru -WindowStyle Hidden
        $null = Await-CandidateHealth
        $restartReadyMs = $restartClock.Elapsed.TotalMilliseconds
        $persistedPause = Invoke-RestMethod "$baseUrl/api/content/status" -TimeoutSec 5
        $persistedResources = Invoke-RestMethod "$baseUrl/api/resources" -TimeoutSec 5
        $persistedDevices = Invoke-RestMethod "$baseUrl/api/devices" -TimeoutSec 5
        $persistedPool = Invoke-RestMethod "$baseUrl/api/content/workers" -TimeoutSec 5
        if (-not $persistedPause.paused -or [int]$persistedPause.in_flight -ne 0 -or
            $persistedResources.limits.scan_threads -ne $ScanThreads -or $persistedResources.limits.concurrent_scans -ne $ConcurrentScans -or
            $persistedResources.limits.minimum_free_mib -ne 1024 -or $persistedDevices.budget.readers_per_device -ne $DeviceReaders -or
            [int]$persistedPool.workers -ne $ContentWorkers) { throw 'Pause or resource settings did not survive restart.' }
        foreach ($id in $sourceIds) {
            $persistedSource = Invoke-RestMethod "$baseUrl/api/sources/$id" -TimeoutSec 5
            if ($persistedSource.source.content_concurrency -ne $SourceReaders) { throw 'Source policy did not survive restart.' }
        }
        $retained = Invoke-RestMethod "$baseUrl/api/search?q=content%3Arecoveryneedle&limit=10&count=exact" -TimeoutSec 15
        if ($retained.hits.Count -ne 10 -or -not $retained.total.exact -or [long]$retained.total.value -ne $totalFiles) {
            throw 'Restart lost searchable fixture content.'
        }
        $resumed = Invoke-RestMethod "$baseUrl/api/content/resume" -Method Post -TimeoutSec 10
        if ($resumed.paused) { throw 'Candidate did not resume explicitly.' }
        $restart = @{ performed = $true; qualification = 'forced restart after drain; not mid-file pause or installed upgrade'
            original_pid = $originalPid; restarted_pid = $candidate.Id; pause_response_ms = $pauseMs; restart_health_ms = $restartReadyMs
            pause_and_limits_preserved = $true; retained_search_hits = $retained.hits.Count
            retained_search_total = $retained.total; resumed = $true }
    }
    $sorted = @($latencies | Sort-Object)
    $report = [ordered]@{
        measured_at_utc = [DateTime]::UtcNow.ToString('o')
        qualification = 'synthetic smoke only; process I/O is not physical disk I/O'
        conditions = 'host not isolated; normal native change feed may observe other host-volume activity'
        platform = [Environment]::OSVersion.VersionString; version = $health.version; binary_sha256 = $sha
        fixture_directory = $fixtureDir
        fixture_files = $totalFiles; fixture_bytes = $sourceBytes; source_count = $SourceCount
        small_files_per_source = $Files; large_files_per_source = $LargeFilesPerSource; large_file_mib = $LargeFileMiB
        content_workers = $ContentWorkers; scan_threads = $ScanThreads; concurrent_scans = $ConcurrentScans; source_readers = $SourceReaders
        crawl_timing = 'first scan request to all sources complete; source creation/policy setup excluded'
        crawl = (Delta $before $after $elapsed)
        files_per_s = $totalFiles / $elapsed; source_bytes_per_s = $sourceBytes / $elapsed
        http_query_samples = $sorted.Count
        http_query_latencies_ms = $latencies.ToArray()
        query_interval_ms = $QueryIntervalMilliseconds
        http_query_p95_ms = $sorted[[Math]::Max(0, [Math]::Ceiling($sorted.Count * 0.95) - 1)]
        http_query_p99_ms = $sorted[[Math]::Max(0, [Math]::Ceiling($sorted.Count * 0.99) - 1)]
        crawl_query_tail_sample_sufficient = $sorted.Count -ge 100
        http_query_max_ms = $sorted[-1]
        idle = (Delta $idleBefore $idleAfter $idleElapsed)
        catalog_writer_after_crawl = $activity.catalog_writer
        catalog_writer_before_idle = $idleActivityBefore.catalog_writer
        catalog_writer_after_idle = $idleActivity.catalog_writer
        storage_after_idle = $idleActivity.storage
        memory_after_idle = $memory
        device_reader_limit = $DeviceReaders
        device_status_samples_during_crawl = $deviceSamples
        max_observed_device_reservations = $maxDeviceReaders
        resolved_device_observed = $sawResolvedDevice
        resolved_shared_device_observed = $sawSharedDevice
        devices_after_idle = $devicesAfterIdle
        watchers_before_idle = @($idleSourcesBefore | ForEach-Object { @{ source = $_.source.id; watcher = $_.watcher } })
        watchers_after_idle = @($idleSourcesAfter | ForEach-Object { @{ source = $_.source.id; watcher = $_.watcher } })
        files_indexed = $activity.workers.files_indexed
        search_total = $search.total
        restart_after_idle = $restart
    }
    $json = $report | ConvertTo-Json -Depth 8
    [IO.File]::WriteAllText((Join-Path $fixtureDir 'report.json'), $json, $utf8)
    Write-Output $json
} catch {
    $failed = $_
    # Record the failure first: nothing below may replace the error being described.
    $failure = @{ measured_at_utc = [DateTime]::UtcNow.ToString('o'); status = 'failed'; error = $failed.Exception.Message
        binary_sha256 = $sha; fixture_directory = $fixtureDir
        fixture_files = $totalFiles; fixture_bytes = $sourceBytes; source_count = $SourceCount
        content_workers = $ContentWorkers; scan_threads = $ScanThreads; concurrent_scans = $ConcurrentScans; device_readers = $DeviceReaders }
    [IO.File]::WriteAllText((Join-Path $fixtureDir 'failure.json'), ($failure | ConvertTo-Json -Depth 5), $utf8)
    # A caller that never received a report still learns which fixture to read.
    try { $failed.Exception.Data['recovery_fixture_directory'] = $fixtureDir } catch { }
    throw $failed
} finally {
    # Stop only the process launched above; never match eidos by executable name,
    # and never let cleanup hide the failure that brought us here.
    if ($null -ne $candidate -and -not $candidate.HasExited) {
        try { Stop-Process -InputObject $candidate -Force }
        catch { Write-Warning "Candidate $($candidate.Id) could not be stopped: $($_.Exception.Message)" }
    }
    Write-Host "Temporary fixture and measurement retained: $fixtureDir"
}
