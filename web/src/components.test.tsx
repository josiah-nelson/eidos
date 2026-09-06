import { afterEach, expect, test } from 'vitest'
import { cleanup, render, screen } from '@testing-library/react'
import type { SourceCompleteness } from './generated/api'
import { CompletenessBanner } from './components'

afterEach(cleanup)

const base: SourceCompleteness = {
  source_id: '1',
  name: 'fixture',
  state: 'complete',
  metadata_complete: true,
  content_complete: true,
  content_not_replicated: false,
  content_pending: '0',
  content_failed: '0',
  listing_errors: '0',
  freshness: 'live',
}

test('a policy boundary never reads as "not scanned" or as "fully indexed"', () => {
  render(<CompletenessBanner c={{ ...base, metadata_complete: false, policy_note: 'storage boundaries are not enumerated' }} />)
  expect(screen.getByText(/storage boundaries are not enumerated/)).toBeTruthy()
  expect(screen.queryByText(/no published generation yet/)).toBeNull()
  expect(screen.queryByText(/fully indexed/)).toBeNull()
})

test('a policy note does not hide independent listing and content warnings', () => {
  render(
    <CompletenessBanner
      c={{ ...base, metadata_complete: false, policy_note: 'exclusion policy application is pending', listing_errors: '3', content_pending: '7' }}
    />,
  )
  expect(screen.getByText(/exclusion policy application is pending/)).toBeTruthy()
  expect(screen.getByText(/3 directories could not be listed/)).toBeTruthy()
  expect(screen.getByText(/7 files await content indexing/)).toBeTruthy()
})

test('without a policy note the existing banners are unchanged', () => {
  const { unmount } = render(<CompletenessBanner c={base} />)
  expect(screen.getByText(/fully indexed/)).toBeTruthy()
  unmount()
  render(<CompletenessBanner c={{ ...base, metadata_complete: false }} />)
  expect(screen.getByText(/no published generation yet/)).toBeTruthy()
})
