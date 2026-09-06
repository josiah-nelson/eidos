import { afterEach, beforeEach, expect, test, vi } from 'vitest'
import { act, cleanup, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { MemoryRouter } from 'react-router'
import { api, type DeviceView } from '../api'
import DeviceResources from './DeviceResources'

const initial: DeviceView = {
  budget: { readers_per_device: 2, unresolved_shared_fallback: true, topology_draining: false,
    devices: [{ key: 'unresolved-shared-budget', sources: ['1', '2'], content_readers: 1, scan_threads: 1, peak_readers: 2 }] },
  sample_age_s: '1', stale: false, source_errors: { '2': 'network topology unavailable' },
  source_roots: { '1': 'fixture-first', '2': 'fixture-second' },
}
let client: QueryClient
beforeEach(() => {
  client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } })
  vi.spyOn(api, 'devices').mockResolvedValue(initial)
  vi.spyOn(api, 'setDeviceLimits').mockImplementation(async limits => {
    const next = { ...initial, budget: { ...initial.budget, ...limits } }
    vi.mocked(api.devices).mockResolvedValue(next)
    return next
  })
})
afterEach(() => { cleanup(); client.clear(); vi.restoreAllMocks() })
function mount() { render(<MemoryRouter><QueryClientProvider client={client}><DeviceResources /></QueryClientProvider></MemoryRouter>) }

test('shared membership, both reader classes, capacity and unknown reasons are visible', async () => {
  mount()
  await screen.findByText('At capacity — new readers wait')
  expect(screen.getByText(/one conservative budget/)).toBeTruthy()
  expect(screen.getByText(/network topology unavailable/)).toBeTruthy()
  expect(screen.getByText('Content readers')).toBeTruthy()
  expect(screen.getByText('Scan threads')).toBeTruthy()
  expect(screen.getAllByRole('link', { name: 'fixture-second' })[0].getAttribute('href')).toBe('/sources/2')
})

test('a draft survives polling and a remote change, then saves explicitly', async () => {
  mount()
  const input = await screen.findByLabelText('Readers per backing device') as HTMLInputElement
  await userEvent.clear(input)
  await userEvent.type(input, '3')
  act(() => client.setQueryData(['devices'], { ...initial, budget: { ...initial.budget, readers_per_device: 4 } }))
  await screen.findByText(/device limit changed elsewhere/)
  expect(input.value).toBe('3')
  expect(api.setDeviceLimits).not.toHaveBeenCalled()
  await userEvent.click(screen.getByRole('button', { name: 'Save device limit' }))
  await screen.findByText(/Device limit saved/)
  expect(api.setDeviceLimits).toHaveBeenCalledWith({ readers_per_device: 3 }, expect.anything())
})

test('an untouched form follows remote limits and a failed save retains the draft', async () => {
  mount()
  const input = await screen.findByLabelText('Readers per backing device') as HTMLInputElement
  act(() => client.setQueryData(['devices'], { ...initial, budget: { ...initial.budget, readers_per_device: 5 } }))
  await waitFor(() => expect(input.value).toBe('5'))
  vi.mocked(api.setDeviceLimits).mockRejectedValueOnce(new Error('settings are not writable'))
  await userEvent.click(screen.getByRole('button', { name: 'Save device limit' }))
  await screen.findByText('settings are not writable')
  expect(input.value).toBe('5')
  expect(screen.queryByText(/Device limit saved/)).toBeNull()
})

test('topology draining, stale state and HTTP failure do not claim available capacity', async () => {
  vi.mocked(api.devices).mockResolvedValue({ ...initial, stale: true, sample_age_s: '91', budget: { ...initial.budget, topology_draining: true, readers_per_device: 4 } })
  mount()
  await screen.findByText(/New admissions wait for existing reservations/)
  expect(screen.getByText(/91s old.*stale or unavailable/)).toBeTruthy()
  expect(screen.getByText('Waiting for topology drain')).toBeTruthy()
  expect(screen.queryByText('Available')).toBeNull()
  vi.mocked(api.devices).mockRejectedValue(new Error('device endpoint unavailable'))
  await act(async () => { await client.invalidateQueries({ queryKey: ['devices'] }) })
  await screen.findByText('device endpoint unavailable')
})
