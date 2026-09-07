// Every operator-facing sentence on the wizard's Fund, Waiting and Live
// screens, in one file so the words can be reviewed without reading the
// components.
//
// House rules: plain words, short sentences, no em dashes, never a promise of
// income or of how fast Textile approves anyone. "Permit2" appears once, in
// parentheses. Server strings are shown verbatim only inside banners.

export const CONTACT_EMAIL = 'contact@textilecredit.com'
export const TELEGRAM = 't.me/TextileNigeria'
export const PUBLIC_SWAP_BASE = 'https://app.textilecredit.com/s/swap'

/** `USDT or cNGN`, `USDT, cNGN or wBRL`. */
export function joinOr(items: string[]): string {
  return joinWith(items, 'or')
}

/** `USDT and cNGN`. */
export function joinAnd(items: string[]): string {
  return joinWith(items, 'and')
}

function joinWith(items: string[], word: string): string {
  if (items.length === 0) return ''
  if (items.length === 1) return items[0] ?? ''
  return `${items.slice(0, -1).join(', ')} ${word} ${items[items.length - 1] ?? ''}`
}

/** A dollar floor for prose: `$20`, `$1`, `$0.50`. */
export function dollars(n: number): string {
  return Number.isInteger(n) ? `$${n}` : `$${n.toFixed(2)}`
}

/**
 * What adding a corridor does to the bot it is added to, in the three states
 * the panel can actually see it in.
 *
 * `running` alone is not enough to write the sentence. The fleet reports
 * `running` for a bot that is quoting and `canStop` for one with a live process
 * at all, and a `paused` or `restarting` bot is neither: it is not quoting, and
 * it is not stopped. The add bounces it exactly like a running one, and the
 * server reports that bounce as `restarted: false` with no error, which is what
 * puts the add flow's "still running its old config" banner on screen. Told
 * only `running`, the copy said "this starts the bot" on the same screen as
 * that banner. Three states, three sentences, no screen saying both.
 */
export type BotRunState = 'quoting' | 'live-but-not-quoting' | 'stopped'

/** The fleet's two booleans, as the one state the copy splits on. */
export function botRunState(running: boolean, canStop: boolean): BotRunState {
  if (running) return 'quoting'
  return canStop ? 'live-but-not-quoting' : 'stopped'
}

