// The bot's ERC-8004 agent identity, under Tools.
//
// Optional and off the setup path on purpose: a bot quotes the same with or
// without one, so nothing in the add-corridor wizard asks for it. Register is
// one transaction from the operator wallet, `register(agentURI)` on the
// Identity Registry of the bot's own chain. The agentURI is a data URI holding
// the whole registration file, so the name and description the operator
// writes here are stored onchain with the identity and nobody hosts anything.
// `stitch register` on the CLI does the same.
//
// The one-shot can't write the config (it's mounted read-only), so it prints
// the minted id and this card hands it to the link route, which checks
// `ownerOf` before recording it.

import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react'
import { Link } from 'react-router-dom'
import { ApiError, api } from '../api'
import {
  agentIdFromOutput,
  confirmRegisterText,
  gasPhrase,
  identityPhase,
  networkName,
  profileProblem,
  readableFile,
  registrationFile,
} from '../agentIdentity'
import { botPath } from '../botRoutes'
import { shortAddress } from '../format'
import { appendLine, LEVEL_CLASS } from '../logBuffer'
import { streamSse } from '../sse'
import type { AgentIdentity as Identity, AgentProfile, ExitEvent, LogLine } from '../types'
import { Disclosure } from './ModuleControls'
import { Banner, Button, Card, Field, Input, Loading, Tag, TextArea } from './ui'

export default function AgentIdentity({
  bot,
  vault,
  onStop,
}: {
  bot: string
  /** The bot's OperatorVault, when it makes for one. Listed in the file. */
  vault: string | null
  /** Stop the bot the backend named as holding the wallet's nonce. */
  onStop: (target: string) => void
}) {
  const [identity, setIdentity] = useState<Identity | null>(null)
  const [profile, setProfile] = useState<AgentProfile | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [lines, setLines] = useState<LogLine[]>([])
  const [running, setRunning] = useState(false)
  const [exit, setExit] = useState<ExitEvent | null>(null)
  const active = useRef<{ abort: () => void } | null>(null)

  const show = useCallback((next: Identity) => {
    setIdentity(next)
    // Seed the form once; a reload after a failed run keeps what was typed.
    setProfile((typed) => typed ?? next.defaults)
  }, [])

  const load = useCallback(async () => {
    try {
      show(await api.identity(bot))
      setError(null)
    } catch (e) {
      setError(messageOf(e))
    }
  }, [bot, show])

  useEffect(() => {
    void load()
  }, [load])
  useEffect(() => () => active.current?.abort(), [])

  async function link(agentId: string) {
    setError(null)
    try {
      show(await api.linkIdentity(bot, agentId))
    } catch (e) {
      setError(messageOf(e))
    }
  }

  async function registerViaCustody(text: AgentProfile) {
    setRunning(true)
    setError(null)
    try {
      show(await api.registerViaCustody(bot, text))
    } catch (e) {
      const message = messageOf(e)
      try {
        const next = await api.identity(bot)
        show(next)
        // A timeout leaves the call pending, and the pending card says so.
        setError(next.pending ? null : message)
      } catch {
        setError(message)
      }
    } finally {
      setRunning(false)
    }
  }

  function register(current: Identity, text: AgentProfile) {
    if (!window.confirm(confirmRegisterText(bot, current))) return
    if (current.viaCustody) {
      void registerViaCustody(text)
      return
    }
    active.current?.abort()
    setLines([])
    setExit(null)
    setError(null)
    setRunning(true)
    let seen: LogLine[] = []
    active.current = streamSse(
      api.registerUrl(bot),
      { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(text) },
      {
        onEvent: (event, data) => {
          if (event === 'line') {
            seen = appendLine(seen, data as LogLine)
            setLines(seen)
          } else if (event === 'exit') {
            const result = data as ExitEvent
            setExit(result)
            setRunning(false)
            const minted = result.ok ? agentIdFromOutput(seen) : null
            if (minted) void link(minted)
            else void load()
          } else if (event === 'error') {
            setError((data as { message: string }).message)
            setRunning(false)
          }
        },
        onDone: () => setRunning(false),
        onError: (message) => {
          setError(message)
          setRunning(false)
        },
      },
    )
  }

  const phase = identity ? identityPhase(identity) : null
  return (
    <Card
      title="ERC-8004 agent identity"
      action={phase === 'registered' ? <RegisteredPill /> : <Tag>optional</Tag>}
    >
      {!identity && !error && <Loading what="agent identity" />}
      {!identity && error && <Banner tone="danger">{error}</Banner>}
      {identity && phase && (
        <div className="space-y-4">
          <Intro identity={identity} phase={phase} />
          {phase === 'registered' && <Registered identity={identity} />}
          {phase === 'link' && <LinkExisting identity={identity} onLink={link} />}
          {phase === 'pending' && identity.pending && (
            <PendingCustody
              pending={identity.pending}
              running={running}
              onCheck={() => void registerViaCustody(profile ?? identity.defaults!)}
            />
          )}
          {phase === 'form' && profile && (
            <RegisterForm
              bot={bot}
              identity={identity}
              profile={profile}
              vault={vault}
              running={running}
              onChange={setProfile}
              onReset={() => setProfile(identity.defaults)}
              onRegister={() => register(identity, profile)}
              onStopWatching={() => {
                active.current?.abort()
                setRunning(false)
              }}
              onStop={onStop}
            />
          )}
          {identity.readError && phase !== 'unavailable' && (
            <Banner tone="warning">{identity.readError}</Banner>
          )}
          {error && <Banner tone="danger">{error}</Banner>}
          {exit && !exit.ok && (
            <Banner tone="danger">
              register exited with code {exit.code}. The output below says why.
            </Banner>
          )}
          {lines.length > 0 && <Output lines={lines} />}
        </div>
      )}
    </Card>
  )
}

