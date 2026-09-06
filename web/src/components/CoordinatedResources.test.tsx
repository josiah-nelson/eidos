import { afterEach, beforeEach, expect, test, vi } from 'vitest'
import { act, cleanup, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { api, type ResourceSettings, type ResourceSettingsView } from '../api'
import CoordinatedResources from './CoordinatedResources'

const settings: ResourceSettings = {
  content_workers: 4,
  scan_threads: 4,
  concurrent_scans: 1,
  minimum_free_mib: 1024,
  readers_per_device: 2,
}
const initial: ResourceSettingsView = {
  current: settings,
  outcome: 'current',
  pending: null,
  error: null,
}
let client: QueryClient

beforeEach(() => {
  client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } })
  vi.spyOn(api, 'resourceSettings').mockResolvedValue(initial)
  vi.spyOn(api, 'applyResourceSettings').mockImplementation(async current => {
    const result: ResourceSettingsView = {
      current, outcome: 'applied', pending: null, error: null,
    }
    vi.mocked(api.resourceSettings).mockResolvedValue(result)
    return result
  })
  vi.spyOn(api, 'repairResourceSettings').mockResolvedValue(initial)
})
afterEach(() => { cleanup(); client.clear(); vi.restoreAllMocks() })

function mount() {
  render(<QueryClientProvider client={client}><CoordinatedResources /></QueryClientProvider>)
}

test('applies the complete custom tuple while explaining independent source caps', async () => {
  mount()
  const workers = await screen.findByLabelText('Content workers') as HTMLInputElement
  await userEvent.clear(workers)
  await userEvent.type(workers, '3')
  await userEvent.click(screen.getByRole('button', { name: 'Apply coordinated settings' }))
  await screen.findByText(/Coordinated settings applied/)
  expect(api.applyResourceSettings).toHaveBeenCalledWith({ ...settings, content_workers: 3 }, expect.anything())
  expect(screen.getByText(/Per-source reader caps remain independent/)).toBeTruthy()
  expect(screen.getByText(/No measured preset is selected/)).toBeTruthy()
})

test('polling preserves a draft and exposes a changed live tuple', async () => {
  mount()
  const workers = await screen.findByLabelText('Content workers') as HTMLInputElement
  await userEvent.clear(workers)
  await userEvent.type(workers, '3')
  act(() => client.setQueryData(['resource-settings'], {
    ...initial, current: { ...settings, content_workers: 6 },
  }))
  expect(workers.value).toBe('3')
  await screen.findByText(/live resource tuple changed/)
  await userEvent.click(screen.getByRole('button', { name: 'Load the current tuple' }))
  await waitFor(() => expect(workers.value).toBe('6'))
})

test('partial application names the remaining component and repairs explicitly', async () => {
  const partial: ResourceSettingsView = {
    current: { ...settings, scan_threads: 2 },
    outcome: 'partial',
    error: 'device settings are not writable',
    pending: {
      target: { ...settings, scan_threads: 2, readers_per_device: 1 },
      completed_components: ['metadata_limits'],
      next_component: 'device_readers',
      failed_component: 'device_readers',
      error: 'device settings are not writable',
      cleanup_pending: false,
    },
  }
  vi.mocked(api.resourceSettings).mockResolvedValue(partial)
  vi.mocked(api.repairResourceSettings).mockImplementation(async () => {
    const result: ResourceSettingsView = {
      current: partial.pending!.target, outcome: 'applied', pending: null, error: null,
    }
    vi.mocked(api.resourceSettings).mockResolvedValue(result)
    return result
  })
  mount()
  await screen.findByText(/next is device reader limit/)
  expect((screen.getByRole('button', { name: 'Apply coordinated settings' }) as HTMLButtonElement).disabled).toBe(true)
  await userEvent.click(screen.getByRole('button', { name: 'Repair coordinated save' }))
  await waitFor(() => expect(api.repairResourceSettings).toHaveBeenCalledTimes(1))
  await waitFor(() => expect(screen.queryByText(/needs repair/)).toBeNull())
})
