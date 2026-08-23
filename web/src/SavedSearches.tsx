import { useEffect, useRef, useState } from 'react'
import { createBrowserLock } from './browser-lock'
import {
  MAX_SAVED_SEARCH_BYTES,
  SAVED_SEARCHES_STORAGE_KEY,
  canonicalSearchUrl,
  deleteSavedSearch,
  duplicateSavedSearch,
  importSavedSearches,
  loadSavedSearches,
  nameCollision,
  renameSavedSearch,
  serializeSavedSearches,
  updateSavedSearches,
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

const browserLock = createBrowserLock()

export function SavedSearchControls({ state, onRun }: Props) {
  const [initial] = useState(loadLocal)
  const [searches, setSearches] = useState(initial.searches)
  const [dialog, setDialog] = useState<NameDialog | null>(null)
  const [notice, setNotice] = useState(initial.discarded ? 'Ignored invalid saved-search data.' : '')
  const importInput = useRef<HTMLInputElement>(null)
  const dialogElement = useRef<HTMLDialogElement>(null)

  useEffect(() => {
    const sync = (event: StorageEvent) => {
      if (event.key !== SAVED_SEARCHES_STORAGE_KEY) return
      const loaded = loadSavedSearches(event.newValue)
      setSearches(loaded.searches)
      setNotice(loaded.discarded ? 'Another tab saved invalid data; invalid rows were ignored.' : 'Saved searches updated in another tab.')
    }
    window.addEventListener('storage', sync)
    return () => window.removeEventListener('storage', sync)
  }, [])

  useEffect(() => {
    if (dialog && dialogElement.current && !dialogElement.current.open) dialogElement.current.showModal()
  }, [dialog])

  const closeDialog = () => {
    dialogElement.current?.close()
    setDialog(null)
  }

  const commit = async (update: (latest: SavedSearch[]) => { searches: SavedSearch[]; value: string }) => {
    const result = await updateSavedSearches(localStorage, browserLock, update)
    setSearches(result.searches)
    setNotice(result.value)
  }

  const submitName = async () => {
    if (!dialog) return
    try {
      await commit((latest) => {
        if (dialog.kind === 'save') {
          const result = upsertSavedSearch(latest, dialog.name, state)
          const value = result.replaced ? `Replaced “${result.saved.name}”.` : `Saved “${result.saved.name}”.`
          return { searches: result.searches, value }
        }
        return { searches: renameSavedSearch(latest, dialog.id!, dialog.name), value: `Renamed to “${dialog.name.trim()}”.` }
      })
      closeDialog()
    } catch (error) {
      setDialog({ ...dialog, error: (error as Error).message })
    }
  }

  const duplicate = async (id: string) => {
    try {
      await commit((latest) => {
        const result = duplicateSavedSearch(latest, id)
        return { searches: result.searches, value: `Created “${result.saved.name}”.` }
      })
    } catch (error) {
      setNotice((error as Error).message)
    }
  }

  const remove = async (saved: SavedSearch) => {
    if (!window.confirm(`Delete the saved search “${saved.name}”?`)) return
    try {
      await commit((latest) => ({ searches: deleteSavedSearch(latest, saved.id), value: `Deleted “${saved.name}”.` }))
    } catch (error) {
      setNotice((error as Error).message)
    }
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
      if (file.size > MAX_SAVED_SEARCH_BYTES) throw new Error('Saved-search imports must be 1 MB or smaller.')
      const raw = await file.text()
      await commit((latest) => {
        const result = importSavedSearches(latest, raw)
        const value = [
          `${result.imported} imported`,
          result.renamed ? `${result.renamed} renamed to avoid conflicts` : '',
          result.discarded ? `${result.discarded} invalid ignored` : '',
        ]
          .filter(Boolean)
          .join(' · ')
        return { searches: result.searches, value }
      })
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
                      <button type="button" className="linkish" onClick={() => void duplicate(saved.id)}>
                        Duplicate
                      </button>
                      <button type="button" className="linkish danger" onClick={() => void remove(saved)}>
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
        <dialog
          ref={dialogElement}
          className="modal saved-search-dialog"
          aria-labelledby="saved-search-title"
          onCancel={(event) => {
            event.preventDefault()
            closeDialog()
          }}
          onMouseDown={(event) => {
            if (event.target === event.currentTarget) closeDialog()
          }}
        >
          <form
            className="form"
            onSubmit={(event) => {
              event.preventDefault()
                void submitName()
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
              <button type="button" className="btn" onClick={closeDialog}>
                Cancel
              </button>
              <button type="submit" className="btn primary">
                {collision && dialog.kind === 'save' ? 'Replace' : dialog.kind === 'save' ? 'Save' : 'Rename'}
              </button>
            </div>
          </form>
        </dialog>
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
