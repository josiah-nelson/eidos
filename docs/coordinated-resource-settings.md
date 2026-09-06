# Coordinated resource settings

Activity can apply the content worker pool, metadata scan width, concurrent
scan ceiling, data-volume reserve and shared-device reader ceiling as one
recoverable custom configuration. These values are still admission controls,
not measured performance presets. Per-source content reader caps remain
independent and are never changed by this operation.

The component owners intentionally retain their existing files:
`content-workers.json`, `resource-limits.json` and `device-limits.json`. A
single rename cannot atomically replace three files. Before changing any of
them, the service therefore syncs `resource-settings-operation.json` with the
complete target and an ordered step list. It checkpoints the journal after
each component replacement and removes it only after every setting is live.
Tighter limits are applied before relaxed limits where the component-file
boundary allows it.

If a component write or journal checkpoint fails, the API and Activity show
the actual live tuple, completed components, the next or failed component and
the durable target. They report a partial outcome rather than a successful
save. Correct the storage problem and choose **Repair coordinated save** to
continue idempotently. A cleanup failure is also partial even when every target
setting is already live. Individual resource, device and worker saves return a
conflict while a journal remains, so they cannot silently replace a target
that restart recovery will replay.

At startup, the service validates and replays a pending journal before loading
the component controls or starting scanners, content workers and topology
work. An unreadable, invalid or unsupported journal fails startup instead of
guessing a configuration. A settings/checkpoint failure during startup also
fails closed with the journal retained. If only removal of a completed journal
fails, the consistent target may start and the cleanup remains visible and
repairable.

Lowering any ceiling affects new admissions. Existing files, scan widths and
device reservations finish and release normally; no lease is revoked. The
ordinary manual controls and their restart behavior remain available whenever
no repair is pending.

## API and CLI

`GET /api/resource-settings` returns the current live tuple and any pending
operation. `POST /api/resource-settings` accepts the complete custom tuple:

```json
{
  "settings": {
    "content_workers": 2,
    "scan_threads": 2,
    "concurrent_scans": 1,
    "minimum_free_mib": 1024,
    "readers_per_device": 2
  }
}
```

The response outcome is `applied`, `partial` or `failed`. A `partial` response
always includes `pending`; a `failed` response means the initial journal could
not be written and no component changed. `POST /api/resource-settings/repair`
replays the pending target. The status endpoint uses `current` when no operation
is pending and `partial` when repair is required. It never infers success from
a saved label.

```powershell
eidos resources coordinated
eidos resources coordinated apply --content-workers 2 --scan-threads 2 `
  --concurrent-scans 1 --minimum-free-mib 1024 --device-readers 2
eidos resources coordinated repair
```

Add `--json` before the `coordinated` subcommand for the complete wire response.
The CLI exits nonzero when the result remains incomplete, which makes partial
application visible to scripts.
