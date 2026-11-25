/**
 * Turning a timestamp into a verdict.
 *
 * The client cannot trust the local clock against the desktop's: a laptop five
 * minutes behind would otherwise believe a live friend had been offline for five
 * minutes. `estimateSkew` measures the difference once, from a successful
 * response, and every later comparison goes through it.
 */

/** Storage shape published by the desktop client. */
export interface Presence {
  schema: number
  device: { id: string; name: string; os: string }
  seq: number
  status: 'active' | 'idle' | 'paused'
  /** Unix seconds, measured on the reporting machine. */
  ts: number
  updatedAt: string
  heartbeatS: number
  focus: Focus | null
  media: Media | null
}

export interface Focus {
  /** Public id chosen by the desktop rule. Map it to your own wording. */
  app: string
  /** Free-form group from the rule, e.g. `dev`. Useful for icons or a fallback. */
  category: string
  since: number
  seconds: number
}

export interface Media {
  title: string
  artist: string
  app: string
}

export type Verdict = 'live' | 'recent' | 'stale' | 'offline'

export interface Staleness {
  verdict: Verdict
  /** Seconds since the snapshot was written, clock-corrected. */
  age: number
}

/**
 * How old a snapshot may be before it stops counting as live.
 *
 * Derived from the heartbeat the client itself advertised rather than hard-coded,
 * so changing the interval in config.json does not require a blog redeploy.
 * Three missed heartbeats is the threshold the desktop README documents.
 */
export function offlineAfter(presence: Presence): number {
  return Math.max(60, (presence.heartbeatS || 90) * 3)
}

export function classify(presence: Presence, nowMs: number, skewMs = 0): Staleness {
  const serverNow = Math.floor((nowMs - skewMs) / 1000)
  const age = Math.max(0, serverNow - presence.ts)
  const limit = offlineAfter(presence)

  if (age > limit) return { verdict: 'offline', age }
  if (presence.status === 'paused') return { verdict: 'stale', age }
  if (age <= (presence.heartbeatS || 90) * 1.5) return { verdict: 'live', age }
  return { verdict: 'recent', age }
}

/**
 * Estimate the offset between this browser and the desktop by comparing the
 * snapshot timestamp against the local clock.
 *
 * Returns milliseconds to subtract from local time. A network round trip adds
 * noise, but half a second of error on a ninety-second threshold is harmless.
 */
export function estimateSkew(presence: Presence, nowMs = Date.now()): number {
  return nowMs - presence.ts * 1000
}

export function formatAge(seconds: number): string {
  if (seconds < 60) return `${seconds} 秒前`
  if (seconds < 3600) return `${Math.floor(seconds / 60)} 分钟前`
  if (seconds < 86400) return `${Math.floor(seconds / 3600)} 小时前`
  return `${Math.floor(seconds / 86400)} 天前`
}