function messageOf(e: unknown): string {
  return e instanceof ApiError ? e.message : String(e)
}

function Intro({ identity, phase }: { identity: Identity; phase: string }) {
  if (phase === 'unavailable') {
    return (
      <p className="text-sm text-muted">
        ERC-8004 has no Identity Registry on {networkName(identity)} yet, so there is
        nothing to register this bot with.
      </p>
    )
  }
  return (
    <p className="text-sm text-muted">
      Lists this bot in the public ERC-8004 agent registry on {networkName(identity)},
      so other agents can find it and, over time, build up a track record for it.
      Optional: the bot quotes the same either way.
    </p>
  )
}

function RegisterForm({
  bot,
  identity,
  profile,
  vault,
  running,
  onChange,
  onReset,
  onRegister,
  onStopWatching,
  onStop,
}: {
  bot: string
  identity: Identity
  profile: AgentProfile
  vault: string | null
  running: boolean
  onChange: (next: AgentProfile) => void
  onReset: () => void
  onRegister: () => void
  onStopWatching: () => void
  onStop: (target: string) => void
}) {
  const problem = profileProblem(profile, identity.limits)
  const edited =
    !!identity.defaults &&
    (profile.name !== identity.defaults.name ||
      profile.description !== identity.defaults.description)
  const cost = gasPhrase(identity.gas)
  return (
    <>
      <div className="space-y-3">
        <Field
          label="Name"
          hint={<Counter text={profile.name} max={identity.limits.name} />}
        >
          <Input
            value={profile.name}
            disabled={running}
            onChange={(e) => onChange({ ...profile, name: e.target.value })}
          />
        </Field>
        <Field
          label="Description"
          hint={<Counter text={profile.description} max={identity.limits.description} />}
        >
          <TextArea
            rows={2}
            value={profile.description}
            disabled={running}
            className="font-sans text-sm"
            onChange={(e) => onChange({ ...profile, description: e.target.value })}
          />
        </Field>
        <p className="text-xs text-faint">
          Both are stored onchain with the identity, and anyone can read them.
          Changing them later takes another transaction.
          {edited && !running && (
            <>
              {' '}
              <button type="button" className="underline hover:text-ink" onClick={onReset}>
                Use the generated text
              </button>
            </>
          )}
        </p>
      </div>
      <Rows>
        <Row label="Owner">
          <Mono>{identity.owner ? shortAddress(identity.owner) : '—'}</Mono>
          <span className="text-muted"> · this bot&apos;s operator wallet</span>
        </Row>
        <Row label="Cost">
          One transaction{cost ? `, ${cost}` : ''}
          {identity.gas && (
            <span className={identity.gas.enough ? 'text-muted' : 'text-warning'}>
              {' '}
              · wallet has {identity.gas.balance} {identity.gas.symbol}
            </span>
          )}
        </Row>
      </Rows>
      <Disclosure title="What gets stored onchain">
        <pre className="overflow-x-auto whitespace-pre-wrap break-all font-mono text-xs leading-relaxed text-muted">
          {readableFile(registrationFile(profile, identity.chainId, vault))}
        </pre>
      </Disclosure>
      {!running && <Blocked bot={bot} identity={identity} onStop={onStop} />}
      {problem && <p className="text-sm text-warning">{problem}</p>}
      <div className="flex flex-wrap items-center gap-2">
        <Button
          busy={running}
          disabled={!identity.canRegister || problem !== null || running}
          onClick={onRegister}
        >
          Register on {networkName(identity)}
        </Button>
        {running && !identity.viaCustody && (
          <Button variant="ghost" onClick={onStopWatching}>
            Stop watching
          </Button>
        )}
      </div>
      {running && identity.viaCustody && (
        <p className="text-xs text-muted">
          Waiting on Fireblocks. If a policy needs a co-signer, approve the Contract
          Call there. You can leave this page: the panel remembers this registration
          and checks on it when you come back.
        </p>
      )}
    </>
  )
}

