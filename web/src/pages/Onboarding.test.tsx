import { afterEach, beforeEach, expect, test, vi } from 'vitest'
import { act, cleanup, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { MemoryRouter } from 'react-router'
import { api, type AddedSource, type FleetStatus, type VolumeCandidateView } from '../api'
import SourcesPage from './SourcesPage'
import Onboarding from './Onboarding'

const clients: QueryClient[] = []
const fleet = {
  node_id: 'fixture-node', name: 'fixture', fingerprint: 'fixture',
  central: false, enrolled: false, sync_enabled: false, peers: [], sessions: [],
  local_sources: [], replica_sources: [], counters: { connections_attempted: '0', connections_established_outbound: '0', connections_established_inbound: '0', connections_refused_unknown_peer: '0', connections_refused_version: '0', duplicate_sessions_closed: '0', disconnects: '0', join_approvals: '0', offers_sent: '0', offers_received: '0', batches_sent: '0', batches_applied: '0', rows_shipped: '0', rows_applied: '0', acks_sent: '0', acks_received: '0', duplicates_acknowledged: '0', stale_batches: '0', rejections_received: '0', rejections_sent: '0', fences: '0', full_resyncs: '0', repairs_offered: '0', repairs_applied: '0', repair_rows_applied: '0', frames_refused_oversize: '0', frames_malformed: '0', bytes_control_sent: '0', bytes_control_received: '0', bytes_catalog_sent: '0', bytes_catalog_received: '0', bytes_repair_sent: '0', bytes_repair_received: '0', materialize_ms_total: '0', apply_ms_total: '0', backfill_steps: '0', collections: '0', tombstones_collected: '0' }, degraded: [],
  join_requests: [], discovered_masters: [],
} as FleetStatus
const volume = (root: string): VolumeCandidateView => ({
  root, drive_type: 'fixed', filesystem: 'NTFS', volume_name: '',
  total_bytes: '10000', free_bytes: '9000', supports_usn: true, already_indexed: false,
})
function added(root = 'D:\\', id = '1'): AddedSource {
  return {
    source: {
      id, host_id: '1', name: root, kind: 'windows_local', root_path: root,
      aliases: [], state: 'new', state_reason: null, policy_version: 1,
      root_object_id: null, published_generation: null, volume_id: null,
      preserve_offline: true, reconcile_interval_s: null, content_enabled: true,
      content_concurrency: 2, sync_policy: 'inherit', checkpoint_kind: null,
      checkpoint_at: null, last_scan_started_at: null, last_scan_completed_at: null,
      created_at: '0', updated_at: '0',
    },
    counts: {
      objects: '0', entries: '0', directories: '0', files: '0', logical_bytes: '0',
      allocated_bytes: '0', content_pending: '0', content_indexed: '0', content_failed: '0',
      content_excluded: '0', content_unsupported: '0', open_errors: '0',
    },
    completeness: {
      source_id: id, name: root, state: 'new', metadata_complete: false,
      content_complete: false, content_not_replicated: false, content_pending: '0',
      content_failed: '0', listing_errors: '0', freshness: 'unknown',
    },
  }
}
function mount(page = <Onboarding onDone={vi.fn()} onManual={vi.fn()} />) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } })
  clients.push(client)
  render(<QueryClientProvider client={client}><MemoryRouter>{page}</MemoryRouter></QueryClientProvider>)
  return client
}
async function chooseStandalone() {
  await userEvent.click(await screen.findByLabelText(/Standalone/))
  await userEvent.click(screen.getByRole('button', { name: 'Choose sources' }))
  await screen.findByRole('heading', { name: 'Choose drives' })
}
beforeEach(() => {
  vi.spyOn(api, 'fleetStatus').mockResolvedValue(fleet)
  vi.spyOn(api, 'volumes').mockResolvedValue([volume('D:\\'), volume('E:\\')])
  vi.spyOn(api, 'sources').mockResolvedValue([])
  vi.spyOn(api, 'setFleetCentral').mockResolvedValue({} as Awaited<ReturnType<typeof api.setFleetCentral>>)
})
afterEach(() => {
  cleanup()
  clients.splice(0).forEach(client => client.clear())
})

test('role comes first and fixed drives are never preselected', async () => {
  mount()
  expect(screen.queryByRole('checkbox', { name: 'Index D:\\' })).toBeNull()
  await chooseStandalone()
  expect((screen.getByRole('checkbox', { name: 'Index D:\\' }) as HTMLInputElement).checked).toBe(false)
  expect((screen.getByRole('button', { name: 'Add selected drives' }) as HTMLButtonElement).disabled).toBe(true)
  expect(api.setFleetCentral).toHaveBeenCalledWith({ central: false, listen: '' })
})

test('master choice configures discovery before advancing', async () => {
  mount()
  await userEvent.click(await screen.findByLabelText(/Master —/))
  await userEvent.click(screen.getByRole('button', { name: 'Choose sources' }))
  await screen.findByRole('heading', { name: 'Choose drives' })
  expect(api.setFleetCentral).toHaveBeenCalledWith({ central: true })
})

