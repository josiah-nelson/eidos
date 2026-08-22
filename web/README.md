# eidos web UI

Vite + React 19 + TypeScript, TanStack Query for server state, TanStack
Virtual for large lists, react-router for pages. Talks to the service's
`/api` routes; in development the Vite dev server proxies to
`http://127.0.0.1:7700` (see `vite.config.ts`).

```powershell
npm ci
npm run dev      # http://localhost:5173 with API proxy; run `eidos serve` alongside
npm run lint     # oxlint
npm run build    # tsc -b && vite build → dist/ (served by `eidos serve --web-dir web\dist`)
```

Pages: search (query editor with live interpretation, virtualized results,
facets, explanation), browse (tree + canvas treemap), sources (health,
completeness, scans, errors, exclusions). Activity and policies pages arrive
with milestones 4 and 5.