export const fund = {
  title: 'Fund the bot',
  intro: (minToken: number, minGas: number) =>
    `Send money to the bot's wallet from any wallet or exchange. It starts by itself once one token reaches ${dollars(minToken)} and there is ${dollars(minGas)} of gas. Nothing to press.`,
  addressLabel: (network: string) => `Bot wallet on ${network}`,
  copyAddress: 'Copy address',
  copied: 'Copied',
  copyFailed: "Couldn't copy. Select the address and copy it by hand.",
  viewOn: (host: string) => `View on ${host}`,
  chainWarning: (network: string) =>
    `Send on ${network} only. Money sent on another network does not arrive.`,
  noOperator:
    'This bot has no wallet address the panel can read, so there is nothing to fund here.',

  stableHint: (min: number, stable: string, soft: string) =>
    `${dollars(min)} of ${stable} lets the bot buy ${soft}.`,
  softHint: (min: number, soft: string) =>
    `${dollars(min)} of ${soft} lets the bot sell ${soft}.`,
  gasHint: (min: number, amount: string | null, gas: string) =>
    amount
      ? `Needs ${dollars(min)}, about ${amount} ${gas}. Pays for the approvals.`
      : `Needs ${dollars(min)} of ${gas}. Pays for the approvals.`,
  gasHintUnpriced: (gas: string) =>
    `Any ${gas} balance counts on this chain. Pays for the approvals.`,
  fundedHint: 'Enough to start.',
  gasOkHint: 'Enough.',
  estimated: '(estimated)',

  pill: {
    funded: 'Funded',
    empty: 'Empty',
    low: 'Not enough',
    addGas: 'Add gas',
    ok: 'OK',
    reading: 'Reading',
    unknown: 'Unknown',
  },

  gate: (min: number, symbols: string[], minGas: number, gas: string) =>
    `Ready when the wallet holds ${dollars(min)} of ${joinOr(symbols)}, plus ${dollars(minGas)} of ${gas} for gas.`,
  needsSide: (min: number, symbols: string[]) =>
    `Add at least ${dollars(min)} of ${joinOr(symbols)} to the bot wallet.`,
  needsGas: (minGas: number, gas: string) => `Add at least ${dollars(minGas)} of ${gas} for gas.`,
  cantPrice: (symbol: string) =>
    `Can't price ${symbol} right now, so its balance doesn't count yet.`,
  cantRead: "Can't read the bot wallet on chain right now.",
  /** The permanent case, in the gate's own bullet list. Not "add money": no
      amount of either token changes this answer. */
  unpriceablePair: (symbols: string[]) =>
    `The panel can't say what ${joinAnd(symbols)} are worth in dollars, so it can't check this wallet. Sending money here does not change that.`,
  gasUnpriced: "Can't price gas on this chain, so any non-zero balance counts.",

  status: (seconds: number, time: string) =>
    `Checking every ${seconds} seconds. Last check ${time}.`,
  statusFirst: 'Checking the wallet.',
  checkNow: 'Check now',
  readError: (error: string) => `Can't read the wallet right now: ${error}. Still trying.`,
  /** The row's own note. `reason` is the server's, shown as it wrote it: it
      knows whether the feed is down or the pair simply cannot be valued, and
      an invented "trying again" once promised a retry that could not work. */
  priceError: (symbol: string, reason: string | null) =>
    reason ? `Can't price ${symbol}: ${reason}.` : `Can't price ${symbol} right now.`,
  fundsFound: 'Funds found. Setting up the bot.',
  unreadable: (error: string) => `The panel can't read this bot's config: ${error}`,

  unpriceableTitle: "This pair can't be set up here",
  unpriceable: (symbols: string[]) =>
    `The panel prices a pair against dollars, and it has no dollar price for ${joinAnd(symbols)}. It can't tell whether the wallet holds enough to trade, so it can't start the bot on this pair.`,
  unpriceableNext:
    'Set up a bot on a pair with USDT or USDC in it. This bot keeps its wallet and anything already sent to it.',

  gone: 'This bot no longer exists. Go back and create it again.',
  startOver: 'Start over',
  differentBot: 'Set up a different bot',
  retry: 'Retry',
  back: 'Back',
}

/**
 * The Where step: which bot should quote the corridor the operator just picked.
 *
 * It only appears when they already run a bot on that chain, and it is written
 * in outcomes rather than in the word "bot", because someone who clicked Add
 * corridor did not come here to think about processes. One vocabulary rule
 * throughout: the corridor is named ("cNGN / USDT"), and "corridor" is the only
 * generic noun. Never "pair".
 */
