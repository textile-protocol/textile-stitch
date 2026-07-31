import { Field, Input, Select } from './ui'

export type SignerKind = 'local' | 'turnkey' | 'mpcvault'
export type KeyForm = 'privateKey' | 'seedPhrase'

/** The signer state a form owns, so both the wizard and Change signer share one shape. */
export interface SignerState {
  kind: SignerKind
  keyForm: KeyForm
  secret: string
  mpc: Record<string, string>
}

export const emptySigner: SignerState = {
  kind: 'local',
  keyForm: 'privateKey',
  secret: '',
  mpc: {},
}

/**
 * The signer picker plus the fields the chosen backend needs. Controlled: the parent
 * owns the state so it can clear the secret the moment it's sent. Shared by the add-bot
 * wizard and the Change signer flow, so the two can't drift on field names.
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
          onChange={(e) => set({ kind: e.target.value as SignerKind })}
        >
          <option value="local">Hot wallet (local key)</option>
          <option value="turnkey">MPC — Turnkey</option>
          <option value="mpcvault">MPC — MPCVault · Experimental</option>
        </Select>
      </Field>

      {value.kind === 'local' && (
        <>
          <Field label="Key material">
            <Select
              value={value.keyForm}
              onChange={(e) => set({ keyForm: e.target.value as KeyForm })}
            >
              <option value="privateKey">Private key</option>
              <option value="seedPhrase">Seed phrase</option>
            </Select>
          </Field>
          <Field
            label={value.keyForm === 'privateKey' ? 'Private key' : 'Seed phrase'}
            hint="Written to an owner-only file on the host and mounted read-only into the bot. The panel never reads it back."
          >
            <Input
              type="password"
              value={value.secret}
              autoComplete="off"
              spellCheck={false}
              placeholder={value.keyForm === 'privateKey' ? '0x…' : 'twelve words…'}
              onChange={(e) => set({ secret: e.target.value })}
            />
          </Field>
        </>
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
      return s.secret.trim().length > 0
    case 'turnkey':
      return ['organizationId', 'signWith', 'operatorAddress', 'apiPublicKey', 'apiPrivateKey'].every(
        filled,
      )
    case 'mpcvault':
      return ['vaultUuid', 'clientSignerPubkey', 'operatorAddress', 'apiToken'].every(filled)
  }
}

/** The `kind`-tagged request body the API expects. */
export function buildSigner(s: SignerState): unknown {
  if (s.kind === 'local') {
    return { kind: s.kind, [s.keyForm]: s.secret.trim() }
  }
  return { kind: s.kind, ...s.mpc }
}
