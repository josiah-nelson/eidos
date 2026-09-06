# Content rules and protected storage

Open **Sources → a source → Content rules**. Add a folder exclusion, or use
the advanced editor for ordered folder/regular-expression include and exclude
rules. Names, sizes and physical file metadata remain searchable when their
content is excluded. Replicas are edited on their origin node.

Rules are drafts until **Apply to existing and future files**. Polling keeps
an edited draft; a different saved revision requires loading the current
policy before applying. Validation/preview does not save or open source files.

## Matching

- Paths are relative to the source root, using `/` separators. A folder rule
  matches that path and descendants, not similarly named siblings.
- Regex is tested against the whole relative file path, with an unanchored
  search by default. For example, `^build/.*\.log$` matches logs under `build`.
  Look-around and backreferences are not supported. Use at most 100 rules,
  4,096 bytes per pattern; regex compiled size is bounded to 256 KiB per rule.
- Matching uses the source volume's case sensitivity. Unknown volumes use
  case-insensitive matching. The editor shows which applies.
- The **last matching operator rule wins**. An include can override built-in
  cache/extension exclusions, but makes the file only a content candidate:
  sniffing, extractor support and configured extraction limits still apply.
- Self-store, symlink, placeholder, offline/recall and root swap-file safety
  cannot be overridden. An include does not enable a disabled content pipeline.
- A preview of an uncatalogued path assumes a regular, online file. It cannot
  infer attributes without reading the source. Recorded decisions identify the
  reason, rule ID, policy revision and engine version.

Operator rules in this release control **content**, not physical inventory or
enrichment. There is no new global `bin` exclusion.

## Apply and recovery

Apply saves a revision and closes new content claims and scans for that source.
An open scan must finish or be cancelled before Apply; already claimed content
files drain. Other sources can continue working.

The coordinator walks catalog object IDs in indexed pages of at most 128,
without opening or re-enumerating source files. It updates decisions and
aggregate content counts, invalidates superseded content generations, clears
stored chunks/manifests and queues durable content-index deletes. Retired
virtual archive entries are removed from metadata projections through the
ordinary outbox. Newly included files become pending for extraction.

Progress is **objects checked / changed**, not a pre-counted percentage.
Search coverage changes progressively; the source reports a policy transition
until content cleanup has committed and been acknowledged. Metadata projection
lag is reported separately by the normal search coverage contract.

The apply cursor commits with each catalog batch. Content-index deletions are
acknowledged only after commit; an acknowledgement failure retains the durable
queue for replay. A restart resumes unfinished work. A recorded error stops
that source's application and is shown with **Retry application**. Resolve the
store error first; a retry does not replace the rules or increment the revision.
Application also runs when content extraction is paused or globally disabled.

Path changes that affect inherited decisions schedule the same catalog-only
pass. A directory move may therefore temporarily hold content claims for that
source. These are bounded object pages, not a hard byte/time limit on deleting
one very large object's cached chunks or archive manifest.

## Eidos's own directories

The configured data directory (including both indexes) and a separately
configured `--log-dir` are automatically protected. Configured lexical and
resolved canonical paths are retained. Enumeration observes the directory
boundary without listing its children; native translation/application and
content processing also enforce protection. Existing cached content in newly
protected directories is removed by the resumable apply operation.

Boundaries and any last-known metadata remain visible, with incomplete totals
and an explicit coverage explanation. They are **not empty directories**.
When a former boundary is removed from configuration, a successful rescan is
needed to recover missing inventory and clear its coverage gap. Offline CLI
source registration/scans also protect their data directory while retaining
previously configured log roots; run the service to finish a newly scheduled
policy cleanup before an offline scan can proceed.

This is path-based protection, not filesystem isolation: do not register an
alternate mount/share alias or hard link to internal storage as a separate
source. Arbitrary aliases and concurrent filesystem redirection are not proven
equivalent by this policy. Put Eidos storage outside indexed roots where
practical. This change is not a physical-disk or installed-release qualification.

## API and CLI

All routes use the existing `/api` prefix and API v2 integer convention.

| Route | Purpose |
|---|---|
| `GET /sources/{id}/policy` | Rules, revision, protection, progress, error |
| `POST /sources/{id}/policy/preview` | Validate `{rules, paths}`; at most 50 paths |
| `POST /sources/{id}/policy` | Apply `{expected_revision, rules}` |
| `POST /sources/{id}/policy/retry` | Retry the saved operation |
| `GET /sources/{id}/exclusions` | Existing recorded-decision summary |

Each rule has a stable unique `id` (1–64 ASCII letters/digits/`-`/`_`), `kind`
(`directory` or `regex`), `pattern`, and boolean `include`. A rules file is a
JSON array:

```json
[
  {"id":"cache","kind":"directory","pattern":"build/cache","include":false},
  {"id":"keep","kind":"regex","pattern":"^build/cache/keep\\.txt$","include":true}
]
```

```powershell
eidos exclusions 1 status
eidos exclusions 1 preview rules.json build/cache/keep.txt
eidos exclusions 1 apply rules.json --revision 0
eidos exclusions 1 status
eidos exclusions 1 retry
```

Use `--url` or `EIDOS_URL` for another running service. CLI output is JSON;
the revision must be the value returned by Status, not a guessed overwrite.
