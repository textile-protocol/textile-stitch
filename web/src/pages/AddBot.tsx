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

/**
 * The add-bot wizard: corridor, then name, then wallet.
 *
 * Deliberately three visible steps rather than one long form, matching the desktop
 * app. Nothing is sent until the last step, and the secret fields are never
 * pre-filled or read back — the API has no route that returns key material
 * (Create wallet returns a phrase once at generation time only).
 */
export default function AddBot() {
  const navigate = useNavigate()
  const [corridors, setCorridors] = useState<Corridor[] | null>(null)
  const [loadError, setLoadError] = useState<string | null>(null)

  const [step, setStep] = useState(0)
  const [corridorId, setCorridorId] = useState('')
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
        setCorridorId(r.corridors[0]?.id ?? '')
      })
      .catch((e) => setLoadError(e instanceof ApiError ? e.message : String(e)))
  }, [])

  if (loadError && !corridors) return <ErrorState error={loadError} />
  if (!corridors) return <Loading what="the corridor list" />

  const corridor = corridors.find((c) => c.id === corridorId)

  async function submit() {
    // Re-check right before create so a fleet change between typing and click
    // still gets a confirm. Soft warning only — the API does not refuse.
    if (corridor) {
      try {
        const check = await api.checkSigner({
          chainId: corridor.chainId,
          signer: buildSigner(signer),
        })
        if (check.conflicts.length > 0) {
          const names = check.conflicts.map((c) => c.name).join(', ')
          if (
            !window.confirm(
              `Another bot already uses this wallet on chain ${corridor.chainId}: ${names}.\n\nSharing one wallet across bots on the same chain races nonces and will cause issues. Create anyway?`,
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
        corridorId,
        start,
        signer: buildSigner(signer),
      })
      // Clear the secret from component state the moment it's no longer needed.
      setSigner(emptySigner)
      navigate(`/bots/${encodeURIComponent(res.bot.name)}`, {
        state: { note: res.message },
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
      <Steps current={step} labels={['Corridor', 'Name', 'Wallet']} />

      {step === 0 && (
        <Card title="Which corridor should it quote?">
          <div className="space-y-3">
            {corridors.map((c) => (
              <label
                key={c.id}
                className={`flex cursor-pointer items-center gap-3 rounded-lg border p-3 ${
                  c.id === corridorId
                    ? 'border-accent bg-accent-tint'
                    : 'border-line-soft hover:bg-hover'
                }`}
              >
                <input
                  type="radio"
                  name="corridor"
                  checked={c.id === corridorId}
                  onChange={() => setCorridorId(c.id)}
                  className="accent-[var(--tx-accent)]"
                />
                <span className="font-bold">{c.displayName}</span>
                <span className="text-sm text-muted">{c.networkLabel}</span>
                <span className="ml-auto text-xs text-faint">chain {c.chainId}</span>
              </label>
            ))}
          </div>
          <div className="mt-4 flex items-center justify-between">
            <button
              className="text-xs text-muted underline"
              onClick={() => setShowTemplate(!showTemplate)}
            >
              {showTemplate ? 'Hide' : 'Show'} the config this writes
            </button>
            <Button variant="primary" onClick={() => setStep(1)} disabled={!corridor}>
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

            <SignerConflictWarning
              chainId={corridor?.chainId}
              signer={signer}
            />

            <Toggle
              checked={start}
              onChange={setStart}
              label="Start it immediately"
            />
            {!start && (
              <p className="text-xs text-faint">
                Left off, the bot is created but stopped. Approve the router
                allowance and dry-run it from its page first — that's the safer
                order.
              </p>
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
