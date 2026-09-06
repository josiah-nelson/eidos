import { afterEach, beforeEach, expect, test, vi } from 'vitest'
import { act, cleanup, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { api, type ExclusionPolicy } from '../api'
import ExclusionEditor from './ExclusionEditor'

const initial: ExclusionPolicy = { revision: 0, engine_version: 3, rules: [], phase: 'applied', processed: '0', changed: '0', error: null, protected_directories: ['.eidos'], case_sensitive: false }
let client: QueryClient
beforeEach(() => {
  client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } })
  vi.spyOn(api, 'exclusionPolicy').mockResolvedValue(initial)
  vi.spyOn(api, 'applyExclusions').mockImplementation(async (_id, body) => ({ ...initial, revision: 1, rules: body.rules, phase: 'applying' }))
  vi.spyOn(api, 'previewExclusions').mockResolvedValue([])
  vi.spyOn(api, 'retryExclusions').mockResolvedValue({ ...initial, revision: 1, phase: 'purging' })
})
afterEach(() => { cleanup(); client.clear() })
function mount(remote = false) { render(<QueryClientProvider client={client}><ExclusionEditor sourceId="1" remote={remote} /></QueryClientProvider>) }
async function addFolder() {
  const folder = await screen.findByLabelText('Exclude a folder')
  await userEvent.type(folder, 'build/cache')
  await userEvent.click(screen.getByRole('button', { name: 'Add folder exclusion' }))
}

test('folder draft survives polling and only explicit Apply saves it', async () => {
  mount()
  await addFolder()
  act(() => client.setQueryData(['exclusion-policy', '1'], { ...initial, processed: '123' }))
  expect((screen.getByLabelText('Pattern 1') as HTMLInputElement).value).toBe('build/cache')
  expect(api.applyExclusions).not.toHaveBeenCalled()
  await userEvent.click(screen.getByRole('button', { name: 'Validate and preview' }))
  await screen.findByText(/Rules validated/)
  expect(api.applyExclusions).not.toHaveBeenCalled()
  await userEvent.click(screen.getByRole('button', { name: 'Apply to existing and future files' }))
  await screen.findByText(/Current files drain first/)
  expect(api.applyExclusions).toHaveBeenCalledWith('1', { expected_revision: 0, rules: [{ id: expect.any(String), kind: 'directory', pattern: 'build/cache', include: false }] })
})

test('validation and save errors retain editable draft and do not claim success', async () => {
  vi.mocked(api.previewExclusions).mockRejectedValueOnce(new Error('invalid regex'))
  vi.mocked(api.applyExclusions).mockRejectedValueOnce(new Error('a scan is open'))
  mount()
  await addFolder()
  await userEvent.click(screen.getByRole('button', { name: 'Validate and preview' }))
  await screen.findByText('invalid regex')
  await userEvent.click(screen.getByRole('button', { name: 'Apply to existing and future files' }))
  await screen.findByText('a scan is open')
  expect((screen.getByLabelText('Pattern 1') as HTMLInputElement).value).toBe('build/cache')
  expect(screen.queryByText(/Current files drain first/)).toBeNull()
})

test('a concurrent revision cannot overwrite an active draft', async () => {
  mount()
  await addFolder()
  act(() => client.setQueryData(['exclusion-policy', '1'], { ...initial, revision: 1, rules: [{ id: 'external', kind: 'directory', pattern: 'other', include: false }] }))
  await screen.findByText(/Policy changed elsewhere/)
  expect((screen.getByLabelText('Pattern 1') as HTMLInputElement).value).toBe('build/cache')
  const apply = screen.getByRole('button', { name: 'Apply to existing and future files' }) as HTMLButtonElement
  expect(apply.disabled).toBe(true)
  await userEvent.click(screen.getByRole('button', { name: 'Load saved policy' }))
  expect((screen.getByLabelText('Pattern 1') as HTMLInputElement).value).toBe('other')
  expect(screen.queryByText(/Policy changed elsewhere/)).toBeNull()
})

test('operator can order include and exclude regex rules and preview explanations', async () => {
  vi.mocked(api.previewExclusions).mockResolvedValue([{ path: 'build/cache/keep.txt', state: 'pending', reason: 'user_include', rule: 'operator:1:allow', catalogued: false }])
  mount()
  await addFolder()
  await userEvent.click(screen.getByRole('button', { name: 'Add regex rule' }))
  await userEvent.type(screen.getByLabelText('Pattern 2'), '^build/cache/keep')
  await userEvent.selectOptions(screen.getByLabelText('Action 2'), 'include')
  await userEvent.click(screen.getByRole('button', { name: 'Move rule 2 earlier' }))
  expect((screen.getByLabelText('Action 1') as HTMLSelectElement).value).toBe('include')
  await userEvent.click(screen.getByRole('button', { name: 'Move rule 1 later' }))
  await userEvent.type(screen.getByLabelText(/Preview file paths/), 'build/cache/keep.txt')
  await userEvent.click(screen.getByRole('button', { name: 'Validate and preview' }))
  await screen.findByText(/not catalogued; assumes a regular/)
  expect(api.previewExclusions).toHaveBeenCalledWith('1', { paths: ['build/cache/keep.txt'], rules: [expect.objectContaining({ include: false }), expect.objectContaining({ include: true })] })
  await userEvent.click(screen.getByRole('button', { name: 'Remove rule 2' }))
  expect(screen.queryByText(/Rules validated/)).toBeNull()
})

test('durable application errors and protected coverage are visible with retry', async () => {
  vi.mocked(api.exclusionPolicy).mockResolvedValue({ ...initial, revision: 1, phase: 'purging', error: 'disk full' })
  mount()
  await screen.findByText(/Application stopped: disk full/)
  await screen.findByText(/contents and totals are unknown or last-known/)
  await userEvent.click(screen.getByRole('button', { name: 'Retry application' }))
  await waitFor(() => expect(api.retryExclusions).toHaveBeenCalledWith('1'))
  await waitFor(() => expect(screen.queryByText(/Application stopped/)).toBeNull())
})

test('replica editor directs changes to origin without fetching local policy', async () => {
  mount(true)
  await screen.findByText(/Edit exclusions on this source's origin node/)
  expect(api.exclusionPolicy).not.toHaveBeenCalled()
  expect(screen.queryByRole('button', { name: 'Apply to existing and future files' })).toBeNull()
})