export const place = {
  title: (label: string) => `Where should ${label} run?`,

  /**
   * The recommendation, as a label that does not move.
   *
   * It sits above the rows rather than on the selected one. Tied to the radio,
   * the only screen that recommended the long road was the one where the short
   * one had just been ruled out for the bot the operator named, which is the
   * screen that needs the recommendation most.
   */
  recommendedHeading: 'Recommended',
  recommendedWhy:
    'Adding it to an existing bot means both corridors will share the liquidity in the pool (same USDT can be traded against 2 different assets)',

  addTo: (bot: string) => `Add to ${bot}`,
  /** One, two or three corridors by name; a count above that. */
  quotes: (pairs: string[]) =>
    pairs.length === 0
      ? 'Quotes nothing yet.'
      : pairs.length <= 3
        ? `Quotes ${joinAnd(pairs)}.`
        : `Quotes ${pairs.length} corridors.`,
  wallet: (short: string) => `Wallet ${short}.`,
  /** Filled in when the background balance read lands. Absent if it doesn't. */
  holds: (amounts: string[]) => `Holds ${joinAnd(amounts)}.`,

  /** The two things that are actually true of sharing a bot. Both real. */
  sharedWallet:
    'The corridors on one bot share its wallet. The same money backs all of them, so a trade on one leaves less for the others.',
  /**
   * A stopped bot is an eligible target, and on that one the restart sentence
   * is false twice over: nothing restarts, and nothing was quoting. What
   * happens instead is bigger, so it gets said: the flow starts that bot, and
   * every corridor already on it goes back on the book.
   *
   * A paused or restarting bot is the third case. It is bounced like a running
   * one, so it must not be promised the stopped bot's sentence.
   */
  restarts: (bot: string | null, state: BotRunState) => {
    const name = bot ?? 'the bot'
    if (state === 'quoting')
      return `Adding a corridor restarts ${name}. Its other corridors stop quoting for a few seconds.`
    if (state === 'live-but-not-quoting')
      return `${name} is not quoting right now. Adding a corridor restarts it, and its corridors go back on the book when it comes up.`
    return `Adding a corridor starts ${name}. Its other corridors start quoting again too.`
  },
  /**
   * The same consequence with no row selected yet.
   *
   * `restarts` needs a bot and the state that bot is in, and this screen can
   * open with nothing selected: a prefilled bot that turned out to be blocked
   * leaves the radio on the separate-bot row. The consequence still belongs on
   * the screen, so it is said about whichever bot they end up picking.
   */
  restartsAny:
    'Adding a corridor restarts the bot you put it on, so its other corridors stop quoting for a few seconds. A bot that is stopped is started instead.',

  separateTitle: 'Set up a separate bot',
  separateBody:
    'Its own wallet and its own money. You fund it, and Textile approves it again. Takes longer.',

  blockedAlready: (label: string) => `Already quoting ${label}.`,
  blockedNotEditable:
    "The panel can't edit this bot's config, so it can't take another corridor.",
  blockedNotConnected:
    'Not connected to Textile yet. Open it from the fleet and finish setting it up.',
  /** Connected, credential on disk, access request still unanswered. */
  blockedWaitingTextile:
    'Connected, waiting for Textile to approve this maker. It can take the corridor once they do.',
  blockedUnreadable: "The panel can't read this bot's settings right now.",
  blockedUnchecked:
    'Not checked. The panel checks the first few bots on a network and stops there.',
  showBlocked: (count: number, label: string) =>
    `Show ${count} ${count === 1 ? 'bot' : 'bots'} that can't take ${label}`,
  hideBlocked: 'Hide them',
  allBlocked: (label: string) =>
    `None of the bots you run here can take ${label}, so this one needs a bot of its own.`,
  /**
   * The all-blocked screen when the reason is that it is already quoted.
   *
   * Never `allBlocked` here: the corridor does not need a bot of its own, it has
   * one. Told otherwise, an operator who came to add the pair they already quote
   * was pointed at a second wallet, a second lot of money and a second wait on
   * Textile, for a corridor that was live the whole time.
   */
  allBlockedAlready: (bots: string[], label: string) =>
    bots.length === 1
      ? `${bots[0]} already quotes ${label}, so there is nothing to add to it.`
      : `${joinAnd(bots)} already quote ${label}, so there is nothing to add to them.`,
  /** What is left to do about a corridor that is already on a bot. */
  alreadyNext: (label: string) =>
    `To change the spreads or the price feed for ${label}, open the bot that quotes it and edit the corridor in Settings. A separate bot below would be a second wallet quoting ${label}, with its own money behind it.`,
  openBot: (bot: string) => `Open ${bot}`,
  /** Same screen, but some bots were never checked, so "none of them" would be a
      claim about bots nobody looked at. */
  allBlockedPartial: (label: string) =>
    `None of the bots the panel checked can take ${label}. It stops after the first few, and the rest are below. To add it to one of those, find that bot on the fleet page and press Add corridor on its row.`,

  /** A prefilled target that did not survive the pick, said in one line. */
  droppedUnknown: (bot: string) => `There is no bot called ${bot} on this panel.`,
  droppedOffChain: (bot: string, label: string) =>
    `${bot} runs on another network, so it can't quote ${label}.`,
  droppedBlocked: (bot: string, reason: string) => `${bot}: ${reason}`,

  next: 'Next',
  back: 'Back',
}

