# Bounded synthetic measurement, NOT release/physical-disk qualification.
# Launches only the specified binary with new temporary data on loopback.
# Retains fixture, logs and report; never touches an installed service.
param(
    [string]$Binary = (Join-Path $PSScriptRoot "../target/debug/eidos.exe"),
    [ValidateRange(32, 4096)][int]$Files = 1024,
    [ValidateRange(5, 30)][int]$IdleSeconds = 15,
    [ValidateRange(1, 64)][int]$DeviceReaders = 2
)
$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'This measurement script uses Windows process I/O counters.' }
$candidatePath = (Resolve-Path -LiteralPath $Binary).Path
$fixtureDir = Join-Path ([IO.Path]::GetTempPath()) ("eidos-recovery-" + [guid]::NewGuid().ToString('N'))
$sourceDir = Join-Path $fixtureDir 'source'
$dataDir = Join-Path $fixtureDir 'data'
$logDir = Join-Path $fixtureDir 'logs'
New-Item -ItemType Directory -Path $sourceDir, $dataDir, $logDir | Out-Null
$utf8 = [Text.UTF8Encoding]::new($false)
$payload = ("recoveryneedle synthetic indexing fixture apple maple river cloud`n" * 64)
$sourceBytes = 0L
for ($i = 0; $i -lt $Files; $i++) {
    $text = "document $i`n" + $payload
    [IO.File]::WriteAllText((Join-Path $sourceDir "document-$i.txt"), $text, $utf8)
    $sourceBytes += $utf8.GetByteCount($text)
}
$listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
$listener.Start()
$port = $listener.LocalEndpoint.Port
$listener.Stop()
$baseUrl = "http://127.0.0.1:$port"
$arguments = @('serve', '--bind', "127.0.0.1:$port", '--data-dir', ('"' + $dataDir + '"'),
    '--log-dir', ('"' + $logDir + '"'), '--scan-threads', '2', '--content-workers', '2',
    '--no-auto-reconcile', '--no-fleet', '--no-update-check')
