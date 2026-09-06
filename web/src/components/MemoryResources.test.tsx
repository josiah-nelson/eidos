import { afterEach, beforeEach, expect, test, vi } from 'vitest'
import { act, cleanup, render, screen, waitFor } from '@testing-library/react'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { api, type MemoryView } from '../api'
import MemoryResources from './MemoryResources'

const initial: MemoryView = {
  process: { pid: 42, resident_bytes: '104857600', peak_resident_bytes: '209715200', private_commit_bytes: '314572800' },
  sample_age_s: '1', stale: false, error: null,
  catalog: { baseline_connections: 13, page_cache_per_connection_bytes: '67108864', page_cache_baseline_target_bytes: '872415232', mmap_per_connection_limit_bytes: '1099511627776' },
  catalog_writer_budget_bytes: '100663296', content_writer_budget_bytes: '268435456',
}
let client: QueryClient
beforeEach(() => {
  client = new QueryClient({ defaultOptions: { queries: { retry: false } } })
  vi.spyOn(api, 'memory').mockResolvedValue(initial)
})
afterEach(() => { cleanup(); client.clear(); vi.restoreAllMocks() })
function mount() { render(<QueryClientProvider client={client}><MemoryResources /></QueryClientProvider>) }

test('shows process consumption separately from accurately scoped budgets', async () => {
  mount()
  await screen.findByText(/Process 42/)
  expect(screen.getByText('Resident RAM / working set')).toBeTruthy()
  expect(screen.getByText(/13 baseline connections/)).toBeTruthy()
  expect(screen.getByText(/Per open scan session, in addition to baseline/)).toBeTruthy()
  expect(screen.getByText(/not allocated RAM/)).toBeTruthy()
  expect(screen.getByText(/Do not add them to resident RAM/)).toBeTruthy()
})

test('a failed process probe retains budgets and shows unavailable rather than zero', async () => {
  vi.mocked(api.memory).mockResolvedValue({ ...initial, process: null, stale: true, error: 'probe failed' })
  mount()
  await screen.findByRole('alert')
  expect(screen.getAllByText('unavailable')).toHaveLength(3)
  expect(screen.getByText('Content-index writer')).toBeTruthy()
  act(() => client.setQueryData(['memory'], initial))
  await waitFor(() => expect(screen.queryByRole('alert')).toBeNull())
  expect(screen.queryByText(/awaiting a fresh sample/)).toBeNull()
})

test('cold and stale samples are explicit, and absent platform counters stay unavailable', async () => {
  vi.mocked(api.memory).mockResolvedValue({ ...initial, process: null, stale: true, sample_age_s: null })
  mount()
  await screen.findByText(/Process memory pending or unavailable/)
  act(() => client.setQueryData(['memory'], { ...initial, stale: true, sample_age_s: '60', process: { ...initial.process, peak_resident_bytes: null, private_commit_bytes: null } }))
  await screen.findByText(/sampled 60s ago.*awaiting a fresh sample/)
  expect(screen.getAllByText('unavailable')).toHaveLength(2)
})

test('an HTTP error is visible instead of silently blank diagnostics', async () => {
  vi.mocked(api.memory).mockRejectedValue(new Error('memory endpoint unavailable'))
  mount()
  await screen.findByText(/memory endpoint unavailable/)
})