/** Adding the picked corridor to a bot that already exists. */
export const add = {
  feedTitle: 'Price feed',
  feedLead: (bot: string) =>
    `This corridor gets its own feed. The other corridors on ${bot} keep theirs.`,
  /** Where the RPC question would have been. It belongs to the bot. */
  feedRpcNote: (bot: string) =>
    `It reads the chain through ${bot}'s RPC. One bot, one network.`,

  spreadTitle: 'Spreads',
  spreadNote: (bot: string) =>
    `Per corridor. ${bot}'s other corridors keep the spreads they have.`,
  /** Same split as `place.restarts`: a stopped bot is started, not bounced. */
  spreadRestart: (bot: string, state: BotRunState) => {
    if (state === 'quoting')
      return `This restarts ${bot}. Its other corridors stop quoting for a few seconds.`
    if (state === 'live-but-not-quoting')
      return `${bot} is not quoting right now. This restarts it, and its other corridors come back with it.`
    return `This starts ${bot}. Its other corridors start quoting again too.`
  },
  commit: (bot: string) => `Add to ${bot}`,

  workTitle: (label: string, bot: string) => `Adding ${label} to ${bot}`,
  addRow: (label: string, bot: string) => `Add ${label} to ${bot}`,
  addRowSub: (state: BotRunState) => {
    if (state === 'quoting')
      return 'The bot restarts. Its other corridors stop quoting for a few seconds.'
    if (state === 'live-but-not-quoting')
      return 'The bot restarts. It is not quoting right now, and its other corridors come back with it.'
    return 'The bot starts. Its other corridors start quoting again too.'
  },
  saveRow: 'Save your spread and price feed',
  saveRowSub: 'Another restart, for the same reason.',
  enrollRow: (bot: string, label: string) => `Tell Textile ${bot} quotes ${label}`,
  enrollRowSub:
    'Textile already approved this maker. This adds the new corridor to it.',

  addFailed: (bot: string) => `${bot} is still quoting what it was.`,
  /**
   * Only for a bot with a live process that kept its old config: a restart that
   * failed, or one paused mid-tick. A stopped bot is started by this flow, so
   * it never sees this line.
   */
  notRestarted: (bot: string) =>
    `It is still running its old config, so the new corridor is not quoted yet. Open ${bot} from the fleet and restart it.`,
  saveFailed: (label: string) =>
    `${label} is on the bot, with the spreads and feed Textile ships for it. Your changes did not save.`,
  enrollFailed: (message: string, bot: string) =>
    `Textile did not take the new corridor: ${message}. It is on ${bot}, and it will not be quoted until this goes through.`,
  /**
   * The way out of a Retry that keeps failing. The corridor is on disk either
   * way, so leaving costs nothing that staying would save, and staying with a
   * venue that is not answering costs the operator every other thing they came
   * to the panel to do: the record this lane leaves behind reopens it on every
   * later visit until it is cleared.
   */
  enrollLeave: (bot: string) =>
    `You can leave this and come back to it. Open ${bot} from the fleet and press Reconnect to Textile to try again.`,
  enrollStartOver: 'Set up something else',
  fundingUnreadable: (message: string) =>
    `Can't read the bot wallet right now: ${message}. Still trying.`,

  /**
   * The corridor is live on one side only. Not a failure and not a warning to
   * dismiss: it is what the wallet holds, said once, where the operator can act
   * on it.
   */
  oneSided: (bot: string, soft: string, stable: string) =>
    `${bot} can buy ${soft} with the ${stable} already there. Send ${soft} to the same wallet if you also want it to sell.`,

  /**
   * The corridor is written and enrolled, and the panel has no dollar price for
   * either of its sides, so the funding check can never pass. A property of the
   * corridor, not of the wallet: asking for money here would ask for money that
   * cannot help. The Fund step ends the same way for a new bot.
   */
  unpriceableTitle: "This corridor can't be checked here",
  unpriceable: (label: string, bot: string, symbols: string[]) =>
    `${label} is on ${bot} and Textile has it. The panel checks a wallet by pricing it in dollars, and it has no dollar price for ${joinAnd(symbols)}, so it can't tell whether there is enough behind this corridor to quote it. Sending money does not change that.`,
  unpriceableNext: (bot: string) =>
    `Open ${bot} from the fleet to start it and watch its logs. Its other corridors are not affected. To take this one off again, use Corridors on the bot page.`,

  retry: 'Retry',
  back: 'Back',
}

