# Eidos

> **Search your files the way you remember them.**

**With Eidos, the machine a file lives on no longer determines where you have to search.**

Eidos is a private, self-hosted search system for the files and folders scattered across your computers, servers, virtual machines, archives, and network storage. It makes everything searchable from one place without requiring you to first remember where something lives.

One search across every system you control, with the context and certainty ordinary file search leaves behind.

> [!IMPORTANT]
> **This README is a product vision draft and describes Eidos in its intended future state.**
>
> Eidos is under active development, and several capabilities described below are planned rather than available today. For an exact breakdown of the current implementation, see [What works today](#what-works-today) and the [release roadmap](docs/roadmap.md).
>
> The first packaged Windows release is [v0.5.0](https://github.com/josiah-nelson/eidos/releases/latest); see [installing.md](docs/installing.md). APIs, schemas, and query syntax may still change.

## The file is somewhere. That should be enough.

Somewhere across your systems is the thing you need.

It might be on a sleeping laptop, inside a VM you have not opened in months, buried in an archive, or duplicated under several slightly different names. You remember what it was about, what was near it, or how you used it, but not exactly what it was called or where it lived.

Most file-search tools make you answer the location question first:

1. Which machine was it on?
2. Is that machine online?
3. Was it on the host or inside a VM?
4. Was it loose, archived, or stored on a share?
5. Which search tool works there?

Eidos does not ask you to remember where a file lives before you can search for it.

## Most search tools stop at the machine. Eidos doesn’t.

Existing tools each understand one part of the problem:

- Desktop search is fast, but usually ends at the edge of one computer.
- Cloud drives search only what you place inside their folders and infrastructure.
- Content and vector search can find related text, but often lose the file’s identity, location, freshness, and surrounding context.
- Storage analyzers know where space went, but not what the files mean or how they relate.
- Backup software reports what its jobs intended to preserve, not necessarily what still exists on disk.

Eidos connects those partial views.

It maintains a living, durable understanding of what exists across your systems: where files have lived, whether the information is current, which copies are identical or related, what surrounds them, and how completely a question can be answered.

It does this without moving your files into a new repository or requiring a vendor cloud.

## Built for the searches people actually have

Eidos is being built for situations like these:

- Find the project notes you edited last winter, even though you cannot remember their name or machine.
- Locate a configuration file that mentions the media server, wherever it ended up.
- Find an old spreadsheet containing a renewal date, including copies inside supported archives.
- Identify the folder containing both a wiring diagram and the corresponding controller configuration.
- Determine which copy of a directory is newest and where the other copies exist.
- Check whether important files really exist on multiple physical systems.
- Search a powered-off virtual machine without booting it first.

Some of these scenarios are available in the current Windows-first foundation; others define where Eidos is going. All of them depend on the same underlying idea: file search should begin with what you remember, not where you think you stored it.

## What makes Eidos different

### Search across systems as though everything were local

Eidos is designed to search workstations, laptops, servers, VMs, and network shares through one interface, one query model, and one ranked result set.

There is no remote-desktop scavenger hunt, no sequence of mapped drives, and no separate search window for every machine. The physical location of a file remains visible and important, but it no longer dictates where the search must begin.

### Offline does not mean forgotten

Laptops sleep. Servers restart. VMs may remain powered off for months.

Eidos preserves the last known state of an unavailable source instead of silently emptying it. Results can show where a file was last observed and how fresh that knowledge is.

A file does not stop existing because its machine is temporarily absent.

### Files are not their paths

Most indexing systems treat a path as the file’s identity. That breaks down as soon as a file is renamed, moved, hard-linked, copied, or observed from another source.

Eidos maintains a canonical catalog of filesystem objects and models paths separately. Search indexes, hashes, snippets, aggregates, and relationships are derived views that can be rebuilt without losing the underlying truth.

A rename remains a rename. It does not become a fictional deletion followed by the discovery of unrelated new content.

### Directories are part of the answer

Folders are not merely containers in Eidos. They are searchable results with properties of their own.

Eidos can search using the contents and shape of an entire subtree:

- directories containing particular combinations of file types;
- trees above a given apparent or allocated size;
- folders containing more than a specified number of files;
- directories whose newest descendant changed within a time range;
- later, exact and near copies of directory trees.

This makes it possible to find a project from the fragments around it even when no single filename gives the answer away.

### Search inside the things other tools treat as files

An archive or virtual disk may be one file to the host filesystem, but it contains another filesystem from the user’s perspective.

Eidos is being designed to represent those contents as virtual directory trees. Supported archives can be searched without being unpacked into the source tree, and planned virtual-disk support will make files inside VHDX images discoverable from the host without installing an agent in the guest or starting the VM.

Resource limits follow the entire nested operation, so corrupt archives, extreme compression ratios, and unexpectedly large contents cannot silently consume unbounded memory or storage.

### Answers arrive quickly and carry their uncertainty

The durable index can return useful results immediately. In the planned fleet architecture, available machines can then confirm, update, or retract results as fresher information arrives.

Instead of waiting behind a spinner for the slowest source, you see the answer converge.

Freshness is part of the result, not hidden implementation detail.

### “It isn’t there” can be a real answer

A zero-result page is meaningful only when you know what was actually searched.

Every Eidos response reports its coverage: which sources are complete, which are pending, stale, degraded, or offline, and what could not be searched. The CLI returns a nonzero status for incomplete operations so scripts cannot silently mistake a partial answer for a definitive one.

Exact and case-sensitive matches are verified against original text. Broad searches are identified rather than quietly truncated. Every hit retains its provenance.

Eidos never turns “I could not check” into “it does not exist.”

### Search by what is inside the file

Eidos indexes literal text with line-aware snippets and supports:

- ranked terms and phrases;
- case-sensitive exact tokens;
- case-sensitive substrings;
- regular expressions;
- filename, path, type, size, and time filters;
- Boolean combinations of content and metadata;
- surrounding lines without opening the original file.

The content pipeline streams large files with bounded memory, recognizes common Windows text encodings, rejects binary data, and records whether each file was indexed completely, partially, excluded, or failed.

### Copies become information instead of clutter

The same content on several machines should not appear as several unrelated facts.

The Eidos roadmap builds on content identity and directory fingerprints to group exact copies, compare trees, expose differences, and eventually track lineage between related versions. Search can favor the copy available now while still showing where the others were last observed.

That same knowledge can reduce unnecessary transfers. Bytes already present elsewhere in the fleet do not need to be sent again.

### Backup verification from observed reality

Eidos does not aim to replace backup software. It can provide an independent view of what the backup system actually produced.

The planned coverage model will let you describe an expectation such as:

> This tree should exist in three copies, on two physical systems, with one copy off-site.

Eidos can compare that expectation with files it has actually observed. Single-copy data, stale replicas, missing destinations, and broken synchronization become visible without trusting the backup tool’s own success report as the only source of truth.

## Adaptive, not invasive

Your systems are not one uniform pool of storage. They may include fast NVMe, slower disks, intermittent laptops, sleeping VMs, network shares, active working trees, backup destinations, and enormous directories full of generated files.

Eidos is designed to adapt to those differences.

Frequently changing and actively used areas can remain hot. Expensive background work can be scheduled around the storage beneath it. Repetitive noise such as build caches and dependency trees can be deprioritized without becoming hidden or unsearchable. Offline systems remain remembered without being presented as current.

Signals used to improve ranking and scheduling are intended to be sampled and aggregated locally, not retained as a detailed history of everything you opened or searched.

The index, file contents, behavioral signals, and optional enrichment models remain inside infrastructure you control. There is no required vendor cloud.

## Context without guesswork

Eidos understands more than filenames.

It knows when a file moved without becoming a different file. It knows when several paths contain the same content, when one observed copy is older, and when the information from a machine may be stale. It can use directory structure, neighboring files, extracted entities, references, and document relationships as part of an answer.

That intelligence begins with evidence from the filesystem itself:

- stable identity;
- directory topology;
- original text;
- content fingerprints;
- timestamps and history;
- source availability;
- relationships between files and documents.

Optional semantic retrieval can eventually add another signal when the exact words are no longer known. It does not replace exact search, provenance, or the canonical catalog.

## What works today

The v0.5 release is Windows-first (a signed installer; macOS builds from source) and focused on proving the foundation.

| Area | Current capability |
|---|---|
| Catalog | Stable hosts, sources, filesystem objects, entries, paths, scan generations, and processing states |
| Local scanning | Native NTFS enumeration with stable file identity |
| Live updates | USN journal monitoring, restart catch-up, and reconciliation after invalid checkpoints |
| Network storage | Read-only generic SMB crawling with explicit freshness semantics |
| Search | Names, paths, metadata, exact text, phrases, substrings, glob, regex, filters, and Boolean queries |
| Content | Streaming extraction, encoding detection, line-aware chunks and snippets, BLAKE3 hashing, bounded memory |
| Directories | First-class directory results, descendant predicates, subtree sizes, counts, tree browsing, and treemap data |
| Reliability | Atomic scan publication, durable jobs, offline preservation, and per-source completeness |
| Interfaces | Web UI, CLI, and HTTP API using the same typed query model |
| Archives | ZIP member inventories searchable by name, path, and size |
| Packaging | Signed Windows setup (per-user or service), in-place upgrades; macOS LaunchAgent from source |
| Fleet (experimental) | Explicitly enrolled nodes replicate catalog metadata to one central over mutual TLS; central search over the union |

Everything Eidos does to a source is read-only.

## Where it is going

The detailed sequence and acceptance gates live in the [release roadmap](docs/roadmap.md), but the larger direction includes:

- Windows and macOS agents feeding a durable central search service;
- one-command enrollment and resumable catch-up after long periods offline;
- direct indexing of supported virtual-disk filesystems;
- recursive ZIP, RAR, 7z, and tar discovery;
- text extraction from common Office documents and PDFs;
- compact source summaries that avoid waking or querying irrelevant machines;
- progressive results that update as live sources respond;
- exact copy grouping, directory comparison, and text diffs;
- file lineage and near-copy similarity;
- independent backup-coverage expectations;
- document, entity, backlink, and code relationships;
- optional semantic retrieval with visible provenance;
- Linux and NAS-oriented agents;
- point-in-time search across prior observed states;
- predicted offline working sets for laptops;
- an index capable of outliving any individual machine that contributed to it.

These are not unrelated products being attached to a search box. They grow from the same foundation: a correct, durable understanding of what your files are, where they have existed, how they relate, and how confidently Eidos can answer a question about them.

## Measured on real storage

Eidos is developed against real, imperfect storage rather than only tiny synthetic fixtures.

The current reference corpus includes local SSD and HDD volumes plus SMB shares, totaling more than four million entries. Recent measured results on the reference system include:

| Measurement | Result |
|---|---:|
| Full catalog | 4.15 million entries |
| Warm local metadata scans | 3.5–4.5 seconds |
| Incremental change visibility | approximately 0.5 seconds |
| Literal text processed | 28.84 GiB in approximately 17 minutes |
| Ordinary ranked and phrase content queries | 19 ms p95 |
| Case-sensitive exact-token content queries | 19 ms p95 |
| Name-query family | 32 ms p95 |
| Selective name/path regex family | 39 ms p95 |
| Largest streaming fixture | 2.14 GiB with 8.6 MiB peak working set |

These numbers are reference measurements, not universal guarantees. Hardware, storage latency, corpus shape, query selectivity, and cache state all matter. See [docs/benchmarks.md](docs/benchmarks.md) for the complete methodology, corpus description, limitations, and results.

## Quick start

Download `eidos-<version>-setup.exe` from the
[releases page](https://github.com/josiah-nelson/eidos/releases) and run
it. The setup installs the core either for you alone (no administrator rights;
it runs while you are signed in) or for the whole computer as a Windows
service. Selecting the optional system collector adds a per-machine service
and requires administrator approval. Setup asks where the program and its data
should live and which port to use, and opens the web interface when it finishes.
[docs/installing.md](docs/installing.md) covers every option, unattended
installs, upgrades and removal.

macOS has no installer package yet: build `Eidos.app` from a clone and
install the agent from it, which is what makes Full Disk Access grantable.
See [docs/installing-macos.md](docs/installing-macos.md).

Building from source requires Rust stable with the MSVC target, Visual
Studio Build Tools with the Windows SDK, and Node.js 24 or newer; see
[docs/development.md](docs/development.md).

```powershell
cd web; npm ci; npm run build; cd ..
cargo build --release              # the web UI is embedded in eidos.exe

# Register a source and start the API and web UI.
.\target\release\eidos.exe source add projects D:\Projects
.\target\release\eidos.exe serve --data-dir data
```

The web UI will be available at `http://127.0.0.1:7700`.

From another shell:

```powershell
# Search names and metadata.
.\target\release\eidos.exe search "readme ext:md mtime:>=30d"

# Search indexed file contents.
.\target\release\eidos.exe search 'content:"connection refused" ext:log'

# Find directories containing both Markdown and JSON descendants.
.\target\release\eidos.exe search --mode directories "has:md has:json" --explain

# Produce structured output for automation.
.\target\release\eidos.exe search --json "ext:zip size:>1G"
```

The service keeps supported NTFS sources current through the USN journal and periodically reconciles sources without a reliable change feed.

The CLI exits with status `2` when any requested source was incomplete.

## How it works

Eidos separates durable truth from rebuildable search infrastructure:

1. **Scanners observe filesystems read-only.**
2. **The canonical catalog records objects, directory entries, sources, generations, states, and aggregates.**
3. **Bounded background workers extract text, compute hashes, and build other derived information.**
4. **Catalog and content indexes make those projections searchable.**
5. **One query model powers the web UI, CLI, HTTP API, and future integrations.**
6. **Every response joins results with current paths, provenance, and source completeness.**

Search indexes can be rebuilt. File identity, observed state, and completeness cannot be guessed afterward, so they belong in the catalog.

See [docs/architecture.md](docs/architecture.md) for the invariants, data model, scheduler, query execution, and evolution from the standalone service to a fleet.

## Why “Eidos”?

*Eidos* (εἶδος) means “form”: the thing itself, distinct from where it happens to sit.

That distinction is foundational to the project. A file is not merely its current path, and moving it does not change what it is.

The name also deliberately evokes eidetic memory: not just recalling that something exists, but retaining the details and context needed to find it again.

## Documentation

| Document | Contents |
|---|---|
| [Overview](docs/overview.md) | Product thesis, representative scenarios, requirements, and non-goals |
| [Architecture](docs/architecture.md) | Invariants, identity model, catalog, search architecture, scheduler, and fleet evolution |
| [Roadmap](docs/roadmap.md) | Releases, milestones, acceptance gates, and sequencing |
| [v0.5 dogfood-fleet sprint](docs/v0.5-dogfood-fleet-sprint.md) | Unified installer, measured multi-node slice, chunking bakeoff, and release gates |
| [Private fleet](docs/fleet.md) | Enrolling nodes with a central, what replicates, status, bounds, and the failure matrix (experimental) |
| [Query syntax](docs/query-syntax.md) | Query language used by the UI, CLI, and API |
| [Development](docs/development.md) | Toolchain, project layout, tests, linting, and benchmarks |
| [Benchmarks](docs/benchmarks.md) | Reference corpus, methodology, measurements, and known limitations |
| [Releasing](docs/releasing.md) | Signed Windows release workflow |
| [Architecture decisions](docs/adr/) | Records of consequential technical decisions |

## Project layout

```text
crates/
  eidos-domain    IDs, states, query AST, result and completeness contracts
  eidos-scanner   enumeration, Windows fast paths, generic walking, USN journal
  eidos-catalog   canonical SQLite catalog, generations, changes, aggregates, jobs
  eidos-content   bounded sniffing, decoding, line-aware chunking, BLAKE3
  eidos-archive   bounded archive manifests and virtual-member inventories
  eidos-sync      deterministic fleet sync core, fencing, compaction, anti-entropy
  eidos-search    catalog and content indexes, planning, verification, execution
  eidos-query     query syntax parser and renderer
  eidos-service   HTTP API, watchers, reconciliation, and composition
  eidos-cli       the `eidos` executable
web/              Vite, React, and TypeScript web interface
docs/             public documentation and architecture decisions
```

## Contributing

Eidos is developed in the open, but the architecture and public interfaces are still evolving quickly. Expect churn while the v0.5.x line settles.

Bug reports with reproducible synthetic fixtures are especially useful.

Before opening a pull request, run:

```powershell
.\scripts\check.ps1
```

This checks Rust formatting, runs Clippy with warnings denied, executes the test suite, and builds the web application.

## License

Eidos is free software licensed under the [GNU Affero General Public License](LICENSE), version 3 or, at your option, any later version.

If you run a modified version as a network service, the AGPL requires you to offer its source code to the users of that service.

Unless explicitly stated otherwise, contributions intentionally submitted for inclusion in Eidos are licensed under the same terms without additional conditions.
