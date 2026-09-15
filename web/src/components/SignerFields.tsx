import { useEffect, useRef, useState } from 'react'
import { ApiError, api } from '../api'
import { Banner, Button, Field, Input, Select, TextArea } from './ui'

export type SignerKind = 'local' | 'turnkey' | 'mpcvault' | 'fireblocks'
/** How the local hot wallet is provided. Create is the default. */
export type LocalMode = 'create' | 'import'
export type KeyForm = 'privateKey' | 'seedPhrase'

/** The signer state a form owns, so both the wizard and Change signer share one shape. */
export interface SignerState {
  kind: SignerKind
  localMode: LocalMode
  keyForm: KeyForm
  secret: string
  /** Address for a just-generated create-wallet (display only). */
  createdAddress: string
  /** Operator confirmed they saved the generated seed phrase. */
  backedUp: boolean
  mpc: Record<string, string>
}

export const emptySigner: SignerState = {
  kind: 'local',
  localMode: 'create',
  keyForm: 'seedPhrase',
  secret: '',
  createdAddress: '',
  backedUp: false,
  mpc: {},
}

/**
 * The signer picker plus the fields the chosen backend needs. Controlled: the parent
 * owns the state so it can clear the secret the moment it's sent. Shared by the add-bot
 * wizard and the Change signer flow, so the two can't drift on field names.
 *
 * Local hot wallet defaults to **Create wallet** (server generates a BIP-39 phrase).
 * **Import wallet** is the paste-in path for an existing private key or seed phrase.
 */
export function SignerFields({
  value,
  onChange,
}: {
  value: SignerState
  onChange: (v: SignerState) => void
}) {
  const set = (patch: Partial<SignerState>) => onChange({ ...value, ...patch })
  return (
    <>
      <Field label="Signer">
        <Select
          value={value.kind}
          onChange={(e) =>
            set({
              kind: e.target.value as SignerKind,
              // Leaving local: drop any secret sitting in memory.
              ...(e.target.value !== 'local'
                ? {
                    secret: '',
                    createdAddress: '',
                    backedUp: false,
                  }
                : {}),
            })
          }
        >
          <option value="local">Hot wallet (local)</option>
          <option value="turnkey">MPC — Turnkey</option>
          <option value="mpcvault">MPC — MPCVault · Experimental</option>
          <option value="fireblocks">MPC — Fireblocks · Experimental</option>
        </Select>
      </Field>

      {value.kind === 'local' && (
        <LocalWalletFields value={value} onChange={onChange} />
      )}

      {value.kind === 'turnkey' && (
        <MpcFields
          values={value.mpc}
          onChange={(mpc) => set({ mpc })}
          fields={[
            ['organizationId', 'Organization ID', false],
            ['signWith', 'Sign with (wallet or private key id)', false],
            ['operatorAddress', 'Operator address', false],
            ['apiBaseUrl', 'API base URL (optional)', false],
            ['apiPublicKey', 'API public key', false],
            ['apiPrivateKey', 'API private key', true],
          ]}
        />
      )}

      {value.kind === 'fireblocks' && (
        <FireblocksFields
          values={value.mpc}
          onChange={(mpc) => set({ mpc })}
        />
      )}

      {value.kind === 'mpcvault' && (
        <MpcFields
          values={value.mpc}
          onChange={(mpc) => set({ mpc })}
          fields={[
            ['vaultUuid', 'Vault UUID', false],
            ['clientSignerPubkey', 'Client-signer public key', false],
            ['operatorAddress', 'Operator address', false],
            ['apiBaseUrl', 'API base URL (optional)', false],
            ['callbackListenAddr', 'Callback listen address (optional)', false],
            ['apiToken', 'API token', true],
          ]}
        />
      )}
    </>
  )
}

function LocalWalletFields({
  value,
  onChange,
}: {
  value: SignerState
  onChange: (v: SignerState) => void
}) {
  const set = (patch: Partial<SignerState>) => onChange({ ...value, ...patch })

  return (
    <div className="space-y-3">
      <div
        className="flex rounded-lg border border-line-soft p-0.5"
        role="tablist"
        aria-label="Wallet setup"
      >
        {(
          [
            ['create', 'Create wallet'],
            ['import', 'Import wallet'],
          ] as const
        ).map(([mode, label]) => {
          const active = value.localMode === mode
          return (
            <button
              key={mode}
              type="button"
              role="tab"
              aria-selected={active}
              className={`flex-1 rounded-md px-3 py-1.5 text-sm font-bold transition ${
                active
                  ? 'bg-accent text-on-accent'
                  : 'text-muted hover:bg-hover'
              }`}
              onClick={() => {
                if (mode === value.localMode) return
                set({
                  localMode: mode,
                  secret: '',
                  createdAddress: '',
                  backedUp: false,
                  keyForm: mode === 'create' ? 'seedPhrase' : 'privateKey',
                })
              }}
            >
              {label}
            </button>
          )
        })}
      </div>

      {value.localMode === 'create' ? (
        <CreateWalletPanel value={value} onChange={onChange} />
      ) : (
        <ImportWalletPanel value={value} onChange={onChange} />
      )}
    </div>
  )
}

