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
  Select,
  Toggle,
} from '../components/ui'
import type { Corridor } from '../types'

type SignerKind = 'local' | 'turnkey' | 'mpcvault'
type KeyForm = 'privateKey' | 'seedPhrase'

/**
 * The add-bot wizard: corridor, then name, then signer.
 *
 * Deliberately three visible steps rather than one long form, matching the desktop
 * app. Nothing is sent until the last step, and the secret fields are never
 * pre-filled or read back — the API has no route that returns key material.
 */
export default function AddBot() {
  const navigate = useNavigate()
  const [corridors, setCorridors] = useState<Corridor[] | null>(null)
  const [loadError, setLoadError] = useState<string | null>(null)

  const [step, setStep] = useState(0)
  const [corridorId, setCorridorId] = useState('')
  const [name, setName] = useState('')
  const [signerKind, setSignerKind] = useState<SignerKind>('local')
  const [keyForm, setKeyForm] = useState<KeyForm>('privateKey')
  const [secret, setSecret] = useState('')
  const [mpc, setMpc] = useState<Record<string, string>>({})
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
    setBusy(true)
    setError(null)
    try {
      const res = await api.createBot({
        name: name.trim(),
        corridorId,
        start,
        signer: buildSigner(signerKind, keyForm, secret, mpc),
      })
      // Clear the secret from component state the moment it's no longer needed.
      setSecret('')
      setMpc({})
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
      <Steps current={step} labels={['Corridor', 'Name', 'Signer']} />

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
        <Card title="How does it sign?">
          <div className="space-y-4">
            <Field label="Signer">
              <Select
                value={signerKind}
                onChange={(e) => setSignerKind(e.target.value as SignerKind)}
              >
                <option value="local">Hot wallet (local key)</option>
                <option value="turnkey">MPC — Turnkey</option>
                <option value="mpcvault">MPC — MPCVault · Experimental</option>
              </Select>
            </Field>

            {signerKind === 'local' && (
              <>
                <Field label="Key material">
                  <Select
                    value={keyForm}
                    onChange={(e) => setKeyForm(e.target.value as KeyForm)}
                  >
                    <option value="privateKey">Private key</option>
                    <option value="seedPhrase">Seed phrase</option>
                  </Select>
                </Field>
                <Field
                  label={keyForm === 'privateKey' ? 'Private key' : 'Seed phrase'}
                  hint="Written to an owner-only file on the host and mounted read-only into the bot. The panel never reads it back."
                >
                  <Input
                    type="password"
                    value={secret}
                    autoComplete="off"
                    spellCheck={false}
                    placeholder={keyForm === 'privateKey' ? '0x…' : 'twelve words…'}
                    onChange={(e) => setSecret(e.target.value)}
                  />
                </Field>
              </>
            )}

            {signerKind === 'turnkey' && (
              <MpcFields
                values={mpc}
                onChange={setMpc}
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

            {signerKind === 'mpcvault' && (
              <MpcFields
                values={mpc}
                onChange={setMpc}
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
                disabled={!isSignerComplete(signerKind, secret, mpc)}
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

/** Only the fields the chosen backend actually needs. */
function isSignerComplete(
  kind: SignerKind,
  secret: string,
  mpc: Record<string, string>,
): boolean {
  const filled = (k: string) => (mpc[k] ?? '').trim().length > 0
  switch (kind) {
    case 'local':
      return secret.trim().length > 0
    case 'turnkey':
      return (
        ['organizationId', 'signWith', 'operatorAddress', 'apiPublicKey', 'apiPrivateKey']
          .every(filled)
      )
    case 'mpcvault':
      return ['vaultUuid', 'clientSignerPubkey', 'operatorAddress', 'apiToken'].every(
        filled,
      )
  }
}

function buildSigner(
  kind: SignerKind,
  keyForm: KeyForm,
  secret: string,
  mpc: Record<string, string>,
): unknown {
  if (kind === 'local') {
    return { kind, [keyForm]: secret.trim() }
  }
  return { kind, ...mpc }
}
