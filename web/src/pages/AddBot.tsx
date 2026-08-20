import { useEffect, useState } from 'react'
import { useNavigate } from 'react-router-dom'
import { ApiError, api } from '../api'
import {
  Banner,
  Button,
  Card,
  ErrorState,
  Field,
  Input,
  Loading,
  Toggle,
} from '../components/ui'
import {
  SignerFields,
  buildSigner,
  emptySigner,
  isSignerComplete,
  type SignerState,
} from '../components/SignerFields'
import SignerConflictWarning from '../components/SignerConflictWarning'
import type { Corridor } from '../types'

/** Sentinel corridor id for the "enter your own" option in the picker. */
const CUSTOM = '__custom__'

/** The custom-corridor form, all fields as strings until submit. */
interface CustomState {
  chainId: string
  rpcUrl: string
  reactor: string
  collateral: string
  collateralDecimals: string
  debt: string
  debtDecimals: string
  feedUrl: string
}

const emptyCustom: CustomState = {
  chainId: '',
  rpcUrl: '',
  reactor: '',
  collateral: '',
  collateralDecimals: '18',
  debt: '',
  debtDecimals: '6',
  feedUrl: '',
}

const isAddress = (s: string) => /^0x[0-9a-fA-F]{40}$/.test(s.trim())
const isHttpUrl = (s: string) => /^https?:\/\/\S+$/i.test(s.trim())
const decimalsOk = (s: string) => {
  const n = Number(s)
  return Number.isInteger(n) && n >= 0 && n <= 36
}

/** Every required field present and well-formed — gates the custom form's Next. */
function isCustomComplete(c: CustomState): boolean {
  const chain = Number(c.chainId)
  return (
    Number.isInteger(chain) &&
    chain > 0 &&
    isHttpUrl(c.rpcUrl) &&
    isHttpUrl(c.feedUrl) &&
    isAddress(c.reactor) &&
    isAddress(c.collateral) &&
    isAddress(c.debt) &&
    c.collateral.trim().toLowerCase() !== c.debt.trim().toLowerCase() &&
    decimalsOk(c.collateralDecimals) &&
    decimalsOk(c.debtDecimals)
  )
}

/** The wire shape the create API expects under `custom`. */
function buildCustom(c: CustomState) {
  return {
    chainId: Number(c.chainId),
    rpcUrl: c.rpcUrl.trim(),
    reactor: c.reactor.trim(),
    collateral: c.collateral.trim(),
    collateralDecimals: Number(c.collateralDecimals),
    debt: c.debt.trim(),
    debtDecimals: Number(c.debtDecimals),
    feedUrl: c.feedUrl.trim(),
  }
}

/**
 * The add-bot wizard: corridor, then name, then wallet.
 *
 * Deliberately three visible steps rather than one long form, matching the desktop
 * app. Nothing is sent until the last step, and the secret fields are never
 * pre-filled or read back — the API has no route that returns key material
 * (Create wallet returns a phrase once at generation time only).
 *
 * The corridor step also offers "Custom": a short form for a pair the catalog
 * doesn't ship yet. It collects only what can't be defaulted (chain, RPC,
 * reactor, the two tokens, a price feed); Permit2, the indexer, spreads and sizes
 * default and are editable later from the bot's Settings.
 */