function Counter({ text, max }: { text: string; max: number }) {
  const used = [...text.trim()].length
  return <span className={used > max ? 'text-warning' : undefined}>{`${used} / ${max}`}</span>
}

/**
 * A Fireblocks registration is out and hasn't landed. Checking again asks
 * Fireblocks about that same call; the panel won't send a second.
 */
function PendingCustody({
  pending,
  running,
  onCheck,
}: {
  pending: NonNullable<Identity['pending']>
  running: boolean
  onCheck: () => void
}) {
  return (
    <div className="space-y-3">
      <Banner tone="warning">{pending.message}</Banner>
      <Button busy={running} onClick={onCheck}>
        Check again
      </Button>
    </div>
  )
}

/** The wallet already owns an identity: record it instead of minting another. */
function LinkExisting({
  identity,
  onLink,
}: {
  identity: Identity
  onLink: (agentId: string) => Promise<void>
}) {
  const [agentId, setAgentId] = useState(identity.sibling?.agentId ?? '')
  const [busy, setBusy] = useState(false)
  async function submit(id: string) {
    setBusy(true)
    await onLink(id.trim())
    setBusy(false)
  }
  return (
    <div className="space-y-3">
      <Banner tone="info">
        This bot&apos;s wallet already owns an ERC-8004 identity on{' '}
        {networkName(identity)}. Registering again would mint a second one, so link
        the one it has.
      </Banner>
      {identity.sibling ? (
        <div className="flex flex-wrap items-center gap-3 text-sm">
          <span>
            <strong>{identity.sibling.bot}</strong> uses the same wallet and recorded it
            as agent #{identity.sibling.agentId}.
          </span>
          <Button busy={busy} onClick={() => void submit(identity.sibling!.agentId)}>
            Use agent #{identity.sibling.agentId}
          </Button>
        </div>
      ) : (
        <div className="flex flex-wrap items-end gap-2">
          <div className="w-40">
            <Field label="Agent ID">
              <Input
                inputMode="numeric"
                value={agentId}
                onChange={(e) => setAgentId(e.target.value)}
              />
            </Field>
          </div>
          <Button
            busy={busy}
            disabled={!/^\d+$/.test(agentId.trim())}
            onClick={() => void submit(agentId)}
          >
            Link
          </Button>
        </div>
      )}
      <p className="text-xs text-faint">
        The panel checks onchain that this wallet owns the id before saving it. The id
        is on 8004scan, or in the Registered event of the transaction that minted it.
      </p>
    </div>
  )
}