export const progress = {
  /** `state` is the row's progress state; the chain is still being read while `running` with no symbols. */
  approveTitle: (symbols: string[], state: string) =>
    symbols.length > 0
      ? `Approve spending for ${joinAnd(symbols)}`
      : state === 'running'
        ? 'Check spending approval'
        : 'Spending already approved',
  approveSub:
    "Lets Textile's swap contract (Permit2) move the bot's tokens. The bot quotes both sides of the pair, so both tokens are approved, with one small gas fee each. Usually under a minute, up to two.",
  accessTitle: 'Check your Textile access',
  startTitle: 'Start the bot',
  verifyTitle: 'Make sure it stays up',
  verifySub: 'About ten seconds.',
  showOutput: 'Show output',
  hideOutput: 'Hide output',

  approveFailed: (message: string) => `Approval failed: ${message}.`,
  approveFailedHint: (gas: string) =>
    `If it says insufficient funds, add a little more ${gas} and press Retry. Retry checks the chain first, so a transaction that already landed is not sent twice.`,
  approveBlocked: 'Approval is refused while another process could send from this wallet.',
  stopBot: (name: string) => `Stop ${name}`,
  walletBusy: 'Another approval is still finishing on this wallet. Waiting for it.',
  approveStillBusy: 'Another approval is still running on this wallet after two minutes.',
  approvalDidNotLand: 'the approval did not land',
  startFailed: (message: string) => `The bot didn't start: ${message}.`,
  startBusy: 'The bot wallet is busy with another action. Trying again in 5 seconds.',
  startStayedBusy: 'The bot wallet stayed busy for a minute.',
  verifyFailed: 'The bot stopped right after starting.',
  verifyUnconfirmed: (message: string) => `Couldn't confirm the bot is running: ${message}.`,
  seeLogs: 'See logs',
  recreate:
    "The bot's container is gone. Press Retry. If it keeps failing, delete this bot from the fleet and set it up again.",
  retry: 'Retry',
}

