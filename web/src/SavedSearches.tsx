import { useRef, useState } from 'react'
import {
  SAVED_SEARCHES_STORAGE_KEY,
  canonicalSearchUrl,
  deleteSavedSearch,
  duplicateSavedSearch,
  importSavedSearches,
  loadSavedSearches,
  nameCollision,
  renameSavedSearch,
  serializeSavedSearches,
  upsertSavedSearch,
  type SavedSearch,
  type SearchViewState,
} from './saved-searches'

interface Props {
  state: SearchViewState
  onRun: (state: SearchViewState) => void
}

interface NameDialog {
  kind: 'save' | 'rename'
  id?: string
  name: string
  error?: string
}

export function SavedSearchControls({ state, onRun }: Props) {
  const [initial] = useState(loadLocal)
  const [searches, setSearches] = useState(initial.searches)
  const [dialog, setDialog] = useState<NameDialog | null>(null)
  const [notice, setNotice] = useState(initial.discarded ? 'Ignored invalid saved-search data.' : '')
  const importInput = useRef<HTMLInputElement>(null)

  const commit = (next: SavedSearch[], message: string) => {
    setSearches(next)
    try {
      localStorage.setItem(SAVED_SEARCHES_STORAGE_KEY, serializeSavedSearches(next))
      setNotice(message)
    } catch {
      setNotice('The browser refused local storage; this change will not survive a restart.')
    }
  }

  const submitName = () => {
    if (!dialog) return
    try {
      if (dialog.kind === 'save') {
        const result = upsertSavedSearch(searches, dialog.name, state)
        commit(result.searches, result.replaced ? `Replaced “${result.saved.name}”.` : `Saved “${result.saved.name}”.`)
      } else {
        const next = renameSavedSearch(searches, dialog.id!, dialog.name)
        commit(next, `Renamed to “${dialog.name.trim()}”.`)
      }
      setDialog(null)
    } catch (error) {
      setDialog({ ...dialog, error: (error as Error).message })
    }
  }

  const duplicate = (id: string) => {
    try {
      const result = duplicateSavedSearch(searches, id)
      commit(result.searches, `Created “${result.saved.name}”.`)
    } catch (error) {
      setNotice((error as Error).message)
    }
  }

  const remove = (saved: SavedSearch) => {
    if (!window.confirm(`Delete the saved search “${saved.name}”?`)) return
    commit(deleteSavedSearch(searches, saved.id), `Deleted “${saved.name}”.`)
  }

  const copyLink = async () => {
    try {
      await navigator.clipboard.writeText(canonicalSearchUrl(window.location.href, state))
      setNotice('Copied canonical search link.')
    } catch {
      setNotice('Clipboard unavailable.')
    }
  }

  const exportAll = () => {
    const blob = new Blob([serializeSavedSearches(searches)], { type: 'application/json' })
    const url = URL.createObjectURL(blob)
    const anchor = document.createElement('a')
    anchor.href = url
    anchor.download = 'eidos-saved-searches.json'
    anchor.click()
    URL.revokeObjectURL(url)
    setNotice(`Exported ${searches.length} saved ${searches.length === 1 ? 'search' : 'searches'}.`)
  }

  const importFile = async (file?: File) => {
    if (!file) return
    try {
      const result = importSavedSearches(searches, await file.text())
      const detail = [
        `${result.imported} imported`,
        result.renamed ? `${result.renamed} renamed to avoid conflicts` : '',
        result.discarded ? `${result.discarded} invalid ignored` : '',
      ]
        .filter(Boolean)
        .join(' · ')
      commit(result.searches, detail)
    } catch (error) {
      setNotice((error as Error).message)
    } finally {
      if (importInput.current) importInput.current.value = ''
    }
  }

  const collision = dialog ? nameCollision(searches, dialog.name, dialog.kind === 'rename' ? dialog.id : undefined) : undefined

  return (
    <>
      <div className="search-actions">
        <button type="button" className="btn small" onClick={() => setDialog({ kind: 'save', name: '' })}>
          Save search
        </button>
        <button type="button" className="btn small" onClick={copyLink}>
          Copy link
        </button>
        <details className="menu saved-search-menu">
          <summary className="btn small">Saved ({searches.length})</summary>
          <div className="menu-body">
            <strong>Saved searches</strong>
            {searches.length === 0 ? (
              <span className="muted">No searches saved in this browser.</span>
            ) : (
              <ul className="saved-search-list">
                {searches.map((saved) => (
                  <li key={saved.id}>
                    <button type="button" className="saved-search-run" title={saved.state.q} onClick={() => onRun(saved.state)}>
                      <strong>{saved.name}</strong>
                      <span>{saved.state.q || '(match everything)'}</span>
                    </button>
                    <div className="saved-search-buttons">
                      <button
                        type="button"
                        className="linkish"
                        onClick={() => setDialog({ kind: 'rename', id: saved.id, name: saved.name })}
                      >
                        Rename
                      </button>
                      <button type="button" className="linkish" onClick={() => duplicate(saved.id)}>
                        Duplicate
                      </button>
                      <button type="button" className="linkish danger" onClick={() => remove(saved)}>
                        Delete
                      </button>
                    </div>
                  </li>
                ))}
              </ul>
            )}
            <div className="saved-search-io">
              <button type="button" className="btn small" onClick={() => importInput.current?.click()}>
                Import JSON
              </button>
              <button type="button" className="btn small" onClick={exportAll} disabled={searches.length === 0}>
                Export JSON
              </button>
              <input
                ref={importInput}
                type="file"
                accept="application/json,.json"
                hidden
                onChange={(event) => importFile(event.target.files?.[0])}
              />
            </div>
          </div>
        </details>
        {notice && (
          <span className="muted search-action-notice" role="status">
            {notice}
          </span>
        )}
      </div>
      {dialog && (
        <div className="modal-backdrop" role="presentation" onMouseDown={() => setDialog(null)}>
          <div className="modal saved-search-dialog" role="dialog" aria-modal="true" aria-labelledby="saved-search-title" onMouseDown={(event) => event.stopPropagation()}>
            <form
              className="form"
              onSubmit={(event) => {
                event.preventDefault()
                submitName()
              }}
            >
              <h1 id="saved-search-title">{dialog.kind === 'save' ? 'Save this search' : 'Rename saved search'}</h1>
              <label>
                Name
                <input
                  type="text"
                  value={dialog.name}
                  maxLength={80}
                  autoFocus
                  onChange={(event) => setDialog({ ...dialog, name: event.target.value, error: undefined })}
                />
              </label>
              {collision && dialog.kind === 'save' && (
                <div className="banner warn">Saving will replace “{collision.name}” with the current query and view.</div>
              )}
              {dialog.error && <div className="error-text">{dialog.error}</div>}
              <div className="actions">
                <button type="button" className="btn" onClick={() => setDialog(null)}>
                  Cancel
                </button>
                <button type="submit" className="btn primary">
                  {collision && dialog.kind === 'save' ? 'Replace' : dialog.kind === 'save' ? 'Save' : 'Rename'}
                </button>
              </div>
            </form>
          </div>
        </div>
      )}
    </>
  )
}

function loadLocal() {
  try {
    return loadSavedSearches(localStorage.getItem(SAVED_SEARCHES_STORAGE_KEY))
  } catch {
    return { searches: [], discarded: 1 }
  }
}
