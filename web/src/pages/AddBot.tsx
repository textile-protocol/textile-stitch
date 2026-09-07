import { useEffect, useRef, useState } from 'react'
import { useNavigate, useSearchParams } from 'react-router-dom'
import { ApiError, api } from '../api'
import { botPath } from '../botRoutes'
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
import AddCorridorFlow from '../components/wizard/AddCorridorFlow'
import ApprovalWait from '../components/wizard/ApprovalWait'
import CorridorPicker from '../components/wizard/CorridorPicker'
import FundStep, { type FundOutcome } from '../components/wizard/FundStep'
import LiveStep from '../components/wizard/LiveStep'
import SourcePicker, {
  isHttpUrl,
  sourceOk,
} from '../components/wizard/SourcePicker'
import SpreadFields, { spreadsOk } from '../components/wizard/SpreadFields'
import Steps, {
  LABELS,
  SHORT_LABELS,
  WHERE_LABELS,
} from '../components/wizard/Steps'
import WhereStep, { defaultChoice } from '../components/wizard/WhereStep'
import {
  botPools,
  loadCandidates,
  poolsQuote,
  withTimeout,
  type Candidate,
} from '../components/wizard/candidates'
import { add as addCopy, botRunState, place } from '../components/wizard/wizardCopy'
import {
  clearAddResume,
  clearResume,
  readAddResume,
  readResume,
  saveResume,
} from '../components/wizard/resume'
import { pairSymbols } from '../components/SpreadExample'
import type {
  Corridor,
  RfqAccessResult,
  RfqAccessStatus,
  RfqEnrollment,
  Spread,
} from '../types'

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

/** The Sources step: a default per source, or the operator's own URL. */
interface SourcesState {
  feedMode: 'default' | 'own'
  /** Textile's feed for this corridor, from the template. Empty for custom. */
  feedDefault: string
  feedUrl: string
  rpcMode: 'default' | 'own'
  rpcDefault: string
  rpcUrl: string
}

const emptySources: SourcesState = {
  feedMode: 'default',
  feedDefault: '',
  feedUrl: '',
  rpcMode: 'default',
  rpcDefault: '',
  rpcUrl: '',
}

const isAddress =(s: string) => /^0x[0-9a-fA-F]{40}$/.test(s.trim())
/** Same bar the venue applies to the access request, so a bad address stops at the Name step. */
const isEmail = (s: string) => /^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(s.trim())
/** Mirrors the panel's validate_bot_id, so a bad name stops at the Name step, not at Create. */
const MAX_BOT_NAME = 40
function botNameProblem(s: string): string | null {
  if (s === '') return null
  if (s.length > MAX_BOT_NAME)
    return `Bot name can't be longer than ${MAX_BOT_NAME} characters.`
  if (!/^[a-z0-9-]+$/.test(s))
    return 'Only lowercase letters, digits and hyphens. No spaces or capitals.'
  if (s.startsWith('-') || s.endsWith('-'))
    return 'Must start and end with a letter or digit.'
  if (s.includes('--')) return 'Two hyphens in a row are not allowed.'
  return null
}
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
 * Which corridor the picker should open on for a Fleet row's "Add corridor".
 *
 * The row carries `?bot=` and `?chain=`, and the chain alone lands on the first
 * corridor Textile lists on that chain. For the ordinary operator, who runs one
 * bot quoting one pair, that is the corridor the bot already has: Next then
 * reaches a Where screen where their own bot is blocked as "Already quoting" and
 * the only live row is a second bot. So the bot's own pools are read first and
 * the corridors it already quotes are skipped.
 *
 * Best effort, under a clock. A slow or unreadable bot falls back to the chain
 * hint on its own, which is what it was before, and the Where screen still says
 * why the prefilled bot isn't on offer.
 */
async function openingCorridor(
  onChain: Corridor[],
  bot: string | null,
): Promise<Corridor | undefined> {
  if (onChain.length === 0) return undefined
  if (!bot) return onChain[0]
  try {
    const pools = await withTimeout((signal) => botPools(bot, signal))
    return onChain.find((c) => !poolsQuote(pools, c)) ?? onChain[0]
  } catch {
    return onChain[0]
  }
}

/**
 * The add-bot wizard: corridor, spreads, sources, name, wallet, connect, fund,
 * live.
 *
 * Deliberately many short steps rather than one long form, matching the desktop
 * app. Nothing is sent until the wallet step, and the secret fields are never
 * pre-filled or read back — the API has no route that returns key material
 * (Create wallet returns a phrase once at generation time only).
 *
 * It has two endings and no other way out: the bot is running, or it is funded
 * and waiting for Textile to approve the maker. There is no "do this later" on
 * any step from Connect on. A bot created but left unfunded quotes nothing, so
 * dropping the operator on a settings page half way through was a way to end up
 * with a bot that never traded.
 *
 * The corridor list comes from Textile, not from this build: a corridor listed
 * on the site shows up here on the next page load, with the config already
 * generated for it. When the panel can't reach Textile it serves the corridors
 * compiled into it and says so.
 *
 * The step also offers "Custom": a short form for a pair Textile doesn't list at
 * all. It collects only what can't be defaulted (chain, RPC, reactor, the two
 * tokens, a price feed); Permit2, the indexer, spreads and sizes default and are
 * editable later from the bot's Settings.
 */
