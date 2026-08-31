import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { createBrowserRouter, Navigate, RouterProvider } from 'react-router'
import App from './App'
import SourcesPage from './pages/SourcesPage'
import SourceDetailPage from './pages/SourceDetailPage'
import BrowsePage from './pages/BrowsePage'
import SearchPage from './pages/SearchPage'
import ActivityPage from './pages/ActivityPage'
import FleetPage from './pages/FleetPage'
import './styles.css'

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      retry: 1,
      refetchOnWindowFocus: false,
      staleTime: 2_000,
    },
  },
})

const router = createBrowserRouter([
  {
    path: '/',
    element: <App />,
    children: [
      { index: true, element: <Navigate to="/search" replace /> },
      { path: 'search', element: <SearchPage /> },
      { path: 'activity', element: <ActivityPage /> },
      { path: 'nodes', element: <FleetPage /> },
      { path: 'fleet', element: <Navigate to="/nodes" replace /> },
      { path: 'sources', element: <SourcesPage /> },
      { path: 'sources/:id', element: <SourceDetailPage /> },
      { path: 'browse/:objectId', element: <BrowsePage /> },
      { path: 'browse', element: <BrowsePage /> },
    ],
  },
])

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
    </QueryClientProvider>
  </StrictMode>,
)
