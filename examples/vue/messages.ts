/**
 * Wording tables, keyed by the `app` id your desktop rules publish.
 *
 * The snapshot itself never carries a sentence, so adding a language is a matter
 * of adding a table here — no desktop redeploy, no protocol change.
 */
export interface Phrases {
  offline: string
  paused: string
  idle: string
  generic: string
  /** Per-app wording, keyed by the `app` id from your desktop rules. */
  apps: Record<string, string>
  /** Optional fallback per `category`, for apps you have not named yet. */
  categories?: Record<string, string>
}

export const zh: Phrases = {
  offline: '不在电脑前',
  paused: '暂时不公开状态',
  idle: '暂时离开',
  generic: '在忙别的事',
  apps: {
    code: '正在写代码',
    'deepseek-harness': '在和你聊天',
    cloudmusic: '在听歌',
    msedge: '在上网',
    chrome: '在上网',
  },
  categories: {
    dev: '在写东西',
    browse: '在上网',
    chat: '在聊天',
  },
}

export const en: Phrases = {
  offline: 'Away from the keyboard',
  paused: 'Not sharing right now',
  idle: 'Stepped away',
  generic: 'Busy with something',
  apps: {
    code: 'Writing code',
    'deepseek-harness': 'Chatting with an assistant',
    cloudmusic: 'Listening to music',
    msedge: 'Browsing the web',
    chrome: 'Browsing the web',
  },
  categories: {
    dev: 'Building something',
    browse: 'Browsing the web',
    chat: 'Chatting',
  },
}
