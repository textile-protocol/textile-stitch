// The one place that talks to the panel API.
//
// Every call goes through `request`, so the 401-means-sign-in rule and the
// "surface the server's own message" rule are written once. The server's error
// prose is deliberately shown verbatim: it comes from the config writer and the
// TOML validator, and it's more useful than anything the UI could invent.

import type {
  ActionResult,
  Allowances,
  Bot,
  BotVersions,
  CorridorList,
  CreateBotResult,
  Fleet,
  Funding,
  MigrationResult,
  RfqEmailResult,
  RfqStatusResult,
  SaveResult,
  SessionInfo,
  Settings,
  UpdatesStatus,
} from './types'

export class ApiError extends Error {
  readonly status: number

  constructor(status: number, message: string) {
    super(message)
    this.status = status
    this.name = 'ApiError'
  }

  /** True when the right response is to show the login screen. */
  get needsLogin(): boolean {
    return this.status === 401 || this.status === 403
  }
}

/**
 * Paths that answer "not authorized" as a normal outcome rather than as a
 * session that ran out: asking who you are, and getting the password wrong.
 */
const AUTH_PATHS = new Set(['/api/session', '/api/login', '/api/logout'])

let onUnauthorized: (() => void) | null = null

/**
 * Register what to do when the panel stops accepting our credential.
 *
 * A password session lasts 12 hours and then simply expires. Without this the
 * operator sits on a protected page where every button fails with "not
 * authorized" and the login form is unreachable short of a manual reload.
 */
export function setUnauthorizedHandler(fn: (() => void) | null) {
  onUnauthorized = fn
}

/**
 * Fire the registered unauthorized handler.
 *
 * Used by the SSE reader, which talks to `fetch` directly and therefore never
 * goes through `request`. Same rule: a 401/403 on a protected stream means the
 * session is gone and the operator needs the login screen, not an error banner
 * on a page they can no longer act on.
 */
export function notifyUnauthorized() {
  onUnauthorized?.()
}

async function request<T>(
  path: string,
  init?: RequestInit,
  readBody?: (res: Response) => Promise<T>,
): Promise<T> {
  let res: Response
  try {
    res = await fetch(path, {
      ...init,
      headers: {
        ...(init?.body ? { 'Content-Type': 'application/json' } : {}),
        ...init?.headers,
      },
    })
  } catch (e) {
    // A network failure here almost always means the panel process died or the
    // tailnet dropped, which is worth saying plainly.
    throw new ApiError(0, `Couldn't reach the panel: ${(e as Error).message}`)
  }

  if (!res.ok) {
    const error = new ApiError(res.status, await errorMessage(res))
    if (error.needsLogin && !AUTH_PATHS.has(path.split('?')[0] ?? path)) {
      onUnauthorized?.()
    }
    throw error
  }
  if (res.status === 204) {
    return undefined as T
  }
  if (readBody) {
    return readBody(res)
  }
  return (await res.json()) as T
}

/**
 * A `request` that yields the body as text rather than JSON, for the endpoints that
 * serve a file. Shares the 401-means-sign-in and error-message rules — the point of
 * having it at all.
 */
async function requestText(path: string): Promise<string> {
  return request<string>(path, undefined, (res) => res.text())
}

async function errorMessage(res: Response): Promise<string> {
  const body = await res.text().catch(() => '')
  try {
    const parsed = JSON.parse(body) as { error?: string }
    if (parsed.error) return parsed.error
  } catch {
    // Not our JSON envelope. Axum's own rejections (a malformed body, a bad
    // content type) are plain text, and they name the offending field, which is
    // far more use than the status line. A reverse proxy in front of us can also
    // answer with HTML, hence the guard below.
  }
  const text = body.trim()
  if (text && !text.startsWith('<') && text.length <= 500) return text
  return `${res.status} ${res.statusText}`
}

const json = (body: unknown): RequestInit => ({
  method: 'POST',
  body: JSON.stringify(body),
})

export interface FeedMid {
  /** Quote per base as the feed publishes it (USDT per cNGN). */
  price: number
  /** Unix seconds the feed observed it. */
  timestamp: number
}

export interface DesktopSwitches {
  /** False outside the desktop app: nothing to show. */
  available: boolean
  autostart: boolean
  keepAwake: boolean
  /** "Keep Mac awake" / "Keep PC awake", from the app. */
  keepAwakeLabel: string
  /** A change the app has not applied yet; the values above already reflect it. */
  pending: { autostart?: boolean | null; keepAwake?: boolean | null } | null
}

