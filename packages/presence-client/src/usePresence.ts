import { onScopeDispose, ref, shallowRef, type Ref, type ShallowRef } from 'vue'
import type { Presence, Staleness } from './staleness'
import { classify, estimateSkew } from './staleness'

export interface UsePresenceOptions {
  /** Public URL of presence.json, e.g. `https://presence.example.com/presence.json`. */
  url: string
  /** Poll interval while the tab is visible, in milliseconds. */
  interval?: number
  /** Stop polling when the tab is hidden. On by default: an idle tab costs nothing. */
  pauseWhenHidden?: boolean
  /** Called on every successful fetch, handy for debugging. */
  onUpdate?: (presence: Presence) => void
}

export interface UsePresence {
  presence: Readonly<ShallowRef<Presence | null>>
  staleness: Readonly<ShallowRef<Staleness | null>>
  error: Readonly<Ref<string | null>>
  /** Fetch once, without waiting for the timer. */
  refresh: () => Promise<void>
  start: () => void
  stop: () => void
}

/**
 * Poll the published snapshot and derive how stale it is.
 *
 * Two details matter more than they look:
 *
 * The request is cache-busted with `no-store`. The object is served through a CDN
 * whose edge TTL is measured in seconds; without this the browser would happily
 * reuse a body for minutes.
 *
 * A body whose `seq` is not newer than the one already displayed is discarded.
 * Edge caches are eventually consistent across regions, so a late response can
 * legitimately carry an older snapshot than the one on screen.
 */
export function usePresence(options: UsePresenceOptions): UsePresence {
  const interval = options.interval ?? 15_000
  const pauseWhenHidden = options.pauseWhenHidden ?? true

  const presence = shallowRef<Presence | null>(null)
  const staleness = shallowRef<Staleness | null>(null)
  const error = ref<string | null>(null)

  let timer: ReturnType<typeof setTimeout> | null = null
  let inflight: AbortController | null = null
  let stopped = true
  let skewMs = 0
  let haveSkew = false
  let lastSeq = -1

  async function refresh(): Promise<void> {
    inflight?.abort()
    const controller = new AbortController()
    inflight = controller

    try {
      const response = await fetch(options.url, {
        cache: 'no-store',
        signal: controller.signal,
        headers: { accept: 'application/json' },
      })
      if (!response.ok) {
        // A 404 means the desktop has never published. That is not an error worth
        // shouting about on a blog, so the last known state stays in place.
        error.value = response.status === 404 ? null : `HTTP ${response.status}`
        return
      }

      const body = (await response.json()) as Presence
      if (typeof body?.ts !== 'number' || typeof body?.seq !== 'number') {
        error.value = 'unexpected payload'
        return
      }
      if (body.seq < lastSeq) return

      lastSeq = body.seq
      // Measure the clock offset once from a fresh response, then keep it. Device
      // clocks do not jump often enough to justify re-measuring every poll, and
      // each measurement carries round-trip noise.
      if (!haveSkew) {
        skewMs = estimateSkew(body)
        haveSkew = true
      }

      presence.value = body
      staleness.value = classify(body, Date.now(), skewMs)
      error.value = null
      options.onUpdate?.(body)
    } catch (err) {
      if ((err as Error)?.name === 'AbortError') return
      error.value = (err as Error)?.message ?? 'network error'
    } finally {
      if (inflight === controller) inflight = null
    }
  }

  function schedule() {
    if (timer) clearTimeout(timer)
    timer = setTimeout(async () => {
      await refresh()
      schedule()
    }, interval)
  }

  function start() {
    if (!stopped) return
    stopped = false
    void refresh().then(() => {
      if (!stopped) schedule()
    })
  }

  function stop() {
    stopped = true
    if (timer) clearTimeout(timer)
    timer = null
    inflight?.abort()
    inflight = null
  }

  function onVisibility() {
    if (!pauseWhenHidden || typeof document === 'undefined') return
    if (document.visibilityState === 'visible') start()
    else stop()
  }

  if (typeof document !== 'undefined' && pauseWhenHidden) {
    document.addEventListener('visibilitychange', onVisibility)
  }

  onScopeDispose(() => {
    stop()
    if (typeof document !== 'undefined' && pauseWhenHidden) {
      document.removeEventListener('visibilitychange', onVisibility)
    }
  })

  start()

  return { presence, staleness, error, refresh, start, stop }
}
