<script setup lang="ts">
import { computed } from 'vue'
import { formatAge, usePresence } from '@now-doing/presence-client'
import { zh, type Phrases } from './messages'

/**
 * The payload carries no human-readable text on purpose: the same `app` id has to
 * read differently in every language and on every blog, so the wording lives here.
 */
const props = withDefaults(
  defineProps<{
    /** Public URL of presence.json. */
    url: string
    /** Poll interval in seconds. */
    interval?: number
    /** Hide the whole card when the machine is offline. */
    hideWhenOffline?: boolean
    /** Wording for this locale; pass `en` or your own table. */
    phrases?: Phrases
  }>(),
  { interval: 15, hideWhenOffline: false, phrases: () => zh },
)

const { presence, staleness } = usePresence({
  url: props.url,
  interval: props.interval * 1000,
})

const visible = computed(() => {
  if (!presence.value || !staleness.value) return false
  return staleness.value.verdict !== 'offline' || !props.hideWhenOffline
})

const online = computed(() => staleness.value?.verdict !== 'offline')

const headline = computed(() => {
  const p = presence.value
  if (!p) return ''

  const t = props.phrases
  if (staleness.value?.verdict === 'offline') return t.offline

  if (p.status === 'paused') return t.paused
  if (p.status === 'idle') return t.idle

  const focus = p.focus
  if (!focus) return t.generic
  return t.apps[focus.app] ?? t.categories?.[focus.category] ?? t.generic
})

const detail = computed(() => {
  const m = presence.value?.media
  if (!m?.title) return ''
  const by = m.artist ? ` — ${m.artist}` : ''
  return `在听 ${m.title}${by}`
})

const age = computed(() =>
  staleness.value ? formatAge(staleness.value.age) : '',
)
</script>

<template>
  <aside v-if="visible" class="presence-card" :class="{ offline: !online }">
    <span class="dot" :class="{ live: online }" aria-hidden="true" />
    <div class="body">
      <p class="headline">{{ headline }}</p>
      <p v-if="detail" class="detail">{{ detail }}</p>
    </div>
    <p v-if="age" class="age">{{ age }}</p>
  </aside>
</template>

<style scoped>
.presence-card {
  display: flex;
  align-items: center;
  gap: 0.65rem;
  padding: 0.7rem 0.9rem;
  border-radius: 0.75rem;
  border: 1px solid var(--presence-border, rgb(0 0 0 / 8%));
  background: var(--presence-bg, rgb(0 0 0 / 2%));
  font-size: 0.875rem;
  line-height: 1.4;
}

.presence-card.offline {
  opacity: 0.6;
}

.dot {
  flex: none;
  width: 0.5rem;
  height: 0.5rem;
  border-radius: 999px;
  background: var(--presence-idle, #9ca3af);
}

.dot.live {
  background: var(--presence-live, #10b981);
  box-shadow: 0 0 0 0 var(--presence-live, #10b981);
  animation: presence-pulse 2.4s ease-out infinite;
}

@keyframes presence-pulse {
  0% { box-shadow: 0 0 0 0 rgb(16 185 129 / 45%); }
  70% { box-shadow: 0 0 0 6px rgb(16 185 129 / 0%); }
  100% { box-shadow: 0 0 0 0 rgb(16 185 129 / 0%); }
}

@media (prefers-reduced-motion: reduce) {
  .dot.live { animation: none; }
}

.body {
  min-width: 0;
  flex: 1;
}

.headline {
  margin: 0;
  font-weight: 550;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.detail {
  margin: 0.1rem 0 0;
  color: var(--presence-muted, #6b7280);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.age {
  flex: none;
  margin: 0;
  font-size: 0.75rem;
  color: var(--presence-muted, #6b7280);
  font-variant-numeric: tabular-nums;
}
</style>