function Registered({ identity }: { identity: Identity }) {
  return (
    <>
      {identity.ownedBy && (
        <Banner tone="warning">
          Agent #{identity.agentId} now belongs to <Mono>{shortAddress(identity.ownedBy)}</Mono>
          , not this bot&apos;s wallet. It was transferred, so this bot can no longer
          change it.
        </Banner>
      )}
      <Rows>
        <Row label="Agent">
          <span className="font-bold">#{identity.agentId}</span>
          <span className="text-muted"> on {networkName(identity)}</span>
        </Row>
        {identity.stored && (
          <>
            <Row label="Name">{identity.stored.name}</Row>
            <Row label="Description">{identity.stored.description}</Row>
          </>
        )}
        <Row label="Owner">
          <Mono>{identity.owner ? shortAddress(identity.owner) : '—'}</Mono>
          <span className="text-muted"> · this bot&apos;s operator wallet</span>
        </Row>
      </Rows>
      <div className="flex flex-wrap gap-x-4 gap-y-1 text-sm">
        {identity.links.agent && (
          <ExternalLink href={identity.links.agent}>View on 8004scan</ExternalLink>
        )}
        {identity.links.registry && (
          <ExternalLink href={identity.links.registry}>Registry contract</ExternalLink>
        )}
      </div>
    </>
  )
}

/** Why Register is off, and the one thing that turns it back on. */
function Blocked({
  bot,
  identity,
  onStop,
}: {
  bot: string
  identity: Identity
  onStop: (target: string) => void
}) {
  const reason = identity.blockedReason
  if (!reason) return null
  return (
    <Banner tone="warning">
      <div className="space-y-2">
        <p>{reason}</p>
        {identity.blockedKind === 'transacting' && identity.blockedBy && (
          <Button onClick={() => onStop(identity.blockedBy!)}>
            {identity.blockedBy === bot
              ? `Stop ${bot} so it can register`
              : `Stop ${identity.blockedBy} so ${bot} can register`}
          </Button>
        )}
        {identity.blockedKind === 'gas' && (
          <Link to={botPath(bot, 'funds')} className="font-bold underline hover:no-underline">
            Open Funds
          </Link>
        )}
      </div>
    </Banner>
  )
}

function Output({ lines }: { lines: LogLine[] }) {
  return (
    <div className="max-h-72 overflow-auto rounded-lg bg-canvas p-3 font-mono text-xs leading-relaxed">
      {lines.map((line, i) => (
        <div key={i} className={`whitespace-pre-wrap ${LEVEL_CLASS[line.level]}`}>
          {line.text}
        </div>
      ))}
    </div>
  )
}

function RegisteredPill() {
  return (
    <span className="inline-flex items-center rounded-full bg-success-bg px-2.5 py-0.5 text-xs font-bold text-success">
      registered
    </span>
  )
}

function Rows({ children }: { children: ReactNode }) {
  return (
    <dl className="grid grid-cols-[6.5rem_1fr] gap-x-4 gap-y-2 text-sm">{children}</dl>
  )
}

function Row({ label, children }: { label: string; children: ReactNode }) {
  return (
    <>
      <dt className="text-faint">{label}</dt>
      <dd className="min-w-0">{children}</dd>
    </>
  )
}

function Mono({ children }: { children: ReactNode }) {
  return <span className="font-mono text-xs">{children}</span>
}

function ExternalLink({ href, children }: { href: string; children: ReactNode }) {
  return (
    <a href={href} target="_blank" rel="noreferrer" className="text-accent underline">
      {children}
    </a>
  )
}
