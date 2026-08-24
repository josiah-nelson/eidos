import { useEffect } from 'react'
import { useQuery } from '@tanstack/react-query'
import { NavLink, Outlet } from 'react-router'
import { api } from './api'
import { installInteractionFlush } from './interactions'

export default function App() {
  const health = useQuery({ queryKey: ['health'], queryFn: api.health, refetchInterval: 5000 })
  // Queued interaction events would otherwise be lost when the tab goes away.
  useEffect(installInteractionFlush, [])
  return (
    <div className="shell">
      <header className="topbar">
        <div className="brand">
          eidos<small>filesystem indexer · v0.5 dev</small>
        </div>
        <div className="spacer" />
        <div className="health">
          {health.isError ? (
            <span className="badge bad">service unreachable</span>
          ) : health.data ? (
            <>
              <span className="badge ok">connected</span>{' '}
              <span>
                {health.data.host} · {health.data.sources} sources
                {health.data.running_scans > 0 ? ` · ${health.data.running_scans} scanning` : ''}
              </span>
            </>
          ) : (
            <span className="badge">connecting…</span>
          )}
        </div>
      </header>
      <nav className="sidenav">
        <div className="section">Catalog</div>
        <NavLink to="/sources" className={({ isActive }) => (isActive ? 'active' : '')}>
          Sources
        </NavLink>
        <NavLink to="/browse" className={({ isActive }) => (isActive ? 'active' : '')}>
          Browse
        </NavLink>
        <div className="section">Search</div>
        <NavLink to="/search" className={({ isActive }) => (isActive ? 'active' : '')}>
          Search
        </NavLink>
        <div className="section">Operations</div>
        <NavLink to="/activity" className={({ isActive }) => (isActive ? 'active' : '')}>
          Activity
        </NavLink>
        <a className="disabled" title="Milestone 5">
          Policies
        </a>
      </nav>
      <main className="main">
        <Outlet />
      </main>
    </div>
  )
}