function CreateWalletPanel({
  value,
  onChange,
}: {
  value: SignerState
  onChange: (v: SignerState) => void
}) {
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [copied, setCopied] = useState(false)
  const [revealed, setRevealed] = useState(false)
  // Prevent auto-retry loops when generation fails.
  const [autoTried, setAutoTried] = useState(false)
  // Drop in-flight generate results after unmount or a newer generate().
  const aliveRef = useRef(true)
  const genIdRef = useRef(0)

  useEffect(() => {
    aliveRef.current = true
    return () => {
      aliveRef.current = false
    }
  }, [])

  async function generate() {
    const genId = ++genIdRef.current
    setBusy(true)
    setError(null)
    setCopied(false)
    setRevealed(false)
    try {
      const wallet = await api.generateWallet()
      // Operator may have switched to Import / MPC while this was in flight —
      // never apply a stale create response on top of that.
      if (!aliveRef.current || genId !== genIdRef.current) return
      onChange({
        kind: 'local',
        localMode: 'create',
        keyForm: 'seedPhrase',
        secret: wallet.seedPhrase,
        createdAddress: wallet.address,
        backedUp: false,
        mpc: {},
      })
    } catch (e) {
      if (!aliveRef.current || genId !== genIdRef.current) return
      setError(e instanceof ApiError ? e.message : String(e))
    } finally {
      if (aliveRef.current && genId === genIdRef.current) {
        setBusy(false)
      }
    }
  }

  // First visit (or after switching back to Create): mint without an extra click.
  useEffect(() => {
    if (value.localMode !== 'create') {
      setAutoTried(false)
      return
    }
    if (value.secret.trim() || value.createdAddress || busy || autoTried) return
    setAutoTried(true)
    void generate()
    // Intentionally omit generate: we only want to auto-mint once when empty.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [value.localMode, value.secret, value.createdAddress, busy, autoTried])

  async function copyPhrase() {
    try {
      await navigator.clipboard.writeText(value.secret)
      setCopied(true)
      window.setTimeout(() => setCopied(false), 2000)
    } catch {
      setError("Couldn't copy — select the phrase and copy it manually.")
    }
  }

  function downloadPhrase() {
    const blob = new Blob(
      [
        `Stitch operator wallet\nAddress: ${value.createdAddress}\nSeed phrase:\n${value.secret}\n`,
      ],
      { type: 'text/plain' },
    )
    const url = URL.createObjectURL(blob)
    const a = document.createElement('a')
    a.href = url
    a.download = `stitch-wallet-${value.createdAddress.slice(0, 10)}.txt`
    a.click()
    URL.revokeObjectURL(url)
  }

  return (
    <div className="space-y-3">
      <p className="text-sm text-muted">
        We generate a new wallet for this bot. Save the seed phrase now — the panel
        never shows it again, and only the derived key is written to an owner-only
        file on the host.
      </p>

      {error && (
        <div className="space-y-2">
          <Banner tone="danger">{error}</Banner>
          <Button type="button" busy={busy} onClick={() => void generate()}>
            Try again
          </Button>
        </div>
      )}

      {(busy || (!value.createdAddress && !error)) && (
        <p className="text-sm text-muted">Generating a secure wallet…</p>
      )}

      {value.createdAddress && (
        <>
          <Field label="Address">
            <code className="block break-all rounded-lg border border-line-soft bg-canvas px-3 py-2 font-mono text-xs">
              {value.createdAddress}
            </code>
          </Field>

          <Field
            label="Seed phrase"
            hint="Twelve words. Anyone with this phrase controls the wallet — treat it like cash."
          >
            <div className="space-y-2">
              <div
                className={`rounded-lg border border-line bg-canvas px-3 py-3 font-mono text-sm leading-relaxed ${
                  revealed ? '' : 'select-none blur-sm'
                }`}
              >
                {value.secret}
              </div>
              <div className="flex flex-wrap gap-2">
                <Button type="button" onClick={() => setRevealed((r) => !r)}>
                  {revealed ? 'Hide' : 'Reveal'}
                </Button>
                <Button type="button" onClick={() => void copyPhrase()} disabled={!revealed}>
                  {copied ? 'Copied' : 'Copy'}
                </Button>
                <Button type="button" onClick={downloadPhrase} disabled={!revealed}>
                  Download
                </Button>
                <Button
                  type="button"
                  busy={busy}
                  onClick={() => {
                    setAutoTried(true)
                    void generate()
                  }}
                >
                  Generate another
                </Button>
              </div>
            </div>
          </Field>

          <label className="flex cursor-pointer items-start gap-2 text-sm">
            <input
              type="checkbox"
              className="mt-0.5 accent-[var(--tx-accent)]"
              checked={value.backedUp}
              onChange={(e) => onChange({ ...value, backedUp: e.target.checked })}
            />
            <span>
              I saved this seed phrase somewhere safe. I understand Stitch cannot
              recover it.
            </span>
          </label>
        </>
      )}
    </div>
  )
}

function ImportWalletPanel({
  value,
  onChange,
}: {
  value: SignerState
  onChange: (v: SignerState) => void
}) {
  const [revealed, setRevealed] = useState(false)
  const set = (patch: Partial<SignerState>) => onChange({ ...value, ...patch })

  return (
    <div className="space-y-3">
      <Banner tone="warning">
        Import only on a machine you trust. The key is written to an owner-only
        file on this host and never read back by the panel — but it is not
        encrypted at rest. Prefer Create wallet unless you already hold the key.
      </Banner>

      <Field label="What are you importing?">
        <Select
          value={value.keyForm}
          onChange={(e) =>
            set({
              keyForm: e.target.value as KeyForm,
              secret: '',
            })
          }
        >
          <option value="privateKey">Private key</option>
          <option value="seedPhrase">Seed phrase</option>
        </Select>
      </Field>

      <Field
        label={value.keyForm === 'privateKey' ? 'Private key' : 'Seed phrase'}
        hint="Paste once. Cleared from this form as soon as the bot is created or the signer is switched."
      >
        <div className="relative">
          <Input
            type={revealed ? 'text' : 'password'}
            value={value.secret}
            autoComplete="off"
            autoCorrect="off"
            autoCapitalize="off"
            spellCheck={false}
            name="stitch-wallet-secret"
            data-1p-ignore
            data-lpignore="true"
            data-form-type="other"
            className="pr-20 font-mono text-sm tracking-wide"
            placeholder={
              value.keyForm === 'privateKey' ? '0x…' : 'twelve words…'
            }
            onChange={(e) => set({ secret: e.target.value })}
            onPaste={() => setRevealed(false)}
          />
          <button
            type="button"
            className="absolute right-2 top-1/2 -translate-y-1/2 rounded px-2 py-0.5 text-xs font-bold text-muted hover:bg-hover hover:text-ink"
            onClick={() => setRevealed((r) => !r)}
          >
            {revealed ? 'Hide' : 'Show'}
          </button>
        </div>
      </Field>
    </div>
  )
}

/**
 * Above this, one signature eats so much of the venue's ~750ms reply budget
 * that quoting stops being reliable. Not a hard limit — the bot still runs — but
 * the operator should see it at setup rather than infer it from missed quotes.
 */
const SLOW_SIGN_MS = 400

/**
 * Where the operator's workspace answers.
 *
 * A Fireblocks workspace is provisioned into one region and answers on that host
 * alone, so this is not a preference — an EU workspace simply is not reachable at
 * `api.fireblocks.io`. A picker rather than a text field because the value has to
 * match the backend's allowlist exactly, and there is nothing to gain from
 * letting it be mistyped.
 *
 * The chosen URL is used for discovery *and* written into `stitch.toml`, so the
 * bot signs against the same host the panel verified against.
 */
const FIREBLOCKS_DEFAULT_API = 'https://api.fireblocks.io'

const FIREBLOCKS_REGIONS: [label: string, url: string][] = [
  ['Global (default)', FIREBLOCKS_DEFAULT_API],
  ['EU', 'https://eu-api.fireblocks.io'],
  ['EU2', 'https://eu2-api.fireblocks.io'],
  ['US East', 'https://us-east-1-api.fireblocks.io'],
  ['Sandbox', 'https://sandbox-api.fireblocks.io'],
]

/**
 * Fireblocks needs two credentials and nothing else typed.
 *
 * The vault account comes from a dropdown the panel populates, and the operator
 * address is *never* entered: Verify signs a throwaway message and fills in the
 * address that signature recovered to. That ordering is deliberate — a
 * hand-copied address that doesn't match the vault produces signed orders that
 * fail to settle, and the operator finds out days later.
 *
 * Verify also reports the round-trip latency, which for this backend is the
 * thing most likely to make it unsuitable: signing is a create-then-poll cycle
 * and the venue stops listening for a quote after ~750ms.
 */
function FireblocksFields({
  values,
  onChange,
}: {
  values: Record<string, string>
  onChange: (v: Record<string, string>) => void
}) {
  const [vaults, setVaults] = useState<{ id: string; name: string }[] | null>(null)
  const [busy, setBusy] = useState<'vaults' | 'verify' | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [latencyMs, setLatencyMs] = useState<number | null>(null)
  const alive = useRef(true)
  /**
   * Which set of inputs the in-flight request belongs to.
   *
   * Verify is a real signing round trip and can take seconds. If the operator
   * edits the key, the PEM or the vault while it is out, `invalidate` clears the
   * proof — but the old request is still coming, and writing its address back
   * would leave `isSignerComplete` satisfied by an address proven for inputs
   * that are no longer in the form. That bot would quote orders signed by a
   * wallet it doesn't control. Same guard the Create wallet panel above uses.
   */
  const genRef = useRef(0)

  useEffect(() => {
    alive.current = true
    return () => {
      alive.current = false
    }
  }, [])

  const apiKey = (values.apiKey ?? '').trim()
  const apiPrivateKey = values.apiPrivateKey ?? ''
  const haveCredentials = apiKey.length > 0 && apiPrivateKey.trim().length > 0
  const apiBaseUrl = (values.apiBaseUrl ?? '').trim() || FIREBLOCKS_DEFAULT_API

  /** Patch the form, dropping any proof that belonged to the old values. */
  const set = (patch: Record<string, string>) => {
    onChange({ ...values, ...patch })
  }
  /**
   * Retire everything the old inputs proved.
   *
   * Bumping the generation makes any in-flight response stale, so it will skip
   * its own `finally` — which is why `busy` is cleared here rather than there.
   * `credentialsChanged` also drops the vault list, since a different key or
   * region is a different workspace.
   */
  const invalidate = (
    patch: Record<string, string>,
    { credentialsChanged = false } = {},
  ) => {
    genRef.current += 1
    setLatencyMs(null)
    setBusy(null)
    if (credentialsChanged) setVaults(null)
    // An address proven for one vault says nothing about another.
    set({ ...patch, operatorAddress: '' })
  }

  /**
   * Run one request, applying its result only if the inputs it belongs to are
   * still the ones in the form.
   *
   * A response that arrives after the operator edited the key, PEM or vault
   * describes inputs that no longer exist. Writing it back would leave
   * `isSignerComplete` satisfied by an address proven for the wrong
   * credentials, and that bot would quote orders signed by a wallet it doesn't
   * control — so a stale result is dropped silently rather than applied.
   */
  async function guarded<T>(
    kind: 'vaults' | 'verify',
    call: () => Promise<T>,
    ok: (result: T) => void,
    fail: () => void,
  ) {
    const gen = ++genRef.current
    const stale = () => !alive.current || genRef.current !== gen
    setBusy(kind)
    setError(null)
    try {
      const result = await call()
      if (stale()) return
      ok(result)
    } catch (e) {
      if (stale()) return
      fail()
      setError(e instanceof ApiError ? e.message : String(e))
    } finally {
      if (!stale()) setBusy(null)
    }
  }

  const loadVaults = () =>
    guarded(
      'vaults',
      () => api.fireblocksVaults({ apiKey, apiPrivateKey, apiBaseUrl }),
      (res) => {
        setVaults(res.vaults)
        if (res.vaults.length === 0) {
          setError('That workspace has no vault accounts. Create one in the Fireblocks console.')
        }
      },
      () => setVaults(null),
    )

  const verify = () =>
    guarded(
      'verify',
      () =>
        api.fireblocksVerify({
          apiKey,
          apiPrivateKey,
          apiBaseUrl,
          vaultAccountId: (values.vaultAccountId ?? '').trim(),
        }),
      (res) => {
        setLatencyMs(res.latencyMs)
        set({ operatorAddress: res.address })
      },
      () => {
        setLatencyMs(null)
        set({ operatorAddress: '' })
      },
    )

  return (
    <div className="space-y-3">
      <Field label="Workspace region">
        <Select
          value={apiBaseUrl}
          onChange={(e) =>
            invalidate({ apiBaseUrl: e.target.value }, { credentialsChanged: true })
          }
        >
          {FIREBLOCKS_REGIONS.map(([label, url]) => (
            <option key={url} value={url}>
              {label}
            </option>
          ))}
        </Select>
      </Field>

      <Field label="API key">
        <Input
          type="text"
          autoComplete="off"
          spellCheck={false}
          placeholder="00000000-0000-0000-0000-000000000000"
          value={values.apiKey ?? ''}
          onChange={(e) =>
            invalidate({ apiKey: e.target.value }, { credentialsChanged: true })
          }
        />
      </Field>

      <Field label="API private key (fireblocks_secret.key)">
        <TextArea
          rows={4}
          autoComplete="off"
          spellCheck={false}
          placeholder="-----BEGIN PRIVATE KEY-----"
          value={apiPrivateKey}
          onChange={(e) =>
            invalidate({ apiPrivateKey: e.target.value }, { credentialsChanged: true })
          }
        />
      </Field>

      <Button
        type="button"
        onClick={loadVaults}
        disabled={!haveCredentials || busy !== null}
      >
        {busy === 'vaults' ? 'Loading vaults…' : 'Load vault accounts'}
      </Button>

      {vaults && vaults.length > 0 && (
        <Field label="Vault account">
          <Select
            value={values.vaultAccountId ?? ''}
            onChange={(e) => invalidate({ vaultAccountId: e.target.value })}
          >
            <option value="">Pick a vault account…</option>
            {vaults.map((v) => (
              <option key={v.id} value={v.id}>
                {v.name ? `${v.name} (${v.id})` : v.id}
              </option>
            ))}
          </Select>
        </Field>
      )}

      {(values.vaultAccountId ?? '').trim().length > 0 && (
        <Button
          type="button"
          onClick={verify}
          disabled={busy !== null}
        >
          {busy === 'verify' ? 'Signing a test message…' : 'Verify'}
        </Button>
      )}

      {values.operatorAddress && (
        <Banner tone={latencyMs !== null && latencyMs > SLOW_SIGN_MS ? 'warning' : 'success'}>
          <div>
            Signed as <code>{values.operatorAddress}</code>.
          </div>
          {latencyMs !== null && (
            <div>
              One signature took {latencyMs} ms.{' '}
              {latencyMs > SLOW_SIGN_MS
                ? 'That is too slow to quote reliably — the venue stops listening about 750 ms ' +
                  'after it asks, and this bot still has to price and respond inside that. ' +
                  'Check whether your Transaction Authorization Policy auto-approves signing ' +
                  'rather than waiting on a human.'
                : 'That fits inside the venue reply budget.'}
            </div>
          )}
        </Banner>
      )}

      {error && <Banner tone="danger">{error}</Banner>}
    </div>
  )
}

function MpcFields({
  values,
  onChange,
  fields,
}: {
  values: Record<string, string>
  onChange: (v: Record<string, string>) => void
  fields: [key: string, label: string, secret: boolean][]
}) {
  return (
    <div className="space-y-3">
      {fields.map(([key, label, isSecret]) => (
        <Field key={key} label={label}>
          <Input
            type={isSecret ? 'password' : 'text'}
            autoComplete="off"
            spellCheck={false}
            value={values[key] ?? ''}
            onChange={(e) => onChange({ ...values, [key]: e.target.value })}
          />
        </Field>
      ))}
    </div>
  )
}

/** Only the fields the chosen backend actually needs are required. */
export function isSignerComplete(s: SignerState): boolean {
  const filled = (k: string) => (s.mpc[k] ?? '').trim().length > 0
  switch (s.kind) {
    case 'local':
      if (s.secret.trim().length === 0) return false
      if (s.localMode === 'create') return s.backedUp && s.createdAddress.length > 0
      return true
    case 'turnkey':
      return ['organizationId', 'signWith', 'operatorAddress', 'apiPublicKey', 'apiPrivateKey'].every(
        filled,
      )
    case 'mpcvault':
      return ['vaultUuid', 'clientSignerPubkey', 'operatorAddress', 'apiToken'].every(filled)
    case 'fireblocks':
      // `operatorAddress` is never typed — it only gets set by a successful
      // Verify, so requiring it here is what makes Verify mandatory rather than
      // advisory. A bot whose address was guessed wrong quotes orders no one
      // can fill.
      return ['apiKey', 'apiPrivateKey', 'vaultAccountId', 'operatorAddress'].every(filled)
  }
}

/** The `kind`-tagged request body the API expects. */
export function buildSigner(s: SignerState): unknown {
  if (s.kind === 'local') {
    return { kind: s.kind, [s.keyForm]: s.secret.trim() }
  }
  return { kind: s.kind, ...s.mpc }
}