test('role error is visible and does not silently advance', async () => {
  vi.mocked(api.setFleetCentral).mockRejectedValue(new Error('listener unavailable'))
  mount()
  await userEvent.click(await screen.findByLabelText(/Master —/))
  await userEvent.click(screen.getByRole('button', { name: 'Choose sources' }))
  await screen.findByText(/listener unavailable/)
  expect(screen.queryByRole('heading', { name: 'Choose drives' })).toBeNull()
})

test('master address survives fleet polling; join failure stays actionable', async () => {
  vi.spyOn(api, 'requestFleetJoin').mockRejectedValue(new Error('master unreachable'))
  const client = mount()
  await userEvent.click(await screen.findByLabelText(/Join an existing/))
  const input = screen.getByLabelText('Master IP or host')
  await userEvent.type(input, '192.0.2.20')
  act(() => client.setQueryData(['fleet'], { ...fleet, listening: '0.0.0.0:7701' }))
  expect((input as HTMLInputElement).value).toBe('192.0.2.20')
  await userEvent.click(screen.getByRole('button', { name: 'Request to join' }))
  await screen.findByText('master unreachable')
  expect(api.requestFleetJoin).toHaveBeenCalledWith('192.0.2.20')
})

test('pending and rejected requests are visible and can be cleared', async () => {
  const pending = { request_id: 'fixture-request', master_name: 'fixture-master', endpoint: '192.0.2.20:7701', requested_at: '0', master_fingerprint: 'fixture', rejected_reason: 'not this fleet' }
  vi.mocked(api.fleetStatus).mockResolvedValue({ ...fleet, pending_join: pending } as FleetStatus)
  vi.spyOn(api, 'cancelFleetJoin').mockImplementation(async () => {
    vi.mocked(api.fleetStatus).mockResolvedValue(fleet)
    return {} as Awaited<ReturnType<typeof api.cancelFleetJoin>>
  })
  mount()
  await screen.findByText('Join rejected')
  expect((screen.getByRole('button', { name: 'Choose sources' }) as HTMLButtonElement).disabled).toBe(true)
  await userEvent.click(screen.getByRole('button', { name: 'Clear & try again' }))
  await screen.findByLabelText(/Standalone/)
  expect(api.cancelFleetJoin).toHaveBeenCalledOnce()
})

test('partial success stays mounted through Sources polling and retries only the failed drive', async () => {
  const create = vi.spyOn(api, 'addSource').mockImplementation(async body => {
    if (body.root_path === 'E:\\') throw new Error('drive offline')
    vi.mocked(api.sources).mockResolvedValue([added()])
    return added()
  })
  mount(<SourcesPage />)
  await chooseStandalone()
  await userEvent.click(screen.getByRole('checkbox', { name: 'Index D:\\' }))
  await userEvent.click(screen.getByRole('checkbox', { name: 'Index E:\\' }))
  await userEvent.click(screen.getByRole('button', { name: 'Add selected drives' }))
  await screen.findByText('drive offline')
  expect(screen.getByText('Source added.')).toBeTruthy()
  expect(screen.getByRole('heading', { name: 'Choose drives' })).toBeTruthy()
  create.mockResolvedValue(added('E:\\', '2'))
  await userEvent.click(screen.getByRole('button', { name: 'Retry failed drives' }))
  await waitFor(() => expect(create).toHaveBeenCalledTimes(3))
  expect(create.mock.calls.map(([body]) => body.root_path)).toEqual(['D:\\', 'E:\\', 'E:\\'])
})

test('scan startup failure retries the created source ID without recreating it', async () => {
  const create = vi.spyOn(api, 'addSource').mockResolvedValue({ ...added(), scan_error: 'thread unavailable' })
  const scan = vi.spyOn(api, 'scanSource').mockResolvedValue({} as Awaited<ReturnType<typeof api.scanSource>>)
  mount()
  await chooseStandalone()
  await userEvent.click(screen.getByRole('checkbox', { name: 'Index D:\\' }))
  await userEvent.click(screen.getByRole('button', { name: 'Add selected drives' }))
  await screen.findByText('Scan did not start: thread unavailable')
  await userEvent.click(screen.getByRole('button', { name: 'Retry scan' }))
  await screen.findByText('Scan started.')
  expect(scan).toHaveBeenCalledWith('1')
  expect(create).toHaveBeenCalledOnce()
})

test('manual add keeps its successful result when initial scanning fails', async () => {
  const create = vi.spyOn(api, 'addSource').mockResolvedValue({ ...added(), scan_error: 'thread unavailable' })
  mount(<SourcesPage />)
  await userEvent.click(screen.getByRole('button', { name: 'Add source' }))
  await userEvent.type(screen.getByLabelText('Name'), 'documents')
  await userEvent.type(screen.getByLabelText('Root path'), 'D:\\documents')
  await userEvent.click(screen.getByRole('button', { name: 'Add' }))
  await screen.findByText('Scan did not start: thread unavailable')
  expect(screen.getByRole('button', { name: 'Retry scan' })).toBeTruthy()
  expect(create).toHaveBeenCalledOnce()
})
