import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { api, type ExclusionPolicy, type ExclusionRule } from '../api'
import { ErrorBox, Spinner } from '../components'
import { count, humanState } from '../format'

const same = (a: ExclusionRule[], b: ExclusionRule[]) => JSON.stringify(a) === JSON.stringify(b)

function Editor({ sourceId, saved }: { sourceId: string; saved: ExclusionPolicy }) {
  const qc = useQueryClient()
  const [rules, setRules] = useState(saved.rules)
  const [base, setBase] = useState(saved)
  const [conflict, setConflict] = useState(false)
  const [folder, setFolder] = useState('')
  const [paths, setPaths] = useState('')
  if (base.revision !== saved.revision) {
    if (same(rules, base.rules) || same(rules, saved.rules)) {
      setRules(saved.rules)
      setConflict(false)
    } else setConflict(true)
    setBase(saved)
  }
  const changed = !same(rules, saved.rules)
  const refresh = (value: ExclusionPolicy) => {
    qc.setQueryData(['exclusion-policy', sourceId], value)
    qc.invalidateQueries({ queryKey: ['source', sourceId] })
    qc.invalidateQueries({ queryKey: ['activity'] })
  }
  const apply = useMutation({
    mutationFn: () => api.applyExclusions(sourceId, { expected_revision: base.revision, rules }),
    onSuccess: refresh,
  })
  const preview = useMutation({
    mutationFn: () => api.previewExclusions(sourceId, { rules, paths: paths.split('\n').map(p => p.trim()).filter(Boolean) }),
  })
  const retry = useMutation({ mutationFn: () => api.retryExclusions(sourceId), onSuccess: refresh })
  const busy = saved.phase !== 'applied'
  const disabled = apply.isPending || preview.isPending || busy
  const edit = (value: ExclusionRule[]) => { setRules(value); apply.reset(); preview.reset() }
  // IDs are labels, not credentials. This also works on plain-HTTP LAN URLs
  // where browsers do not expose crypto.randomUUID (a secure-context API).
  const add = (kind: ExclusionRule['kind'], pattern = '') => edit([...rules, { id: `rule-${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`, kind, pattern, include: false }])
  const move = (index: number, step: number) => {
    const copy = [...rules]
    ;[copy[index], copy[index + step]] = [copy[index + step], copy[index]]
    edit(copy)
  }
  return <>
    <p>Keep file names and sizes, and choose which file contents to index. Rules apply to source-relative paths
      using <code>/</code> separators. Last matching rule wins; include rules can override built-in content
      exclusions, but cannot override self-store, symlink, placeholder, offline or swap-file protection.
      Matching is {saved.case_sensitive ? 'case-sensitive' : 'case-insensitive'} on this source.</p>
    <p className="muted">An include makes a file a candidate; it does not add an extractor or enable a disabled content pipeline.</p>
    <p role="status">Policy revision {saved.revision} · {humanState(saved.phase)} · {count(saved.processed)} objects checked · {count(saved.changed)} changed</p>
    {busy && <p className="banner warn">New scans and content claims for this source are waiting. Current files drain first;
      catalog updates and old content cleanup then run in batches. Application resumes after restart. Check errors below if progress stops.</p>}
    {saved.error && <div className="banner bad" role="alert">Application stopped: {saved.error}{' '}
      <button disabled={retry.isPending} onClick={() => retry.mutate()}>Retry application</button></div>}
    {retry.isError && <ErrorBox error={retry.error} />}
    {saved.protected_directories.length > 0 && <div className="banner warn">
      Eidos storage is automatically protected from enumeration and content reads:
      <ul>{saved.protected_directories.map(p => <li key={p}><code>{p || '(entire source root)'}</code></li>)}</ul>
      Boundary directories remain visible. Their contents and totals are unknown or last-known, not an empty folder.
    </div>}
    {conflict && <p className="banner warn" role="alert">Policy changed elsewhere. Your draft is retained; load the saved policy before applying.</p>}
    <fieldset disabled={disabled} style={{ border: 0, padding: 0, minWidth: 0 }}>
      <form className="toolbar" onSubmit={e => { e.preventDefault(); if (folder.trim()) { add('directory', folder.trim()); setFolder('') } }}>
        <label>Exclude a folder <input value={folder} placeholder="build/cache" onChange={e => setFolder(e.target.value)} /></label>
        <button disabled={rules.length >= 100 || !folder.trim()}>Add folder exclusion</button>
      </form>
      <details open={rules.length > 0}>
        <summary>Advanced rules ({rules.length}/100)</summary>
        {rules.map((r, i) => <div key={r.id} className="toolbar" style={{ flexWrap: 'wrap', gap: 8, marginTop: 8 }}>
          <span>{i + 1}.</span>
          <label>Action {i + 1} <select value={r.include ? 'include' : 'exclude'} onChange={e => edit(rules.map((v, n) => n === i ? { ...v, include: e.target.value === 'include' } : v))}>
            <option value="exclude">Exclude content</option><option value="include">Include content</option>
          </select></label>
          <label>Match {i + 1} <select value={r.kind} onChange={e => edit(rules.map((v, n) => n === i ? { ...v, kind: e.target.value as ExclusionRule['kind'] } : v))}>
            <option value="directory">Folder and descendants</option><option value="regex">Regular expression</option>
          </select></label>
          <label>Pattern {i + 1} <input className="mono" value={r.pattern} maxLength={4096} onChange={e => edit(rules.map((v, n) => n === i ? { ...v, pattern: e.target.value } : v))} /></label>
          <button aria-label={`Move rule ${i + 1} earlier`} disabled={i === 0} onClick={() => move(i, -1)}>↑</button>
          <button aria-label={`Move rule ${i + 1} later`} disabled={i === rules.length - 1} onClick={() => move(i, 1)}>↓</button>
          <button aria-label={`Remove rule ${i + 1}`} onClick={() => edit(rules.filter((_, n) => n !== i))}>Remove</button>
        </div>)}
        <p><button disabled={rules.length >= 100} onClick={() => add('regex')}>Add regex rule</button></p>
        <p className="muted small">Regex matches anywhere in the relative file path. Use anchors for an exact match, e.g. <code>^build/.*\.log$</code>.
          Patterns are validated by the service; look-around and backreferences are not supported.</p>
      </details>
      <label>Preview file paths (one per line, up to 50)<br />
        <textarea rows={3} value={paths} style={{ width: '100%' }} onChange={e => { setPaths(e.target.value); preview.reset() }} /></label>
      <p><button onClick={() => preview.mutate()}>{preview.isPending ? 'Checking…' : 'Validate and preview'}</button></p>
      <div className="toolbar">
        <button disabled={!changed || conflict || rules.some(r => !r.pattern)} onClick={() => apply.mutate()}>Apply to existing and future files</button>
        <button onClick={() => { edit(saved.rules); setBase(saved); setConflict(false) }}>Load saved policy</button>
      </div>
    </fieldset>
    <p className="muted small">Edits are a draft until Apply. Application reads catalog metadata, not source files.
      Excluded content is removed; included files queue for extraction. Search coverage changes progressively until application completes.</p>
    {apply.isError && <ErrorBox error={apply.error} />}
    {preview.isError && <ErrorBox error={preview.error} />}
    {preview.isSuccess && <div role="status"><p>Rules validated. Preview does not save changes or open files.</p>
      <ul>{preview.data.map((p, i) => <li key={i}><code>{p.path}</code>: {humanState(p.state)} — {humanState(p.reason)} (<code>{p.rule}</code>)
        {!p.catalogued && ' — not catalogued; assumes a regular, online file'}</li>)}</ul></div>}
  </>
}

export default function ExclusionEditor({ sourceId, remote }: { sourceId: string; remote: boolean }) {
  const q = useQuery({ queryKey: ['exclusion-policy', sourceId], queryFn: () => api.exclusionPolicy(sourceId), refetchInterval: 2000, enabled: !remote })
  return <section aria-labelledby="exclusion-editor-heading">
    <h2 id="exclusion-editor-heading">Content rules</h2>
    {remote ? <p>Edit exclusions on this source's origin node.</p> : q.isPending ? <Spinner label="Loading content rules…" /> : q.isError ? <ErrorBox error={q.error} /> : <Editor key={sourceId} sourceId={sourceId} saved={q.data} />}
  </section>
}