export const wait = {
  title: 'Waiting for Textile',
  body: 'Money is in and spending is approved. Textile still has to approve this maker by hand. Nothing more to do on your side.',
  keepOpen:
    'Keep this page open. It starts the bot the moment Textile says yes. If you close it, open the bot page after they approve you and press Start.',
  emailVerify:
    'Textile sent you a confirmation email. Click the link in it, or the request stays unverified.',
  status: (time: string, seconds: number) =>
    `Last checked ${time}. Checks again in ${seconds} seconds.`,
  statusFirst: 'Checking with Textile.',
  venueError: (message: string, seconds: number) =>
    `Couldn't reach Textile: ${message}. Trying again in ${seconds} seconds.`,
  checkNow: 'Check now',
  checkAgain: 'Check again',
  backToConnect: 'Back to Connect',

  rejectedTitle: 'Textile turned this request down',
  rejectedBody: (slug: string | null) =>
    slug
      ? `Textile declined the access request for maker ${slug}.`
      : 'Textile declined the access request for this maker.',
  flaggedTitle: 'Textile blocked this maker',
  flaggedBody: (slug: string | null) =>
    slug
      ? `Maker ${slug} is blocked and will not receive quotes.`
      : 'This maker is blocked and will not receive quotes.',
  contact: (slug: string | null) =>
    `Write to ${CONTACT_EMAIL} or ask in the Telegram group at ${TELEGRAM}. Mention your maker id${slug ? ` ${slug}` : ''} and which pair.`,
  emailTextile: 'Email Textile',
  emailSubject: (slug: string | null) =>
    `Maker access${slug ? ` for ${slug}` : ''}`,
  requestAgain: 'Request access again',

  approvedTitle: 'Approved. Starting the bot',
  approvedBody: 'Textile approved you. Starting the bot.',
  notQuotableTitle: 'Approved, with one thing left',
  notQuotableBody:
    'Textile approved you. The bot has not picked up the approval yet. This usually clears by itself in a few seconds.',
  notQuotableAgain:
    'The bot still has not picked up the approval. This page keeps checking and starts it as soon as it does.',
  finishSetup: 'Finish setting up',
  restartTitle: 'Approved. The bot needs a restart',
  restartBody: 'Textile approved you, but the running bot could not be restarted onto the new config.',
  restartNow: 'Restart now',
  startFailedTitle: 'The bot did not start',
}

export const live = {
  title: 'Your stitch is live',
  headline: (label: string | null, network: string | null) =>
    label && network
      ? `Your stitch is live on ${label} on ${network}.`
      : label
        ? `Your stitch is live on ${label}.`
        : 'Your stitch is live.',
  fetchedAgo: (seconds: number) => `Live quote from Textile, fetched ${seconds} s ago`,
  rateSell: (buy: string, soft: string, stable: string) => `1 ${stable} gets ${buy} ${soft}`,
  rateBuy: (sell: string, soft: string, stable: string) => `${sell} ${soft} gets 1 ${stable}`,
  probeNote: (sell: string, symbol: string) =>
    `For a ${sell} ${symbol} trade. Textile's fee is included in the number.`,
  quotedByYou: 'Quoted by your stitch.',
  quotedByOther: "Quoted by another maker. Yours isn't in the book yet.",
  quotedByUnknown: 'Textile did not say which maker priced this.',
  swapLink: 'See it on the public swap page',
  mineLink: "Only your bot's price",
  refresh: 'Refresh quote',

  asking: 'Asking Textile for a live quote…',
  firstPrices: 'The bot publishes its first prices a few seconds after it starts.',
  retryingIn: (seconds: number) => `Trying again in ${seconds} s.`,
  noQuoteYet: 'No quote from your stitch yet.',
  stillNone: 'Still no quote from your stitch. Check the logs, then try again.',
  tryAgain: 'Try again',
  /** The probe size and the depth are in different tokens on a buy probe. */
  depth: (probe: string, probeSymbol: string, available: string, availableSymbol: string) =>
    `Textile is quoting, but not yet for a full ${probe} ${probeSymbol}. It can fill ${available} ${availableSymbol} right now.`,
  venueDown: "Textile's quote service didn't answer.",
  openMarket: (buy: string, soft: string, stable: string) =>
    `Textile's book quotes 1 ${stable} = ${buy} ${soft} right now.`,
  unfundedTitle: 'Nothing to quote yet',
  unfundedBody:
    'The bot wallet holds nothing it can quote with. Send money to it and the prices follow.',
  noCorridorTitle: 'No swap corridor for this pair',
  seeLogs: 'See logs',

  keepHeader: 'Two things to keep it live',
  keepProcess: 'Keep this app open. The bot runs inside it and stops when the app closes.',
  keepDocker: 'Keep this machine on and Docker running. The bot stops with them.',
  keepAwake:
    'Keep the machine awake. Turn off sleep while the bot runs. A sleeping laptop quotes nothing.',

  stopped: (status: string) => `Your stitch stopped. ${status}`,
  startAgain: 'Start again',
  panelUnreachable: (message: string) => `Can't reach the panel: ${message}`,
  openBot: 'Open the bot page',
}
