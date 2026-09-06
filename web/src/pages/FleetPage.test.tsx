import { afterEach, expect, test, vi } from 'vitest'
import { cleanup, render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { MemoryRouter } from 'react-router'
import { api, type FleetStatus, type UpdateSettings, type UpdateState } from '../api'
import FleetPage from './FleetPage'

const fleet = {
  node_id: 'fixture', name: 'fixture', fingerprint: 'certificate', central: false,
  enrolled: false, sync_enabled: false, peers: [], sessions: [], local_sources: [],
  replica_sources: [], degraded: [], join_requests: [], discovered_masters: [], counters: {},
} as unknown as FleetStatus
const settings = {
  automatic_checks: true, expected_publisher: null, expected_product: 'Eidos',
  max_artifact_bytes: '268435456',
} as UpdateSettings
const state = {
  checks_enabled: true, current_version: '0.5.0', checked_at: null, check_error: null,
  latest_version: null, available: null, stage_phase: 'idle', stage_error: null, staged: null,
} as UpdateState

function mount() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } })
  render(<QueryClientProvider client={client}><MemoryRouter><FleetPage /></MemoryRouter></QueryClientProvider>)
}

afterEach(() => { cleanup(); vi.restoreAllMocks() })

test('Nodes presents staging as preparation and requires a discovered release', async () => {
  vi.spyOn(api, 'fleetStatus').mockResolvedValue(fleet)
  vi.spyOn(api, 'updateStatus').mockResolvedValue(state)
  vi.spyOn(api, 'updateSettings').mockResolvedValue(settings)
  const save = vi.spyOn(api, 'saveUpdateSettings').mockResolvedValue(state)
  mount()
  await screen.findByText(/Installation and fleet rollout are not enabled yet/)
  expect((screen.getByRole('button', { name: 'Verify & stage' }) as HTMLButtonElement).disabled).toBe(true)
  await userEvent.type(screen.getByLabelText('Expected publisher certificate subject'), 'CN=Test Publisher')
  await userEvent.click(screen.getByRole('button', { name: 'Save update settings' }))
  expect(save).toHaveBeenCalledWith(expect.objectContaining({ expected_publisher: 'CN=Test Publisher' }))
})

test('Nodes explains an unreachable settings endpoint instead of hiding the form', async () => {
  vi.spyOn(api, 'fleetStatus').mockResolvedValue(fleet)
  vi.spyOn(api, 'updateStatus').mockResolvedValue(state)
  vi.spyOn(api, 'updateSettings').mockRejectedValue(new Error('settings are unreadable'))
  mount()
  await screen.findByText(/Update settings unavailable: settings are unreadable/)
  expect(screen.queryByLabelText('Expected publisher certificate subject')).toBeNull()
})

test('Nodes reports a newer release it cannot stage as advice, not as a candidate', async () => {
  vi.spyOn(api, 'fleetStatus').mockResolvedValue(fleet)
  vi.spyOn(api, 'updateSettings').mockResolvedValue(settings)
  vi.spyOn(api, 'updateStatus').mockResolvedValue({ ...state, latest_version: '1.0.0' })
  mount()
  await screen.findByText(/Release 1.0.0 exists but is not a compatible upgrade for this build/)
  expect((screen.getByRole('button', { name: 'Verify & stage' }) as HTMLButtonElement).disabled).toBe(true)
})