export default function AddBot({ rfqDefault = false }: { rfqDefault?: boolean }) {
  const navigate = useNavigate()
  const [corridors, setCorridors] = useState<Corridor[] | null>(null)
  const [loadError, setLoadError] = useState<string | null>(null)

  const [step, setStep] = useState(0)
  const [corridorId, setCorridorId] = useState('')
  const [custom, setCustom] = useState<CustomState>(emptyCustom)
  // Within the corridor step, the custom picker swaps the list for the form.
  const [editingCustom, setEditingCustom] = useState(false)
  const [name, setName] = useState('')
  const [signer, setSigner] = useState<SignerState>(emptySigner)
  const [start, setStart] = useState(false)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [showTemplate, setShowTemplate] = useState(false)

  useEffect(() => {
    api
      .corridors()
      .then((r) => {
        setCorridors(r.corridors)
        // Pending corridors are listed but can't be built, so never preselect
        // one — the Next button would be dead on arrival.
        setCorridorId(r.corridors.find((c) => !c.pendingDeploy)?.id ?? '')
      })
      .catch((e) => setLoadError(e instanceof ApiError ? e.message : String(e)))
  }, [])

  if (loadError && !corridors) return <ErrorState error={loadError} />
  if (!corridors) return <Loading what="the corridor list" />

  const isCustom = corridorId === CUSTOM
  const corridor = corridors.find((c) => c.id === corridorId)
  // The chain the new bot will trade on, for the shared-wallet check. Comes from
  // the preset, or from the custom form once a chain id is typed.
  const chainId = isCustom
    ? Number.isInteger(Number(custom.chainId)) && Number(custom.chainId) > 0
      ? Number(custom.chainId)
      : undefined
    : corridor?.chainId

  async function submit() {
    // Re-check right before create so a fleet change between typing and click
    // still gets a confirm. Soft warning only — the API does not refuse.
    if (chainId) {
      try {
        const check = await api.checkSigner({
          chainId,
          signer: buildSigner(signer),
        })
        if (check.conflicts.length > 0) {
          const names = check.conflicts.map((c) => c.name).join(', ')
          if (
            !window.confirm(
              `Another bot already uses this wallet on chain ${chainId}: ${names}.\n\nSharing one wallet across bots on the same chain races nonces and will cause issues. Create anyway?`,
            )
          ) {
            return
          }
        }
      } catch {
        // Create will surface a bad key; don't block on a check failure.
      }
    }
    setBusy(true)
    setError(null)
    try {
      const res = await api.createBot({
        name: name.trim(),
        ...(isCustom ? { custom: buildCustom(custom) } : { corridorId }),
        start: rfqDefault ? false : start,
        signer: buildSigner(signer),
      })
      // Clear the secret from component state the moment it's no longer needed.
      setSigner(emptySigner)
      navigate(`/bots/${encodeURIComponent(res.bot.name)}`, {
        state: {
          note: res.message,
          // From the create API — not `bot.running`. Docker can report running
          // before the bot's Permit2 preflight finishes (and fails).
          needsPermit2: res.needsPermit2Approval,
        },
      })
    } catch (e) {
      setError(e instanceof ApiError ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="space-y-4">
      <h1 className="text-xl font-bold">Add a bot</h1>
      {rfqDefault && (
        <p className="text-sm text-muted">
          New bots quote Swap via RFQ, not the public ladder. After create,
          open Settings and connect the bot to Textile before starting it.
        </p>
      )}
      <Steps current={step} labels={['Corridor', 'Name', 'Wallet']} />

      {step === 0 && !editingCustom && (
        <Card title="Which corridor should it quote?">
          <div className="space-y-3">
            {corridors.map((c) => (
              <label
                key={c.id}
                className={`flex items-center gap-3 rounded-lg border p-3 ${
                  c.pendingDeploy
                    ? 'cursor-not-allowed border-line-soft opacity-50'
                    : c.id === corridorId
                      ? 'cursor-pointer border-accent bg-accent-tint'
                      : 'cursor-pointer border-line-soft hover:bg-hover'
                }`}
              >
                <input
                  type="radio"
                  name="corridor"
                  checked={c.id === corridorId}
                  disabled={c.pendingDeploy}
                  onChange={() => setCorridorId(c.id)}
                  className="accent-[var(--tx-accent)]"
                />
                <span className="font-bold">{c.displayName}</span>
                <span className="text-sm text-muted">{c.networkLabel}</span>
                {c.pendingDeploy && (
                  <span className="text-xs text-faint">not deployed yet</span>
                )}
                <span className="ml-auto text-xs text-faint">chain {c.chainId}</span>
              </label>
            ))}

            {/* Custom: a pair the catalog doesn't ship. Selecting it and pressing
                Next opens the details form rather than advancing the wizard. */}
            <label
              className={`flex items-center gap-3 rounded-lg border p-3 ${
                isCustom
                  ? 'cursor-pointer border-accent bg-accent-tint'
                  : 'cursor-pointer border-line-soft hover:bg-hover'
              }`}
            >
              <input
                type="radio"
                name="corridor"
                checked={isCustom}
                onChange={() => setCorridorId(CUSTOM)}
                className="accent-[var(--tx-accent)]"
              />
              <span className="font-bold">Custom corridor</span>
              <span className="text-sm text-muted">
                Your own tokens, RPC and price feed
              </span>
            </label>
          </div>
          <div className="mt-4 flex items-center justify-between">
            <button
              className="text-xs text-muted underline disabled:opacity-40"
              onClick={() => setShowTemplate(!showTemplate)}
              disabled={!corridor}
            >
              {showTemplate ? 'Hide' : 'Show'} the config this writes
            </button>
            <Button
              variant="primary"
              onClick={() => (isCustom ? setEditingCustom(true) : setStep(1))}
              disabled={!corridorId}
            >
              Next
            </Button>
          </div>
          {showTemplate && corridor && (
            <pre className="mt-3 max-h-72 overflow-auto rounded-lg bg-canvas p-3 font-mono text-xs leading-relaxed">
              {corridor.tomlTemplate}
            </pre>
          )}
        </Card>
      )}

      {step === 0 && editingCustom && (
        <Card title="Custom corridor details">
          <div className="space-y-4">
            <p className="text-sm text-muted">
              Just the essentials. Permit2, the indexer, spreads and order sizes
              use safe defaults you can change later on the bot&apos;s Settings
              page (or Tools → Edit raw config).
            </p>

            <Field
              label="Chain ID"
              hint="The EVM chain the pair trades on, e.g. 42220 for Celo."
            >
              <Input
                value={custom.chainId}
                inputMode="numeric"
                placeholder="42220"
                onChange={(e) => setCustom({ ...custom, chainId: e.target.value })}
              />
            </Field>

            <Field label="RPC URL" hint="An http(s) JSON-RPC endpoint for that chain.">
              <Input
                value={custom.rpcUrl}
                placeholder="https://forno.celo.org"
                onChange={(e) => setCustom({ ...custom, rpcUrl: e.target.value })}
              />
            </Field>

            <Field
              label="Reactor address"
              hint="SETTLEMENT_V3_FILLER_REACTOR on this chain — where the bot's orders settle. No default: a wrong or zero reactor posts orders that can never fill."
            >
              <Input
                value={custom.reactor}
                placeholder="0x…"
                onChange={(e) => setCustom({ ...custom, reactor: e.target.value })}
              />
            </Field>

            <div className="grid grid-cols-1 gap-4 sm:grid-cols-[1fr_7rem]">
              <Field
                label="Collateral (soft) token"
                hint="The asset the bot buys low and sells high, e.g. cNGN."
              >
                <Input
                  value={custom.collateral}
                  placeholder="0x…"
                  onChange={(e) =>
                    setCustom({ ...custom, collateral: e.target.value })
                  }
                />
              </Field>
              <Field label="Decimals">
                <Input
                  value={custom.collateralDecimals}
                  inputMode="numeric"
                  onChange={(e) =>
                    setCustom({ ...custom, collateralDecimals: e.target.value })
                  }
                />
              </Field>
            </div>

            <div className="grid grid-cols-1 gap-4 sm:grid-cols-[1fr_7rem]">
              <Field
                label="Debt (stable) token"
                hint="The stable asset it quotes against, e.g. USDT."
              >
                <Input
                  value={custom.debt}
                  placeholder="0x…"
                  onChange={(e) => setCustom({ ...custom, debt: e.target.value })}
                />
              </Field>
              <Field label="Decimals">
                <Input
                  value={custom.debtDecimals}
                  inputMode="numeric"
                  onChange={(e) =>
                    setCustom({ ...custom, debtDecimals: e.target.value })
                  }
                />
              </Field>
            </div>

            <Field
              label="Price feed URL"
              hint="An http(s) endpoint returning { price, timestamp } — the debt-per-collateral mid the bot quotes around."
            >
              <Input
                value={custom.feedUrl}
                placeholder="https://api.textilecredit.com/price?chainId=42220&pair=cngn-usdt"
                onChange={(e) => setCustom({ ...custom, feedUrl: e.target.value })}
              />
            </Field>

            <div className="flex justify-between">
              <Button onClick={() => setEditingCustom(false)}>Back</Button>
              <Button
                variant="primary"
                onClick={() => setStep(1)}
                disabled={!isCustomComplete(custom)}
              >
                Next
              </Button>
            </div>
          </div>
        </Card>
      )}

      {step === 1 && (
        <Card title="Name it">
          <div className="space-y-4">
            <Field
              label="Bot name"
              hint="Lowercase letters, digits and single hyphens. This becomes the config directory and part of the container name, so it can't be changed later without recreating the bot."
            >
              <Input
                value={name}
                autoFocus
                placeholder="bot-a"
                onChange={(e) => setName(e.target.value)}
              />
            </Field>
            <div className="flex justify-between">
              <Button onClick={() => setStep(0)}>Back</Button>
              <Button
                variant="primary"
                onClick={() => setStep(2)}
                disabled={name.trim().length === 0}
              >
                Next
              </Button>
            </div>
          </div>
        </Card>
      )}

      {step === 2 && (
        <Card title="Set up the operator wallet">
          <div className="space-y-4">
            <SignerFields value={signer} onChange={setSigner} />

            <SignerConflictWarning chainId={chainId} signer={signer} />

            {rfqDefault ? (
              <p className="text-xs text-faint">
                Created stopped. Connect it to Textile on Settings, then
                approve Permit2, then Start. Starting before Connect quotes
                nothing.
              </p>
            ) : (
              <>
                <Toggle
                  checked={start}
                  onChange={setStart}
                  label="Start it immediately"
                />
                {!start && (
                  <p className="text-xs text-faint">
                    Left off, the bot is created but stopped. On its page,
                    approve Permit2 for the input tokens (needs a little gas on
                    the operator wallet), then dry-run — that&apos;s the safer
                    order before the first live start.
                  </p>
                )}
              </>
            )}

            {error && <Banner tone="danger">{error}</Banner>}

            <div className="flex justify-between">
              <Button onClick={() => setStep(1)}>Back</Button>
              <Button
                variant="primary"
                busy={busy}
                onClick={() => void submit()}
                disabled={!isSignerComplete(signer)}
              >
                Create
              </Button>
            </div>
          </div>
        </Card>
      )}
    </div>
  )
}

function Steps({ current, labels }: { current: number; labels: string[] }) {
  return (
    <ol className="flex items-center gap-2 text-sm">
      {labels.map((label, i) => (
        <li key={label} className="flex items-center gap-2">
          <span
            className={`flex size-6 items-center justify-center rounded-full text-xs font-bold ${
              i <= current ? 'bg-accent text-on-accent' : 'bg-hover text-faint'
            }`}
          >
            {i + 1}
          </span>
          <span className={i === current ? 'font-bold' : 'text-muted'}>{label}</span>
          {i < labels.length - 1 && <span className="text-faint">→</span>}
        </li>
      ))}
    </ol>
  )
}
