# ADR-0036: bound content-index input by bytes

Status: accepted for recovery implementation.

The larger synthetic workload reached resident peaks of 593 and 600 MiB with
four extraction workers, failing the fixed 512-MiB acceptance gate. Tantivy's
256-MiB content writer budget covers segment construction. Its separate
10,000-document input channel can hold three copies of each text chunk and
is not included in that writer budget.

Reserve a shared 16-MiB allowance before allocating queued document text.
Account for all three text fields plus fixed document/channel overhead and
preallocate their required capacity. A document carries its reservation into
Tantivy and releases it on consumption or discard, including failure paths.
Extraction and rebuild both use this path. A five-second admission deadline
reports a stalled consumer without permanently blocking pause or shutdown.

Keep the schema and segment writer budget unchanged. Show the separate input
budget in memory diagnostics. This is an input allocation bound, not a whole
process memory cap; tokenization, index readers, segment flush/merge and other
allocations remain separate. Retain the failed workload results and repeat
the fixed executable comparison before claiming that memory acceptance passes.
