import { afterEach, beforeEach, expect, test, vi } from 'vitest'
import { act, cleanup, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { api, type ResourceView } from '../api'
import ResourceControls from './ResourceControls'

const initial: ResourceView = {
  limits: { scan_threads: 8, concurrent_scans: 1, minimum_free_mib: 1024 },
  active_scans: 0, free_bytes: '4294967296', disk_sample_age_s: '0', admission_blocked: null,
}
let client: QueryClient
beforeEach(() => {
  client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } })
  vi.spyOn(api, 'resources').mockResolvedValue(initial)
  vi.spyOn(api, 'setResources').mockImplementation(async limits => ({ ...initial, limits }))
})
afterEach(() => { cleanup(); client.clear() })
function mount() { render(<QueryClientProvider client={client}><ResourceControls /></QueryClientProvider>) }

test('explicit save persists a draft that survives status polling', async () => {
  mount()
  const input = await screen.findByLabelText('Threads per metadata scan') as HTMLInputElement
  await userEvent.clear(input)
  await userEvent.type(input, '3')
  act(() => client.setQueryData(['resources'], { ...initial, active_scans: 1 }))
  expect(input.value).toBe('3')
  expect(api.setResources).not.toHaveBeenCalled()
  await userEvent.click(screen.getByRole('button', { name: 'Save resource limits' }))
  await screen.findByText(/Resource limits saved/)
  expect(api.setResources).toHaveBeenCalledWith({ scan_threads: 3, concurrent_scans: 1, minimum_free_mib: 1024 }, expect.anything())
})

test('save failure stays visible and retains values for a retry', async () => {
  vi.mocked(api.setResources).mockRejectedValueOnce(new Error('data directory not writable'))
  mount()
  const input = await screen.findByLabelText('Concurrent metadata scans') as HTMLInputElement
  await userEvent.clear(input)
  await userEvent.type(input, '2')
  await userEvent.click(screen.getByRole('button', { name: 'Save resource limits' }))
  await screen.findByText('data directory not writable')
  expect(input.value).toBe('2')
  expect(screen.queryByText(/Resource limits saved/)).toBeNull()
  await userEvent.click(screen.getByRole('button', { name: 'Save resource limits' }))
  await screen.findByText(/Resource limits saved/)
  await waitFor(() => expect(api.setResources).toHaveBeenCalledTimes(2))
})

test('disk-pressure reason is visible and polling shows recovery', async () => {
  vi.mocked(api.resources).mockResolvedValue({ ...initial, admission_blocked: 'data volume below reserve' })
  mount()
  await screen.findByText(/New scans and content reads are waiting: data volume below reserve/)
  act(() => client.setQueryData(['resources'], initial))
  await waitFor(() => expect(screen.queryByText(/New scans and content reads are waiting/)).toBeNull())
})

test('an untouched form follows limits changed elsewhere', async () => {
  mount()
  const input = await screen.findByLabelText('Threads per metadata scan') as HTMLInputElement
  expect(input.value).toBe('8')
  act(() => client.setQueryData(['resources'], {
    ...initial, limits: { scan_threads: 4, concurrent_scans: 2, minimum_free_mib: 512 },
  }))
  await waitFor(() => expect(input.value).toBe('4'))
  expect(screen.queryByText(/changed elsewhere/)).toBeNull()
})

test('a draft survives a conflicting change and can adopt the current values', async () => {
  mount()
  const input = await screen.findByLabelText('Threads per metadata scan') as HTMLInputElement
  await userEvent.clear(input)
  await userEvent.type(input, '3')
  act(() => client.setQueryData(['resources'], {
    ...initial, limits: { scan_threads: 4, concurrent_scans: 2, minimum_free_mib: 512 },
  }))
  // The operator's edit is never discarded, but the conflict is visible.
  expect(input.value).toBe('3')
  await screen.findByText(/changed elsewhere/)
  await userEvent.click(screen.getByRole('button', { name: 'Load the current values' }))
  expect(input.value).toBe('4')
  expect(screen.queryByText(/changed elsewhere/)).toBeNull()
})

test('a save landing back from the server is not reported as a conflict', async () => {
  mount()
  const input = await screen.findByLabelText('Threads per metadata scan') as HTMLInputElement
  await userEvent.clear(input)
  await userEvent.type(input, '3')
  await userEvent.click(screen.getByRole('button', { name: 'Save resource limits' }))
  await screen.findByText(/Resource limits saved/)
  act(() => client.setQueryData(['resources'], {
    ...initial, limits: { scan_threads: 3, concurrent_scans: 1, minimum_free_mib: 1024 },
  }))
  expect(screen.queryByText(/changed elsewhere/)).toBeNull()
  expect(input.value).toBe('3')
})