export const api = {
  /** The desktop app's own switches, relayed through the panel. */
  desktop: () => request<DesktopSwitches>('/api/desktop'),
  setDesktop: (body: { autostart?: boolean; keepAwake?: boolean }) =>
    request<DesktopSwitches>('/api/desktop', {
      method: 'PATCH',
      body: JSON.stringify(body),
    }),

  session: () => request<SessionInfo>('/api/session'),
  login: (password: string) => request<SessionInfo>('/api/login', json({ password })),
  logout: () => request<unknown>('/api/logout', { method: 'POST' }),

  /**
   * `signal` is for the callers an operator is waiting on: the wizard's Next
   * asks this and one `settings` per same-chain bot before it can paint, and a
   * wedged Docker daemon can hold `/api/bots` open with no server-side budget
   * behind it. Nothing else passes one.
   */
  fleet: (signal?: AbortSignal) =>
    request<Fleet>('/api/bots', signal ? { signal } : undefined),
  bot: (name: string) => request<Bot>(`/api/bots/${encodeURIComponent(name)}`),
  /** Set (or, with '', clear) the display name. The id never changes. */
  rename: (name: string, displayName: string) =>
    request<Bot>(`/api/bots/${encodeURIComponent(name)}/name`, {
      method: 'PATCH',
      body: JSON.stringify({ displayName }),
    }),
  corridors: () => request<CorridorList>('/api/corridors'),

  /**
   * The mid a price feed is publishing now, fetched by the panel with the
   * bot's own feed adapter (the browser can't: Textile's /price only answers
   * cross-origin for the app). The Spread step's worked example runs on it.
   */
  feedMid: (url: string, signal?: AbortSignal) =>
    request<FeedMid>(
      `/api/feed/mid?url=${encodeURIComponent(url)}`,
      signal ? { signal } : undefined,
    ),

  createBot: (body: unknown) =>
    request<CreateBotResult>('/api/bots', json(body)),

  /**
   * Mint a fresh hot wallet for the Create wallet step. Returns address + seed
   * phrase once — nothing is stored until the client posts the phrase back on
   * create.
   */
  generateWallet: () =>
    request<{ address: string; seedPhrase: string }>(
      '/api/wallets/generate',
      json({}),
    ),

  /**
   * Dry-run: which other bots already use this signer on this chain. Used to warn
   * before create — sharing a wallet races nonces.
   */
  checkSigner: (body: {
    chainId: number
    signer: unknown
    excludeBot?: string
  }) =>
    request<{
      operatorAddress: string
      chainId: number
      conflicts: {
        name: string
        chainId: number | null
        operatorAddress: string | null
        /** Live with taker/closer on — Start will refuse. */
        blocksLiveSwitch: boolean
      }[]
    }>('/api/signer/check', json(body)),

  start: (name: string) => act(name, 'start'),
  stop: (name: string) => act(name, 'stop'),
  restart: (name: string) => act(name, 'restart'),
  recreate: (name: string) => act(name, 'recreate'),
  /** Pull the panel bot image and recreate this bot on it. */
  updateBot: (name: string) => act(name, 'update'),

  /** The published builds this bot could be rolled back to, newest first. */
  botVersions: (name: string) =>
    request<BotVersions>(`/api/bots/${encodeURIComponent(name)}/versions`),

  /**
   * Recreate this bot on an earlier published build. `tag` names one exact
   * version (`sha-…`); the repository is the panel's own, so a client picks a
   * version and never an image.
   */
  rollback: (name: string, tag: string) =>
    request<ActionResult>(
      `/api/bots/${encodeURIComponent(name)}/rollback`,
      json({ tag }),
    ),

  // `acceptLedgerLoss` is for the second attempt only. The first rolls back when
  // the old container's nonce ledger can't be read, so a transient daemon error
  // costs a retry instead of the nonces for live orders.
  migrate: (name: string, acceptLedgerLoss = false) =>
    request<MigrationResult>(
      `/api/bots/${encodeURIComponent(name)}/migrate?acceptLedgerLoss=${acceptLedgerLoss}`,
      { method: 'POST' },
    ),

  remove: (name: string, deleteConfig: boolean) =>
    request<{ message: string }>(
      `/api/bots/${encodeURIComponent(name)}?deleteConfig=${deleteConfig}`,
      { method: 'DELETE' },
    ),

  settings: (name: string, pool: number, signal?: AbortSignal) =>
    request<Settings>(
      `/api/bots/${encodeURIComponent(name)}/settings?pool=${pool}`,
      signal ? { signal } : undefined,
    ),

  saveSettings: (name: string, patch: unknown) =>
    request<SaveResult>(`/api/bots/${encodeURIComponent(name)}/settings`, {
      method: 'PATCH',
      body: JSON.stringify(patch),
    }),

  /** Per-token Permit2 allowance status across every corridor this bot quotes. */
  allowances: (name: string) =>
    request<Allowances>(
      `/api/bots/${encodeURIComponent(name)}/allowances`,
    ),

  /**
   * Wallet balances, dollar values, gas and Permit2 state in one read, plus the
   * "may it start?" gate. Chain and price failures come back as fields, so a
   * caller that polls keeps rendering.
   */
  funding: (name: string) =>
    request<Funding>(`/api/bots/${encodeURIComponent(name)}/funding`),

  /**
   * URL of the approve one-shot, for `streamSse(url, { method: 'POST' }, …)`.
   * A URL rather than a request because the route streams output.
   */
  approveUrl: (name: string) => `/api/bots/${encodeURIComponent(name)}/approve`,
  /** URL of the withdraw one-shot; same streaming shape as approve. */
  withdrawUrl: (name: string) => `/api/bots/${encodeURIComponent(name)}/withdraw`,

  enrollRfq: (name: string) =>
    request<SaveResult>(`/api/bots/${encodeURIComponent(name)}/rfq/enroll`, {
      method: 'POST',
      body: JSON.stringify({}),
    }),

  /** Hand Textile the operator's address; they mail back a confirm link. */
  verifyRfqEmail: (name: string, body: { contactEmail: string }) =>
    request<RfqEmailResult>(
      `/api/bots/${encodeURIComponent(name)}/rfq/verify-email`,
      { method: 'POST', body: JSON.stringify(body) },
    ),

  checkRfqStatus: (name: string) =>
    request<RfqStatusResult>(
      `/api/bots/${encodeURIComponent(name)}/rfq/status`,
      { method: 'POST', body: JSON.stringify({}) },
    ),

  rawConfig: (name: string) =>
    request<{ toml: string; path: string; editable: boolean }>(
      `/api/bots/${encodeURIComponent(name)}/config`,
    ),

  saveRawConfig: (name: string, toml: string) =>
    request<SaveResult>(`/api/bots/${encodeURIComponent(name)}/config`, {
      method: 'PUT',
      body: JSON.stringify({ toml }),
    }),



  /** Append a same-chain catalog corridor as another [[pools]] entry. */
  addPool: (name: string, corridorId: string) =>
    request<SaveResult>(`/api/bots/${encodeURIComponent(name)}/pools`, {
      method: 'POST',
      body: JSON.stringify({ corridorId }),
    }),

  /** Drop one [[pools]] entry. The bot must keep at least one. */
  // The pair is not redundant with the index: an index only means anything
  // against the list this client rendered, and a concurrent removal renumbers
  // it. The panel refuses when the indexed pool is no longer that pair.
  removePool: (name: string, index: number, collateral: string, debt: string) =>
    request<SaveResult>(
      `/api/bots/${encodeURIComponent(name)}/pools/${index}?collateral=${encodeURIComponent(collateral)}&debt=${encodeURIComponent(debt)}`,
      { method: 'DELETE' },
    ),

  /** Registry digest check: which bots (and the panel) are behind. */
  updates: (refresh = false) =>
    request<UpdatesStatus>(`/api/updates${refresh ? '?refresh=1' : ''}`),

  /**
   * Pull a newer panel image and schedule a self-recreate. Returns 202; the UI
   * should poll `/api/session` until the panel is back.
   */
  updatePanel: () =>
    request<{ message: string; targetImage: string }>('/api/panel/update', {
      method: 'POST',
    }),

  /** URL of the SSE log stream, for `EventSource`. */
  logsUrl: (name: string, tail = 500) =>
    `/api/bots/${encodeURIComponent(name)}/logs?tail=${tail}&follow=true`,

  /**
   * Fetch the compose export through the authenticated request path.
   *
   * Not a plain `<a href>`: that's a navigation, so in password mode an expired
   * session replaces the whole SPA with the endpoint's 401 JSON and the operator
   * never reaches the login form. Going through `request` means the unauthorized
   * handler fires like it does everywhere else, and the download is built locally
   * from the text.
   */
  composeExport: () => requestText('/api/compose-export'),
}

function act(name: string, what: string): Promise<ActionResult> {
  return request<ActionResult>(`/api/bots/${encodeURIComponent(name)}/${what}`, {
    method: 'POST',
  })
}