export default function AddBot({ rfqDefault = false }: { rfqDefault?: boolean }) {
  const navigate = useNavigate()
  const [searchParams, setSearchParams] = useSearchParams()
  const [corridors, setCorridors] = useState<Corridor[] | null>(null)
  // Set when the panel served its built-in list because Textile was unreachable.
  const [catalogWarning, setCatalogWarning] = useState<string | null>(null)
  const [loadError, setLoadError] = useState<string | null>(null)

  const [step, setStep] = useState(0)
  const [corridorId, setCorridorId] = useState('')
  const [custom, setCustom] = useState<CustomState>(emptyCustom)
  // Within the corridor step, the custom picker swaps the list for the form.
  const [editingCustom, setEditingCustom] = useState(false)
  const [customMode, setCustomMode] = useState<'fields' | 'toml'>('fields')
  const [importedToml, setImportedToml] = useState('')
  const [importError, setImportError] = useState<string | null>(null)
  const [name, setName] = useState('')
  const [signer, setSigner] = useState<SignerState>(emptySigner)
  const [start, setStart] = useState(false)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [showTemplate, setShowTemplate] = useState(false)
  // Spread step: pre-filled from the corridor's template, editable here so the
  // operator sets it at create instead of discovering it later in Settings.
  const [spreads, setSpreads] = useState<{ buy: Spread; sell: Spread }>({
    buy: { kind: 'bps', value: '1' },
    sell: { kind: 'bps', value: '1' },
  })
  // Sources step: Textile's defaults for this corridor, or the operator's own.
  // An explicit choice on purpose — independent price sources make the venue
  // more robust, so the default is offered, not assumed.
  const [sources, setSources] = useState<SourcesState>(emptySources)
  // Name step: contact details for the venue access request. Held here for the
  // one-click Connect that follows the wallet step.
  const [contact, setContact] = useState({ email: '', whatsapp: '' })
  // After create the wizard keeps going instead of leaving for the bot page.
  const [createdBot, setCreatedBot] = useState<string | null>(null)
  // The wizard opened straight on the Fund step for a bot from an earlier run.
  // Everything the earlier steps collected (corridor, contact, access answer)
  // is gone, so anything derived from them has to stay out of the way.
  const [resumed, setResumed] = useState(false)
  // The one-click Connect: registers the wallet with the venue, then files the
  // access request with the contact details from the Name step.
  const [connecting, setConnecting] = useState(false)
  const [connectError, setConnectError] = useState<string | null>(null)
  const [enrollment, setEnrollment] = useState<RfqEnrollment | null>(null)
  const [accessStatus, setAccessStatus] = useState<RfqAccessStatus | null>(null)
  const [accessMessage, setAccessMessage] = useState<string | null>(null)
  // How the Fund step ended, and therefore what the last step shows: the Live
  // screen once the bot is running, the Waiting screen until then. Null until
  // the Fund step reports. The waiting screen flips this to `live` itself when
  // Textile approves, so the last step changes without a step change.
  const [fundOutcome, setFundOutcome] = useState<FundOutcome | null>(null)

  // Which road this run is on.
  //
  // `new` is the wizard as it has always been: a new wallet, a new maker
  // identity, its own funding. `where` is the one screen that asks which bot
  // should quote the corridor, and it only exists when the operator already
  // runs one on that chain. `add` puts the corridor on a bot that exists.
  //
  // `step` deliberately stays at 0 on both new lanes, and `createdBot` stays
  // null. Three effects key off `step` (the `?resume=` stamp at 6, the record
  // clear at 7, the mount jump to 6), so leaving it alone is what stops the add
  // lane from ever stamping a live production bot into the resume machinery.
  // That is the worst thing this feature could do, and it is closed by the
  // shape of the state rather than by remembering not to.
  const [lane, setLane] = useState<'new' | 'where' | 'add'>('new')
  const [targetBot, setTargetBot] = useState<string | null>(null)
  const [candidates, setCandidates] = useState<Candidate[]>([])
  const [detecting, setDetecting] = useState(false)
  // The operator passed through the Where screen, so the rail keeps its pill.
  const [sawWhere, setSawWhere] = useState(false)
  // 0 price feed, 1 spread, 2 live (the writes run under the Live pill).
  const [shortStep, setShortStep] = useState(0)
  // Why a prefilled target was dropped, shown once on the Where screen.
  const [placeNotice, setPlaceNotice] = useState<string | null>(null)
  // Which row the Where screen is on, null being the separate-bot row. It lives
  // here rather than in that component because the rail is drawn above it: the
  // pill count is the one thing on screen saying which road is short, and while
  // the recommended row is selected it has to be the short one. Kept across a
  // trip into the add lane and back, so Back returns to the choice that was
  // made rather than to the default.
  const [whereChoice, setWhereChoice] = useState<string | null>(null)
  // The pool the add lane wrote, carried in the URL so a reload after the write
  // lands does not add it a second time.
  const [addedPoolIndex, setAddedPoolIndex] = useState<number | null>(null)

  // The URL as it was when this page opened. Mount-only effects read this, not
  // `searchParams`: the add lane writes its own target back into the URL, and a
  // deep-link effect that saw that write would rebuild the lane it just left.
  const openedWith = useRef(searchParams)
  // Read inside mount-only effects, which must not re-run when the URL changes.
  const paramsRef = useRef(searchParams)
  paramsRef.current = searchParams

  // The catalog, once. Guarded like the two effects below it: under StrictMode
  // this effect runs twice, and a second answer landing after the picker is up
  // used to overwrite whatever the operator had picked — including the custom
  // sentinel, which silently created a catalog bot from a custom form. The
  // write is also conditional, so a late answer can never win a race.
  const loadedRef = useRef(false)
  useEffect(() => {
    if (loadedRef.current) return
    loadedRef.current = true
    api
      .corridors()
      .then(async (r) => {
        // Pending corridors are listed but can't be built, so never preselect
        // one — the Next button would be dead on arrival.
        //
        // A link from a Fleet row carries `?chain=`, and a reload of the add
        // lane carries `?corridor=`. Both are preferences, not instructions:
        // they narrow which corridor opens, and the operator can pick anything
        // else. The write stays conditional so a late answer can't win a race.
        const wanted = paramsRef.current.get('corridor')
        const chainHint = Number(paramsRef.current.get('chain'))
        const live = r.corridors.filter((c) => !c.pendingDeploy)
        const onChain =
          Number.isInteger(chainHint) && chainHint > 0
            ? live.filter((c) => c.chainId === chainHint)
            : []
        const first =
          live.find((c) => c.id === wanted)?.id ??
          (await openingCorridor(onChain, paramsRef.current.get('bot')))?.id ??
          live[0]?.id ??
          ''
        // Both together, after the read above: the picker rendering for a beat
        // with nothing picked is a worse first frame than the loading line.
        setCorridors(r.corridors)
        setCatalogWarning(r.warning)
        setCorridorId((prev) => (prev === '' ? first : prev))
      })
      .catch((e) => setLoadError(e instanceof ApiError ? e.message : String(e)))
  }, [])

  // Re-seed the Spread and Sources steps from whichever template is in play:
  // the picked corridor's, the imported toml, or (custom fields) no template at
  // all, where the operator's own URLs stand in for Textile's defaults.
  //
  // Keyed on the template, not on corridorId, and skipped while nothing is
  // picked. The token picker reports '' between the first and second click, so
  // unpicking and repicking the SAME corridor used to run this twice and wipe
  // spreads the operator had already typed. The custom form's URLs are
  // deliberately NOT part of that key: the effect below mirrors them, and
  // keying on them here meant changing one character of an RPC URL threw away
  // a spread the operator had typed two steps earlier.
  const seededRef = useRef<string | null>(null)
  useEffect(() => {
    if (corridorId === '') return
    const catalog = corridors?.find((c) => c.id === corridorId)
    const isCustomFields = corridorId === CUSTOM && customMode === 'fields'
    const template =
      corridorId === CUSTOM
        ? customMode === 'toml'
          ? importedToml
          : ''
        : (catalog?.tomlTemplate ?? '')
    const applied = `${corridorId} ${template}`
    if (seededRef.current === applied) return
    seededRef.current = applied
    const d = defaultsFromToml(template)
    setSpreads({
      buy: { kind: 'bps', value: d.buyBps ?? '1' },
      sell: { kind: 'bps', value: d.sellBps ?? '1' },
    })
    setSources({
      feedMode: isCustomFields ? 'own' : 'default',
      feedDefault: isCustomFields ? '' : (d.feedUrl ?? ''),
      feedUrl: '',
      rpcMode: isCustomFields ? 'own' : 'default',
      rpcDefault: isCustomFields ? '' : (d.rpcUrl ?? ''),
      rpcUrl: '',
    })
  }, [corridors, corridorId, customMode, importedToml])

  // A custom corridor has no Textile default, so the form's own URLs fill the
  // Sources step. Its own effect, keyed on the URLs alone, so typing in that
  // form moves the URLs and touches nothing else. It runs after the seed above
  // whenever both fire, so the mirrored URLs land on top of the blanks.
  useEffect(() => {
    if (corridorId !== CUSTOM || customMode !== 'fields') return
    setSources((s) => ({
      ...s,
      feedMode: 'own',
      feedUrl: custom.feedUrl,
      rpcMode: 'own',
      rpcUrl: custom.rpcUrl,
    }))
  }, [corridorId, customMode, custom.feedUrl, custom.rpcUrl])

  // A reload, or a tab closed mid-funding, comes back to the Fund step for the
  // bot that was being set up. Only the name is remembered: the step re-reads
  // the bot, its wallet and its Textile access from the panel and lands in
  // whichever phase those say. `?resume=` wins over the stored record so a
  // pasted link works in a browser that has no record of its own.
  //
  // Decided once, on mount. Start over clears the record and resets the wizard,
  // and must not be pulled straight back into the step it just left.
  //
  // Precedence, in order. `?resume=` wins outright: a half-created bot sitting
  // mid-funding must never be stranded. Then `?bot=`, the add lane's own
  // target, which SUPPRESSES the stored record: without that, a stale record
  // from a run abandoned earlier the same day swallows the Fleet row's Add
  // corridor click and drops the operator on an unrelated bot's Fund step.
  // Then the stored record, as before.
  const resumeChecked = useRef(false)
  useEffect(() => {
    if (resumeChecked.current) return
    resumeChecked.current = true
    if (createdBot !== null) return
    const explicit = searchParams.get('resume')
    // An add-lane record outranks this one: it means a corridor is on a live
    // bot with Textile possibly not yet told about it, which is the more urgent
    // of the two unfinished runs. The effect below picks it up.
    const resumeBot =
      explicit ??
      (searchParams.get('bot') || readAddResume()
        ? null
        : (readResume()?.bot ?? null))
    if (!resumeBot) return
    setCreatedBot(resumeBot)
    setResumed(true)
    setStep(6)
  }, [createdBot, searchParams])

  // A reload while the add lane was open. `?bot=` plus `?corridor=` is enough
  // to rebuild it: the corridor is re-read from the catalog and the target is
  // re-validated against the fleet, so a bot deleted or changed in between
  // falls back to the Where screen with one line saying why rather than into a
  // request that would be refused.
  //
  // `&added=1` means the pool is already on disk. The duplicate check would
  // then be reporting this flow's own work back at it, so that one bot is
  // exempted and the lane resumes at the writes, which skip straight to
  // enrollment.
  const prefillChecked = useRef(false)
  useEffect(() => {
    if (prefillChecked.current) return
    if (!corridors) return
    const opened = openedWith.current
    if (opened.get('resume')) return
    // The URL recovers a reload of the same tab. The stored record recovers the
    // rest: a tab closed, or a browser lost, between the pool landing on disk
    // and Textile being told about it. Only consulted when the URL says nothing,
    // so a Fleet row's click is never swallowed by an older run.
    const stored =
      opened.get('bot') || opened.get('corridor') ? null : readAddResume()
    const wantedBot = opened.get('bot') ?? stored?.bot ?? null
    const wantedCorridor = opened.get('corridor') ?? stored?.corridorId ?? null
    if (!wantedBot || !wantedCorridor) return
    prefillChecked.current = true
    const picked = corridors.find((c) => c.id === wantedCorridor)
    if (!picked) return
    const already = opened.get('added') === '1' || stored !== null
    const poolParam = stored ? stored.poolIndex : Number(opened.get('pool'))
    setCorridorId(picked.id)
    setDetecting(true)
    void withTimeout((signal) =>
      loadCandidates(picked, already ? wantedBot : null, {
        prefer: wantedBot,
        signal,
      }),
    )
      .then(({ rows, fleetNames }) => {
        setCandidates(rows)
        // Same rule as the picker: the rail only gains a Where pill when the
        // screen is reachable, which needs a bot that can take the corridor.
        if (rows.some((c) => c.eligible)) setSawWhere(true)
        const target = rows.find((c) => c.bot.name === wantedBot)
        if (target?.eligible) {
          setTargetBot(wantedBot)
          // The Where screen is behind this lane, not in front of it: Back from
          // the price feed step opens it, and it has to open on the bot this
          // run is already adding to.
          setWhereChoice(wantedBot)
          setAddedPoolIndex(
            already && Number.isInteger(poolParam) && poolParam >= 0 ? poolParam : null,
          )
          setShortStep(already ? 2 : 0)
          setLane('add')
          return
        }
        setPlaceNotice(
          target
            ? place.droppedBlocked(wantedBot, target.blocked ?? '')
            : fleetNames.includes(wantedBot)
              ? place.droppedOffChain(wantedBot, picked.displayName)
              : place.droppedUnknown(wantedBot),
        )
        // Only ask when there is something to choose. A bot that exists on the
        // chain but cannot take this corridor is not an option, so a screen
        // whose only live row is "separate bot" is a question with one answer.
        // The reason the prefilled bot was dropped still reaches the operator:
        // it rides along as a notice on the step this falls through to.
        setWhereChoice(defaultChoice(rows, wantedBot))
        if (rows.some((c) => c.eligible)) setLane('where')
        else setStep(1)
      })
      .catch(() => setStep(1))
      .finally(() => setDetecting(false))
  }, [corridors])

  // Both endings clear the record. Waiting for Textile can last days, and a
  // record left behind reopens the Fund step for that bot on every later visit
  // to /add, which is to say no second bot could ever be created. The `?resume=`
  // in the URL still brings a reload of THIS tab back to where it was.
  useEffect(() => {
    if (step === 7 && fundOutcome) clearResume()
  }, [step, fundOutcome])

  // Put the bot in the URL once funding starts, so a refresh keeps its place.
  useEffect(() => {
    if (step < 6 || !createdBot) return
    if (searchParams.get('resume') === createdBot) return
    const next = new URLSearchParams(searchParams)
    next.set('resume', createdBot)
    setSearchParams(next, { replace: true })
  }, [step, createdBot, searchParams, setSearchParams])

  if (loadError && !corridors) return <ErrorState error={loadError} />
  if (!corridors) return <Loading what="the corridor list" />

  const isCustom = corridorId === CUSTOM
  const corridor = corridors.find((c) => c.id === corridorId)
  // The pair's symbols for the Spread step's copy and worked example. Null for
  // a custom or imported corridor, where the step reads without them.
  const symbols = isCustom ? null : pairSymbols(corridor?.displayName)
  // The chain the new bot will trade on, for the shared-wallet check. Comes from
  // the preset, or from the custom form once a chain id is typed.
  const importedChainId = chainIdFromToml(importedToml)
  const chainId = isCustom
    ? customMode === 'toml'
      ? importedChainId
      : Number.isInteger(Number(custom.chainId)) && Number(custom.chainId) > 0
        ? Number(custom.chainId)
        : undefined
    : corridor?.chainId
  // The pair for the Live screen's public swap link, from the taker's side:
  // they sell the stable and buy the local token. Null for a custom corridor,
  // and null after a resume: the corridor step still preselects the first
  // corridor in the catalog on every load, so on a resumed run these symbols
  // belong to an unrelated pair and would send the operator to the wrong swap
  // page. The Live screen then uses the link the quote itself carries.
  const liveCorridor =
    !resumed && symbols && chainId
      ? { sellSymbol: symbols.quote, buySymbol: symbols.base, chainId }
      : null

  // Whether the Connect step may be left forwards. After an attempt, whatever
  // it answered: pending, declined and unreachable all still need a funded
  // wallet. And on a resumed run, where an earlier run may already have
  // connected this bot and only the wizard has forgotten — the Fund step's
  // Back must not lead somewhere with no way out. Before the first attempt of
  // a fresh run it stays closed, so Continue can't be used to slip past
  // connecting.
  const canLeaveConnect = accessStatus !== null || connectError !== null || resumed

  /**
   * Drop the add lane's own parameters. `?resume=` is left alone: it belongs to
   * the other lane and to another bot.
   */
  function clearLaneParams() {
    const next = new URLSearchParams(searchParams)
    for (const key of ['bot', 'corridor', 'chain', 'added', 'pool']) next.delete(key)
    setSearchParams(next, { replace: true })
  }

  /**
   * The picker's Next, which is now the only branch point in the wizard.
   *
   * It asks one question the operator never sees: is there already a bot on
   * this corridor's chain? If there isn't, nothing is said and the wizard runs
   * exactly as it always has. Custom corridors never come here: the add route
   * takes a catalog id, and there is no way to append a pasted config to a bot
   * that exists.
   */
  async function goNextFromPicker() {
    const picked = corridors?.find((c) => c.id === corridorId)
    if (!picked) {
      setStep(1)
      return
    }
    // A Fleet row's shortcut is a preference, not an instruction: the operator
    // can still pick a corridor on another network. It is also the bot to read
    // first, so a large fleet can't push it past the fan-out cap.
    const wanted = paramsRef.current.get('bot')
    setDetecting(true)
    try {
      // Under a clock. This used to be `setStep(1)`, instant and offline;
      // `/api/bots` and `/settings` both go through Docker with no server-side
      // budget behind them, and a wedged daemon would otherwise strand the
      // operator on step 1 with a dead button.
      const { rows, fleetNames } = await withTimeout((signal) =>
        loadCandidates(picked, null, { prefer: wanted, signal }),
      )
      // Nothing is said only when there is nothing to say: no bot at all on
      // this chain, which is the wizard as it has always been.
      //
      // Bots that exist and cannot take the corridor are NOT that case. They
      // are the reason this screen exists: the ordinary operator runs one bot
      // quoting one pair, clicks Add corridor on its Fleet row, picks the pair
      // they know, and every same-chain bot comes back blocked. Falling through
      // put them three steps from a second wallet, a second lot of capital and
      // a second Textile approval, having been told neither that their own bot
      // already quotes it nor that a separate bot is what they were building.
      // The screen states that in one line and offers the one live choice.
      // No bot on this chain at all, and no choice to make.
      if (rows.length === 0) {
        setStep(1)
        return
      }
      setCandidates(rows)
      // Only once the screen is actually shown. It drives the rail, so setting
      // it on the way past put a Where pill in the rail of a wizard that never
      // stops there.
      if (rows.some((c) => c.eligible)) setSawWhere(true)
      // Say once, by name, that the prefilled bot isn't on offer rather than
      // quietly preselecting someone else. Same three cases the prefill effect
      // separates: blocked, on another network, or not on this panel at all.
      const target = rows.find((c) => c.bot.name === wanted)
      setPlaceNotice(
        !wanted || target?.eligible
          ? null
          : target
            ? place.droppedBlocked(wanted, target.blocked ?? '')
            : fleetNames.includes(wanted)
              ? place.droppedOffChain(wanted, picked.displayName)
              : place.droppedUnknown(wanted),
      )
      setWhereChoice(defaultChoice(rows, wanted))
      // Same rule: no eligible bot means no choice to offer, so carry straight
      // on into the new-bot wizard instead of showing a dead screen.
      if (rows.some((c) => c.eligible)) setLane('where')
      else setStep(1)
    } catch {
      // A fleet the panel can't read, or one it can't read in time, is not a
      // reason to stop. Fall through to the wizard as it stands, and say
      // nothing about a choice that could not be offered.
      setStep(1)
    } finally {
      setDetecting(false)
    }
  }

  /** Leaving the Where screen. Null is the separate-bot row. */
  function chooseTarget(bot: string | null) {
    setPlaceNotice(null)
    if (bot === null) {
      setTargetBot(null)
      setAddedPoolIndex(null)
      setLane('new')
      setStep(1)
      clearLaneParams()
      return
    }
    setTargetBot(bot)
    setAddedPoolIndex(null)
    setShortStep(0)
    setLane('add')
    const next = new URLSearchParams(searchParams)
    next.set('bot', bot)
    next.set('corridor', corridorId)
    next.delete('added')
    next.delete('pool')
    setSearchParams(next, { replace: true })
  }

  // The rail. Nine pills against five is the only thing on screen telling the
  // operator that adding to a bot they already run is the short road, so the
  // sets are deliberately different lengths. A resumed new-bot run always gets
  // the eight-pill rail: that operator never saw a Where screen.
  const rail =
    sawWhere && !resumed
      ? { labels: WHERE_LABELS, current: step === 0 ? 0 : step + 1 }
      : { labels: LABELS, current: step }

  if (lane === 'where' && corridor) {
    return (
      <div className="space-y-4">
        <h1 className="text-xl font-bold">Add corridor</h1>
        {/* The rail follows the selection. Both sets open Corridor → Where, so
            the pill under the cursor doesn't move; what changes is how much
            road is drawn behind it. Recommending the short road under a
            nine-pill rail advertised the long one at the exact moment the
            operator was choosing between them. */}
        <Steps
          current={1}
          labels={whereChoice === null ? WHERE_LABELS : SHORT_LABELS}
        />
        <WhereStep
          corridor={corridor}
          candidates={candidates}
          selected={whereChoice}
          onSelect={setWhereChoice}
          notice={placeNotice}
          onChoose={chooseTarget}
          onBack={() => {
            setPlaceNotice(null)
            setLane('new')
            clearLaneParams()
          }}
        />
      </div>
    )
  }

  if (lane === 'add' && corridor && targetBot) {
    const ownFeedUrl =
      sources.feedMode === 'own' ? sources.feedUrl.trim() : null
    // A stopped bot is a legitimate target, and the add starts it rather than
    // restarting it. Different consequence, so a different sentence. Unknown
    // only if the lane was reached without a candidate scan, which it can't be.
    //
    // Three states, not two: a paused or restarting bot is neither quoting nor
    // stopped, and the add bounces it like a running one. The copy splits the
    // same way the flow's own restart banner does, so no screen says both "this
    // starts the bot" and "it is still running its old config".
    const target = candidates.find((c) => c.bot.name === targetBot)?.bot
    const targetRunning = target?.running ?? true
    const targetCanStop = target?.canStop ?? true
    const targetState = botRunState(targetRunning, targetCanStop)
    return (
      <div className="space-y-4">
        <h1 className="text-xl font-bold">Add corridor</h1>
        <Steps current={shortStep + 2} labels={SHORT_LABELS} />

        {/* Price feed. The feed is per pool, so this is still the operator's
            own choice; the RPC question is absent by construction, because the
            RPC belongs to the bot and the add route never writes one. */}
        {shortStep === 0 && (
          <Card title={addCopy.feedTitle}>
            <p className="text-sm text-muted">{addCopy.feedLead(targetBot)}</p>
            <div className="mt-4 space-y-5">
              <SourcePicker
                label="Price feed"
                hint="Where the bot reads the mid it quotes around."
                mode={sources.feedMode}
                defaultUrl={sources.feedDefault}
                url={sources.feedUrl}
                onMode={(feedMode) => setSources({ ...sources, feedMode })}
                onUrl={(feedUrl) => setSources({ ...sources, feedUrl })}
              />
            </div>
            <p className="mt-3 text-sm text-faint">
              {addCopy.feedRpcNote(targetBot)}
            </p>
            <div className="mt-4 flex justify-between">
              <Button onClick={() => setLane('where')}>{addCopy.back}</Button>
              <Button
                variant="primary"
                onClick={() => setShortStep(1)}
                disabled={
                  !sourceOk(sources.feedMode, sources.feedDefault, sources.feedUrl)
                }
              >
                Next
              </Button>
            </div>
          </Card>
        )}

        {shortStep === 1 && (
          <Card title={addCopy.spreadTitle}>
            <p className="text-sm text-muted">{addCopy.spreadNote(targetBot)}</p>
            <SpreadFields
              spreads={spreads}
              symbols={symbols}
              onChange={setSpreads}
            />
            <p className="mt-4 text-sm text-muted">
              {addCopy.spreadRestart(targetBot, targetState)}
            </p>
            <div className="mt-4 flex justify-between">
              <Button onClick={() => setShortStep(0)}>{addCopy.back}</Button>
              <Button
                variant="primary"
                onClick={() => setShortStep(2)}
                disabled={!spreadsOk(spreads)}
              >
                {addCopy.commit(targetBot)}
              </Button>
            </div>
          </Card>
        )}

        {shortStep === 2 && (
          <AddCorridorFlow
            bot={targetBot}
            corridor={corridor}
            botRunning={targetRunning}
            botCanStop={targetCanStop}
            spreads={spreads}
            ownFeedUrl={ownFeedUrl}
            addedPoolIndex={addedPoolIndex}
            onAdded={(poolIndex) => {
              setAddedPoolIndex(poolIndex)
              const next = new URLSearchParams(searchParams)
              next.set('added', '1')
              next.set('pool', String(poolIndex))
              setSearchParams(next, { replace: true })
            }}
            onBack={() => setShortStep(1)}
            onStartOver={startOver}
            botPath={botPath(targetBot)}
            onOpenBot={() => navigate(botPath(targetBot))}
          />
        )}
      </div>
    )
  }

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
        ...(isCustom
          ? customMode === 'toml'
            ? { toml: importedToml }
            : { custom: buildCustom(custom) }
          : { corridorId }),
        // Wizard overrides, applied to the template on the server before it is
        // written — one atomic create, never a half-configured bot.
        buySpreadBps: spreads.buy.value.trim(),
        sellSpreadBps: spreads.sell.value.trim(),
        ...(sources.rpcMode === 'own' ? { rpcUrl: sources.rpcUrl.trim() } : {}),
        ...(sources.feedMode === 'own' ? { feedUrl: sources.feedUrl.trim() } : {}),
        start: rfqDefault ? false : start,
        signer: buildSigner(signer),
      })
      // Clear the secret from component state the moment it's no longer needed.
      setSigner(emptySigner)
      // Don't leave for the bot page yet: the wizard connects the bot to
      // Textile itself next.
      setCreatedBot(res.bot.name)
      // Remembered from here on, not from the Fund step: the bot exists now, so
      // a reload or a crash on Connect has to come back to it rather than
      // restart the wizard and collide on the name.
      saveResume(res.bot.name)
      // The create note and needsPermit2Approval are deliberately dropped. Both
      // describe a bot that is created and not yet set up, which is only true
      // between here and the Fund step; the wizard now carries on through
      // approval and start, so nothing downstream may quote them.
      setStep(5)
    } catch (e) {
      setError(e instanceof ApiError ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  /**
   * One click: register the wallet with the venue (Connect), then file the
   * access request with the contact details. Two existing panel routes behind
   * one button. If the request fails after a successful enroll, the enrollment
   * is kept and Retry runs both again — enroll is a reconnect, so it's safe.
   */
  async function connect() {
    if (!createdBot) return
    setConnecting(true)
    setConnectError(null)
    try {
      const enrolled = await api.enrollRfq(createdBot)
      setEnrollment(enrolled.enrollment ?? null)
      if (enrolled.accessStatus) setAccessStatus(enrolled.accessStatus)
      const access = await api.requestRfqAccess(createdBot, {
        contactEmail: contact.email.trim() || undefined,
        contactWhatsapp: contact.whatsapp.trim() || undefined,
      })
      setAccessStatus(access.accessStatus)
      setAccessMessage(access.message)
      if (access.enrollment) setEnrollment(access.enrollment)
    } catch (e) {
      setConnectError(e instanceof ApiError ? e.message : String(e))
    } finally {
      setConnecting(false)
    }
  }

  /**
   * Leave the wizard for the bot's page. Only the Live screen offers this, so
   * by the time it runs the wizard has approved Permit2, started the bot and
   * seen it quote.
   *
   * It carries no create-time state on purpose. `needsPermit2` is hardcoded
   * true by the create endpoint and the create note says the bot is
   * unconnected and not started; both were written for the old flow, which
   * left from Connect. Passing them on now would greet an operator who just
   * watched their own stitch quote with "Permit2 approval required" on the
   * Tools tab.
   */
  function finish() {
    if (!createdBot) return
    navigate(botPath(createdBot), { state: { note: null, needsPermit2: false } })
  }

  /** On from Connect to funding, remembering the bot in case the tab closes. */
  function toFunding() {
    if (!createdBot) return
    saveResume(createdBot)
    setStep(6)
  }

  /**
   * Forget the bot the Fund step was watching and start the wizard again.
   *
   * Two ways here: that bot is gone (deleted elsewhere, or a stale resume
   * record), or the run was resumed into funding and the operator wants to set
   * up a different bot instead. Neither is a way out of the flow: the resumed
   * bot keeps its wallet, its money and its Textile request, and the wizard
   * starts over from the corridor step rather than dropping anyone on a
   * settings page half way through.
   *
   * A third way in: the add lane's enrolment kept failing. Both records go, or
   * the one this page reads on the way in would pull the operator straight back
   * into the lane they just left, on every later visit to /add, for a day.
   */
  function startOver() {
    clearResume()
    clearAddResume()
    const next = new URLSearchParams(searchParams)
    // The add lane's parameters go too. Without that, Start over silently
    // re-branched into the same bot on the next pass through the picker.
    for (const key of ['resume', 'bot', 'corridor', 'chain', 'added', 'pool']) {
      next.delete(key)
    }
    setSearchParams(next, { replace: true })
    setLane('new')
    setTargetBot(null)
    setCandidates([])
    setSawWhere(false)
    setShortStep(0)
    setPlaceNotice(null)
    setWhereChoice(null)
    setAddedPoolIndex(null)
    setCreatedBot(null)
    setResumed(false)
    setFundOutcome(null)
    setEnrollment(null)
    setAccessStatus(null)
    setAccessMessage(null)
    setConnectError(null)
    setStep(0)
  }

  return (
    <div className="space-y-4">
      <h1 className="text-xl font-bold">Add corridor</h1>
      {/* Live is the last label even while the Waiting screen is up: waiting is
          a state on the way there, not a destination of its own. */}
      <Steps current={rail.current} labels={rail.labels} />

      {/* Why a bot named in a Fleet row's link isn't being added to. The Where
          screen shows this itself, but it is skipped when no bot can take the
          corridor, and that is exactly the case an operator who clicked "Add
          corridor" on a specific bot needs told. */}
      {placeNotice && (
        <Banner tone="info" onDismiss={() => setPlaceNotice(null)}>
          {placeNotice}
        </Banner>
      )}

      {step === 0 && !editingCustom && (
        <Card title="Which corridor should it quote?">
          {catalogWarning && (
            <div className="mb-3">
              <Banner tone="warning">{catalogWarning}</Banner>
            </div>
          )}
          {/* Network on the left, that network's tokens on the right. Two picks
              in either order resolve to one corridor id; the picker reports ''
              while the pair is incomplete, so Next stays off. Its Custom row
              opens the details form in one click and leaves corridorId on the
              sentinel, so the row stays highlighted after Back and Next
              reopens the form. */}
          <CorridorPicker
            corridors={corridors}
            value={corridorId}
            onChange={setCorridorId}
            customSelected={isCustom}
            onCustom={() => {
              setCorridorId(CUSTOM)
              setEditingCustom(true)
            }}
          />
          <div className="mt-4 flex items-center justify-between">
            <button
              className="text-xs text-muted underline disabled:opacity-40"
              onClick={() => setShowTemplate(!showTemplate)}
              disabled={!corridor}
            >
              {showTemplate ? 'Hide' : 'Show'} the config this writes
            </button>
            {/* Next is async now: before the wizard commits to setting up a
                new bot it asks the fleet whether one already runs on this
                chain. Typically a couple of hundred milliseconds, and the
                button shows its own spinner. */}
            <Button
              variant="primary"
              busy={detecting}
              onClick={() =>
                isCustom ? setEditingCustom(true) : void goNextFromPicker()
              }
              disabled={!corridorId || detecting}
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
            <div className="flex gap-2">
              <button
                type="button"
                onClick={() => setCustomMode('fields')}
                className={`rounded-full px-3 py-1 text-sm ${
                  customMode === 'fields'
                    ? 'bg-accent text-on-accent'
                    : 'bg-hover text-muted'
                }`}
              >
                Enter the fields
              </button>
              <button
                type="button"
                onClick={() => setCustomMode('toml')}
                className={`rounded-full px-3 py-1 text-sm ${
                  customMode === 'toml'
                    ? 'bg-accent text-on-accent'
                    : 'bg-hover text-muted'
                }`}
              >
                Import stitch.toml
              </button>
            </div>

            {customMode === 'toml' ? (
              <TomlImport
                value={importedToml}
                error={importError}
                onChange={(next) => {
                  setImportedToml(next)
                  setImportError(null)
                }}
                onError={setImportError}
                onBack={() => setEditingCustom(false)}
                onNext={() => setStep(1)}
              />
            ) : (
              <>
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
              </>
            )}
          </div>
        </Card>
      )}

      {step === 1 && (
        <Card title="Spreads">
          <SpreadFields spreads={spreads} symbols={symbols} onChange={setSpreads} />
          <div className="mt-4 flex justify-between">
            <Button onClick={() => setStep(0)}>Back</Button>
            <Button
              variant="primary"
              onClick={() => setStep(2)}
              disabled={!spreadsOk(spreads)}
            >
              Next
            </Button>
          </div>
        </Card>
      )}

      {step === 2 && (
        <Card title="Price feed and RPC">
          <div className="mt-4 space-y-5">
            <SourcePicker
              label="Price feed"
              hint="Where the bot reads the mid it quotes around."
              mode={sources.feedMode}
              defaultUrl={sources.feedDefault}
              url={sources.feedUrl}
              onMode={(feedMode) => setSources({ ...sources, feedMode })}
              onUrl={(feedUrl) => setSources({ ...sources, feedUrl })}
            />
            <SourcePicker
              label="RPC"
              hint="Where the bot reads chain state and sends transactions."
              mode={sources.rpcMode}
              defaultUrl={sources.rpcDefault}
              url={sources.rpcUrl}
              onMode={(rpcMode) => setSources({ ...sources, rpcMode })}
              onUrl={(rpcUrl) => setSources({ ...sources, rpcUrl })}
            />
          </div>
          <div className="mt-4 flex justify-between">
            <Button onClick={() => setStep(1)}>Back</Button>
            <Button
              variant="primary"
              onClick={() => setStep(3)}
              disabled={!sourcesOk(sources)}
            >
              Next
            </Button>
          </div>
        </Card>
      )}

      {step === 3 && (
        <Card title="Name it">
          <div className="space-y-4">
            <Field
              label="Bot name"
              hint={
                botNameProblem(name.trim()) ??
                "Lowercase letters, digits and single hyphens. This becomes the config directory and part of the container name, so it can't be changed later without recreating the bot."
              }
            >
              <Input
                value={name}
                autoFocus
                placeholder="bot-a"
                onChange={(e) => setName(e.target.value)}
              />
            </Field>
            <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
              <Field
                label="Contact email"
                hint={
                  contact.email.trim() !== '' && !isEmail(contact.email)
                    ? 'That does not look like an email address.'
                    : 'Required. Used for the venue access request, so Textile can reach you.'
                }
              >
                <Input
                  value={contact.email}
                  inputMode="email"
                  placeholder="you@example.com"
                  onChange={(e) => setContact({ ...contact, email: e.target.value })}
                />
              </Field>
              <Field label="WhatsApp (optional)">
                <Input
                  value={contact.whatsapp}
                  inputMode="tel"
                  placeholder="+234…"
                  onChange={(e) =>
                    setContact({ ...contact, whatsapp: e.target.value })
                  }
                />
              </Field>
            </div>
            <div className="flex justify-between">
              <Button onClick={() => setStep(2)}>Back</Button>
              <Button
                variant="primary"
                onClick={() => setStep(4)}
                disabled={
                  name.trim().length === 0 ||
                  botNameProblem(name.trim()) !== null ||
                  !isEmail(contact.email)
                }
              >
                Next
              </Button>
            </div>
          </div>
        </Card>
      )}

      {step === 4 && (
        <Card title="Set up the operator wallet">
          <div className="space-y-4">
            <SignerFields value={signer} onChange={setSigner} />

            <SignerConflictWarning chainId={chainId} signer={signer} />

            {rfqDefault ? (
              <p className="text-xs text-faint">
                Created stopped. Next, the wizard connects it to Textile. It
                starts quoting once Textile approves the request and the
                wallet is funded.
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
              <Button onClick={() => setStep(3)}>Back</Button>
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

      {step === 5 && (
        <Card title="Connect to Textile">
          <div className="space-y-4">
            <p className="text-sm text-muted">
              One click. Registers this bot&apos;s funding wallet with the venue
              and saves the credential, then files your access request with the
              contact details you gave. You never paste an id or key. Textile
              still has to approve the request before the bot receives Swap
              quotes.
            </p>
            {createdBot && (
              <p className="text-xs text-faint">
                Bot <span className="font-mono">{createdBot}</span> is created.
                It quotes nothing until it is connected and approved.
              </p>
            )}
            {enrollment && (
              <Banner tone={enrollment.flagged ? 'warning' : 'success'}>
                Connected as{' '}
                <span className="font-mono">{enrollment.makerSlug}</span>
                {enrollment.corridors.length > 0
                  ? `. Approved on: ${enrollment.corridors.join(', ')}.`
                  : '. Textile assigns the corridor when it approves your request.'}
                {enrollment.flagged
                  ? '. This maker is flagged: no Swap quotes until Textile unflags you.'
                  : ''}
              </Banner>
            )}
            {accessStatus && (
              <Banner tone={accessTone(accessStatus)}>
                {accessCopy(accessStatus)}
                {accessMessage ? ` ${accessMessage}` : ''}
              </Banner>
            )}
            {connectError && <Banner tone="danger">{connectError}</Banner>}
            {/* The request needs an address to answer to. Normally the Name
                step has it; on a run resumed straight into funding it is asked
                for here, so this step still works instead of filing a request
                with no email and being refused every time. */}
            {!isEmail(contact.email) && (
              <Field
                label="Contact email"
                hint={
                  contact.email.trim() !== ''
                    ? 'That does not look like an email address.'
                    : 'Required. Textile answers the access request here.'
                }
              >
                <Input
                  value={contact.email}
                  inputMode="email"
                  placeholder="you@example.com"
                  onChange={(e) => setContact({ ...contact, email: e.target.value })}
                />
              </Field>
            )}
            {/* Forward only. Once the request has been tried, Continue goes to
                funding whatever came back: pending, declined, or the venue
                unreachable, the wallet still has to be funded and the waiting
                screen deals with the answer. Continue is not offered before
                the first attempt, so it can't be used to slip past Connect.
                There is no "connect later": the wizard ends at a running bot
                or at one waiting for Textile. */}
            <div className="flex items-center justify-end gap-3">
              {(!accessStatus || connectError) && (
                <Button
                  variant="primary"
                  busy={connecting}
                  disabled={!isEmail(contact.email)}
                  onClick={() => void connect()}
                >
                  {connectError ? 'Retry' : 'Connect to Textile'}
                </Button>
              )}
              {createdBot && canLeaveConnect && (
                <Button
                  variant={connectError ? 'secondary' : 'primary'}
                  onClick={toFunding}
                >
                  Continue
                </Button>
              )}
            </div>
          </div>
        </Card>
      )}

      {/* Fund. Watches the wallet, then approves spending and starts the bot
          on its own. It reports how that ended and the wizard moves on. */}
      {step === 6 && createdBot && (
        <FundStep
          bot={createdBot}
          onStarted={(outcome) => {
            setFundOutcome(outcome)
            setStep(7)
          }}
          onBack={() => setStep(5)}
          onStartOver={startOver}
        />
      )}

      {/* The last step, in one of two states. Running: the Live screen, which
          proves it with a real quote and is the only screen with a way out.
          Not running yet: the Waiting screen, which polls Textile and starts
          the bot itself the moment the answer is yes. */}
      {step === 7 &&
        createdBot &&
        (fundOutcome?.kind === 'live' ? (
          <LiveStep
            bot={createdBot}
            corridor={liveCorridor}
            botPath={botPath(createdBot)}
            onOpenBot={finish}
          />
        ) : (
          <ApprovalWait
            bot={createdBot}
            initial={outcomeAccess(fundOutcome)}
            initialError={outcomeError(fundOutcome)}
            contact={contact}
            onApproved={() => setFundOutcome({ kind: 'live' })}
            onBack={() => setStep(5)}
          />
        ))}
    </div>
  )
}

/** The access result a Fund outcome carries, when it carries one. */
function outcomeAccess(outcome: FundOutcome | null): RfqAccessResult | null {
  if (!outcome || outcome.kind === 'live') return null
  return outcome.access
}

/**
 * Why the Fund step has no access result: the panel could not reach Textile.
 * Without this the Waiting screen opens on "Textile still has to approve this
 * maker by hand" when the truth is that nobody asked Textile anything.
 */
function outcomeError(outcome: FundOutcome | null): string | null {
  return outcome?.kind === 'waiting' ? outcome.error : null
}

const MAX_TOML_BYTES = 64 * 1024

function chainIdFromToml(toml: string): number | undefined {
  const match = toml.match(/^\s*chain_id\s*=\s*(\d+)/m)
  if (!match) return undefined
  const value = Number(match[1])
  return Number.isInteger(value) && value > 0 ? value : undefined
}

function TomlImport({
  value,
  error,
  onChange,
  onError,
  onBack,
  onNext,
}: {
  value: string
  error: string | null
  onChange: (next: string) => void
  onError: (message: string | null) => void
  onBack: () => void
  onNext: () => void
}) {
  const ready = value.trim().length > 0 && value.length <= MAX_TOML_BYTES

  const onFile = async (file: File | undefined) => {
    if (!file) return
    if (file.size > MAX_TOML_BYTES) {
      onError(`File is larger than ${MAX_TOML_BYTES / 1024} KiB`)
      return
    }
    onChange(await file.text())
  }

  return (
    <>
      <p className="text-sm text-muted">
        Paste a stitch.toml from the corridor admin (or a shipped preset). The
        file is validated on the server before anything is written. It must not
        include a [signer] section — the next step collects the wallet.
      </p>
      <Field label="File">
        <input
          type="file"
          accept=".toml,text/plain,application/toml"
          className="text-sm"
          onChange={(event) => void onFile(event.target.files?.[0])}
        />
      </Field>
      <Field
        label="stitch.toml"
        hint="Or paste the file contents here."
      >
        <textarea
          value={value}
          rows={16}
          spellCheck={false}
          placeholder={'chain_id = 42220\nrpc_url = "https://…"\n…'}
          onChange={(event) => onChange(event.target.value)}
          className="w-full rounded-lg border border-line-soft bg-canvas p-3 font-mono text-xs leading-relaxed"
        />
      </Field>
      {error && <Banner tone="danger">{error}</Banner>}
      <div className="flex justify-between">
        <Button onClick={onBack}>Back</Button>
        <Button variant="primary" onClick={onNext} disabled={!ready}>
          Next
        </Button>
      </div>
    </>
  )
}

/**
 * The corridor template's defaults, read the same way `chainIdFromToml` does:
 * a regex over the toml the wizard is about to write. Values may carry a
 * trailing `# comment`, which the patterns stop before. First pool only —
 * a fresh corridor has exactly one.
 */
function defaultsFromToml(toml: string): {
  buyBps?: string
  sellBps?: string
  rpcUrl?: string
  feedUrl?: string
} {
  const num = (key: string) =>
    toml.match(new RegExp(`^\\s*${key}\\s*=\\s*([0-9]+(?:\\.[0-9]+)?)`, 'm'))?.[1]
  const str = (key: string, from = 0) =>
    toml.slice(from).match(new RegExp(`^\\s*${key}\\s*=\\s*"([^"]+)"`, 'm'))?.[1]
  // [feed].url is the first `url =` after the [feed] header.
  const feedAt = toml.search(/^\[feed\]/m)
  return {
    buyBps: num('buy_offset_bps'),
    sellBps: num('sell_offset_bps'),
    rpcUrl: str('rpc_url'),
    feedUrl: feedAt >= 0 ? str('url', feedAt) : undefined,
  }
}

function sourcesOk(s: SourcesState): boolean {
  return (
    sourceOk(s.feedMode, s.feedDefault, s.feedUrl) &&
    sourceOk(s.rpcMode, s.rpcDefault, s.rpcUrl)
  )
}

function accessTone(
  status: RfqAccessStatus,
): 'info' | 'success' | 'warning' | 'danger' {
  switch (status) {
    case 'APPROVED':
      return 'success'
    case 'PENDING':
      return 'info'
    case 'REJECTED':
      return 'danger'
    default:
      return 'warning'
  }
}

function accessCopy(status: RfqAccessStatus): string {
  switch (status) {
    case 'APPROVED':
      return 'Access approved: this bot will receive Swap quotes.'
    case 'PENDING':
      return 'Access requested. Textile is reviewing it; the bot goes live once approved.'
    case 'REJECTED':
      return 'Access was declined. Contact Textile before continuing.'
    default:
      return 'Access not requested yet.'
  }
}
