//! Fixture-driven tests. The fixtures are duplicated in
//! `packages/protocol/fixtures` and read by the Rust suite too, so a change made
//! on either side of the wire shows up as a failing test rather than a silent
//! mismatch at runtime.

import { readFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { describe, expect, it } from 'vitest'
import type { Presence } from '../src/staleness'
import { classify, estimateSkew, formatAge, offlineAfter } from '../src/staleness'

const here = dirname(fileURLToPath(import.meta.url))
const fixtures = join(here, '../../../packages/protocol/fixtures')

function load(name: string): Presence {
  return JSON.parse(readFileSync(join(fixtures, name), 'utf8')) as Presence
}

describe('fixtures', () => {
  it('parses the active snapshot and keeps every field', () => {
    const p = load('active-with-media.json')
    expect(p.status).toBe('active')
    expect(p.focus?.app).toBe('code')
    expect(p.media?.app).toBe('cloudmusic')
    expect(p.media?.title).toBeTruthy()
  })

  it('represents empty blocks as explicit null rather than a missing key', () => {
    for (const name of ['idle-nothing.json', 'paused.json']) {
      const raw = readFileSync(join(fixtures, name), 'utf8')
      expect(raw).toContain('"focus": null')
      expect(raw).toContain('"media": null')
    }
  })
})

describe('classify', () => {
  const base = load('active-with-media.json')

  it('calls a fresh snapshot live', () => {
    const now = (base.ts + 5) * 1000
    expect(classify(base, now, 0).verdict).toBe('live')
  })

  it('calls a snapshot older than three heartbeats offline', () => {
    const stale = base.ts + offlineAfter(base) + 1
    expect(classify(base, stale * 1000, 0).verdict).toBe('offline')
  })

  it('treats paused as stale rather than live or offline', () => {
    const paused = load('paused.json')
    expect(classify(paused, (paused.ts + 5) * 1000, 0).verdict).toBe('stale')
  })

  it('honours clock skew instead of trusting the local clock', () => {
    // The reader's clock is an hour ahead; without correction a five-second-old
    // snapshot would look an hour old and be misreported as offline.
    const now = (base.ts + 3605) * 1000
    const skew = estimateSkew(base, now)
    expect(classify(base, now, skew).verdict).toBe('live')
  })
})

describe('formatAge', () => {
  it('switches units at each boundary', () => {
    expect(formatAge(3)).toBe('3 秒前')
    expect(formatAge(90)).toBe('1 分钟前')
    expect(formatAge(7200)).toBe('2 小时前')
    expect(formatAge(90000)).toBe('1 天前')
  })
})