$sha = (Get-FileHash -LiteralPath $candidatePath -Algorithm SHA256).Hash
$candidate = Start-Process -FilePath $candidatePath -ArgumentList $arguments -PassThru -WindowStyle Hidden
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
try {
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
    $saved = & $candidatePath resources --url $baseUrl --scan-threads 2 --concurrent-scans 1 --minimum-free-mib 1024 --json
    if ($LASTEXITCODE -ne 0) { throw 'Resource CLI round trip failed.' }
    if (($saved | ConvertFrom-Json).limits.scan_threads -ne 2) { throw 'Resource limits were not saved.' }
    $deviceSaved = & $candidatePath resources --url $baseUrl --device-readers $DeviceReaders --json
    if ($LASTEXITCODE -ne 0 -or ($deviceSaved | ConvertFrom-Json).budget.readers_per_device -ne $DeviceReaders) {
        throw 'Device-limit CLI round trip failed.'
    }
    # Warm the on-demand sampler; this reads only the candidate's own process.
    $null = Invoke-RestMethod "$baseUrl/api/memory" -TimeoutSec 5
    $before = Counters
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $added = Invoke-RestMethod "$baseUrl/api/sources" -Method Post -ContentType 'application/json' -TimeoutSec 10 -Body (
        @{ name = 'recovery-fixture'; root_path = $sourceDir; scan = $true } | ConvertTo-Json)
    if ($added.scan_error) { throw $added.scan_error }
    $latencies = [Collections.Generic.List[double]]::new()
    $deviceSamples = 0
    $maxDeviceReaders = 0
    $sawResolvedDevice = $false
    do {
        if ($clock.Elapsed.TotalSeconds -gt 180) { throw 'Synthetic content crawl did not drain in 180 seconds.' }
        $queryClock = [Diagnostics.Stopwatch]::StartNew()
        $null = Invoke-RestMethod "$baseUrl/api/search?q=content%3Arecoveryneedle&limit=10" -TimeoutSec 15
        $latencies.Add($queryClock.Elapsed.TotalMilliseconds)
        $devices = Invoke-RestMethod "$baseUrl/api/devices" -TimeoutSec 5
        $deviceSamples++
        $sawResolvedDevice = $sawResolvedDevice -or (-not $devices.budget.unresolved_shared_fallback -and $devices.budget.devices.Count -gt 0)
        foreach ($device in $devices.budget.devices) {
            $readers = [int]$device.content_readers + [int]$device.scan_threads
            $maxDeviceReaders = [Math]::Max($maxDeviceReaders, [Math]::Max($readers, [int]$device.peak_readers))
            if ($readers -gt $DeviceReaders -or [int]$device.peak_readers -gt $DeviceReaders) {
                throw 'Observed device reader reservations exceeded the saved ceiling.'
            }
        }
        $source = Invoke-RestMethod "$baseUrl/api/sources/$($added.source.id)" -TimeoutSec 10
        if ($source.source.state -ne 'complete') { Start-Sleep -Milliseconds 250 }
    } while ($source.source.state -ne 'complete')
    $elapsed = $clock.Elapsed.TotalSeconds
    $after = Counters
    $activity = Invoke-RestMethod "$baseUrl/api/activity" -TimeoutSec 10
    if ([long]$activity.jobs.queued -ne 0 -or [long]$activity.jobs.running -ne 0 -or [long]$activity.workers.pending_publish -ne 0) {
        throw 'Source reported complete before the pipeline drained.'
    }
    $search = Invoke-RestMethod "$baseUrl/api/search?q=content%3Arecoveryneedle&limit=10" -TimeoutSec 15
    if ($search.hits.Count -ne 10 -or [long]$activity.workers.files_indexed -ne $Files) {
        throw 'The completed synthetic fixture is not searchable as expected.'
    }
    # Let trailing commits/merges settle; no API polling during idle sample.
    Start-Sleep -Seconds 5
    $idleBefore = Counters
    $idleClock = [Diagnostics.Stopwatch]::StartNew()
    Start-Sleep -Seconds $IdleSeconds
    $idleAfter = Counters
    $idleElapsed = $idleClock.Elapsed.TotalSeconds
    $idleActivity = Invoke-RestMethod "$baseUrl/api/activity" -TimeoutSec 10
    # Sample after the unpolled idle window, never during it. Exercise the
    # actual CLI and exact-string API counters, with bounded cold-cache retry.
    $memory = $null
    for ($attempt = 0; $attempt -lt 20; $attempt++) {
        $memoryJson = & $candidatePath resources --url $baseUrl --memory --json
        if ($LASTEXITCODE -ne 0) { throw 'Memory CLI round trip failed.' }
        $memory = $memoryJson | ConvertFrom-Json
        if ($memory.process -and -not $memory.stale -and [long]$memory.sample_age_s -le 1) { break }
        Start-Sleep -Milliseconds 100
    }
    if (-not $memory.process -or $memory.stale -or [long]$memory.sample_age_s -gt 1 -or $memory.process.pid -ne $candidate.Id -or [long]$memory.process.resident_bytes -le 0) {
        throw 'Memory API did not report a fresh sample of the temporary candidate.'
    }
    $deviceJson = & $candidatePath resources --url $baseUrl --devices --json
    if ($LASTEXITCODE -ne 0) { throw 'Device diagnostics CLI failed.' }
    $devicesAfterIdle = $deviceJson | ConvertFrom-Json
    $sorted = @($latencies | Sort-Object)
    $report = [ordered]@{
        measured_at_utc = [DateTime]::UtcNow.ToString('o')
        qualification = 'synthetic smoke only; process I/O is not physical disk I/O'
        conditions = 'host not isolated; normal native change feed may observe other host-volume activity'
        platform = [Environment]::OSVersion.VersionString; version = $health.version; binary_sha256 = $sha
        fixture_files = $Files; fixture_bytes = $sourceBytes; content_workers = 2; scan_threads = 2
        crawl = (Delta $before $after $elapsed)
        files_per_s = $Files / $elapsed; source_bytes_per_s = $sourceBytes / $elapsed
        http_query_samples = $sorted.Count
        http_query_p95_ms = $sorted[[Math]::Max(0, [Math]::Ceiling($sorted.Count * 0.95) - 1)]
        http_query_max_ms = $sorted[-1]
        idle = (Delta $idleBefore $idleAfter $idleElapsed)
        catalog_writer_after_crawl = $activity.catalog_writer
        catalog_writer_after_idle = $idleActivity.catalog_writer
        storage_after_idle = $idleActivity.storage
        memory_after_idle = $memory
        device_reader_limit = $DeviceReaders
        device_status_samples_during_crawl = $deviceSamples
        max_observed_device_reservations = $maxDeviceReaders
        resolved_device_observed = $sawResolvedDevice
        devices_after_idle = $devicesAfterIdle
        files_indexed = $activity.workers.files_indexed
    }
    $json = $report | ConvertTo-Json -Depth 8
    [IO.File]::WriteAllText((Join-Path $fixtureDir 'report.json'), $json, $utf8)
    Write-Output $json
} finally {
    # Stop only the process launched above; never match eidos by executable name.
    if (-not $candidate.HasExited) { Stop-Process -InputObject $candidate -Force }
    Write-Host "Temporary fixture and measurement retained: $fixtureDir"
}
