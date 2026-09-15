// Every operator-facing sentence on the wizard's Fund and Confirm-your-email
// screens, in one file so the words can be reviewed without reading the
// components.
//
// House rules: plain words, short sentences, no em dashes, never a promise of
// income or of how fast Textile approves anyone. "Permit2" appears once, in
// parentheses. Server strings are shown verbatim only inside banners.

export const CONTACT_EMAIL = 'contact@textilecredit.com'
export const TELEGRAM = 't.me/TextileNigeria'

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
  /**
   * In the gas token, with the amount once the price is known: "Deposit 12.1
   * CELO and approve spending". The operator sends a coin, not a dollar
   * figure, so the title names the coin. Before the read lands, or on a chain
   * the panel can't price, it names the token alone.
   */
  title: (amount: string | null, gas: string) =>
    amount ? `Deposit ${amount} ${gas} and approve spending` : `Deposit ${gas} and approve spending`,
  addressLabel: (network: string) => `Bot wallet on ${network}`,
  copyAddress: 'Copy address',
  copied: 'Copied',
  copyFailed: "Couldn't copy. Select the address and copy it by hand.",
  viewOn: (host: string) => `View on ${host}`,
  chainWarning: (network: string) =>
    `Send on ${network} only. Money sent on another network does not arrive.`,
  noOperator:
    'This bot has no wallet address the panel can read, so there is nothing to fund here.',

  approvedHint: 'Textile can settle this token from the wallet.',
  approvalHint: (symbol: string) =>
    `One transaction, paid from gas, that lets Textile settle ${symbol} trades from this wallet. Never spends on its own.`,
  estimated: '(estimated)',

  pill: {
    addGas: 'Add gas',
    approved: 'Approved',
    needsApproval: 'Needs approval',
    ok: 'OK',
    unknown: 'Unknown',
  },

  gate: (minGas: number, gas: string) =>
    `Ready when the wallet holds ${dollars(minGas)} of ${gas} for gas.`,
  /** Why the address above is only half the story on a vault maker. */
  vaultCapital: (gas: string) =>
    `This bot quotes from an OperatorVault, so its corridor tokens come from the vault, not from the address above. Send that address ${gas} for gas only.`,
  needsGas: (minGas: number, gas: string) => `Add at least ${dollars(minGas)} of ${gas} for gas.`,
  cantRead: "Can't read the bot wallet on chain right now.",
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
  fundsFound: 'Gas found. Approving and starting.',
  unreadable: (error: string) => `The panel can't read this bot's config: ${error}`,

  gone: 'This bot no longer exists. Go back and create it again.',
  startOver: 'Start over',
  differentBot: 'Set up a different bot',
  retry: 'Retry',
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

  separateTitle: 'Set up a separate bot',
  separateBody:
    'Its own wallet and its own money. You fund it separately. Takes longer.',

  blockedAlready: (label: string) => `Already quoting ${label}.`,
  blockedNotEditable:
    "The panel can't edit this bot's config, so it can't take another corridor.",
  blockedNotConnected:
    'Not connected to Textile yet. Open it from the fleet and finish setting it up.',
  /** Connected, credential on disk, email address still unconfirmed. */
  blockedWaitingTextile:
    'Connected, but its email address is not confirmed yet. It can take the corridor once it is.',
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
   * was pointed at a second wallet and a second lot of money, for a corridor
   * that was live the whole time.
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
  openBot: 'Open the bot page',
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
   * The same fact on a vault maker, as a condition rather than an errand. Its
   * corridor tokens arrive as deposits through the vault's own epochs, and a
   * plain transfer to the vault mints no shares, so nobody is told to send
   * anything anywhere.
   */
  oneSidedVault: (bot: string, soft: string, stable: string) =>
    `${bot} can buy ${soft} with the ${stable} the vault holds. It sells ${soft} once the vault holds some of that too.`,

  /**
   * Set up, approved, and nothing behind either side yet. Said at the ending
   * rather than waited on before the approval: the approval costs gas and no
   * balance, and money that has not arrived is the bot page's business.
   */
  unfunded: (bot: string, soft: string, stable: string) =>
    `Nothing is behind this corridor yet. Send ${soft} or ${stable} to ${bot}'s wallet and it starts quoting. The Funds tab on the bot page is where that happens.`,
  /** The same ending on a vault maker. Same rule as `oneSidedVault`: a
      condition the vault meets, never an address to send to. */
  unfundedVault: (bot: string, soft: string, stable: string) =>
    `Nothing is behind this corridor yet. ${bot} quotes from an OperatorVault, so it starts quoting once the vault holds ${soft} or ${stable}.`,

  retry: 'Retry',
  back: 'Back',
}

export const progress = {
  /**
   * `state` is the row's progress state; the chain is still being read while
   * `running` with no symbols.
   *
   * "Spending already approved" belongs to `skipped` alone, which is the chain
   * saying there was nothing to approve. On a `pending` row it is a claim about
   * a read nobody has made yet, and it reads as "nothing to do here" on a
   * screen that has not moved — which is how an operator adding a corridor
   * whose token had never been approved came to sit on this step waiting for
   * something the row told them was already done.
   */
  approveTitle: (symbols: string[], state: string) =>
    symbols.length > 0
      ? `Approve spending for ${joinAnd(symbols)}`
      : state === 'running'
        ? 'Check spending approval'
        : state === 'skipped'
          ? 'Spending already approved'
          : 'Approve spending',
  approveSub:
    "Lets Textile's swap contract (Permit2) move the bot's tokens. The bot quotes both sides of the pair, so both tokens are approved, with one small gas fee each. Usually under a minute, up to two.",
  accessTitle: 'Check your Textile seats',
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
  recreate:
    "The bot's container is gone. Press Retry. If it keeps failing, delete this bot from the fleet and set it up again.",
  retry: 'Retry',
}

export const wait = {
  connectTitle: 'Connect to Textile',
  connectBody:
    'Spending is approved. Last thing: Textile needs to know this bot and an email it can confirm. One click registers the bot and sends you a link; the bot goes live the moment you click it.',
  connectButton: 'Connect to Textile',
  connectRetry: 'Retry',
  title: 'Confirm your email',
  sentTo: (email: string) => `We sent it to ${email}. Check spam if it is not there.`,
  noAddressBody:
    'Spending is approved and the bot is registered, but it has no email address on file, so there is no link to click yet. Give Textile one and it goes live as soon as you confirm it.',
  addressLabel: 'Contact email',
  addressHint: 'An inbox you own. Confirming it is what puts this bot on the venue.',
  sendLink: 'Send the confirmation link',
  keepOpen:
    'Keep this page open. It starts the bot as soon as you confirm. If you close it, open the bot page after confirming and press Start.',
  status: (time: string, seconds: number) =>
    `Last checked ${time}. Checks again in ${seconds} seconds.`,
  statusFirst: 'Checking with Textile.',
  venueError: (message: string, seconds: number) =>
    `Couldn't reach Textile: ${message}. Trying again in ${seconds} seconds.`,
  checkNow: 'Check now',
  checkAgain: 'Check again',
  resend: 'Resend the email',
  changeEmail: 'Wrong address? Change it',
  newAddressLabel: 'New contact email',
  newAddressHint: (old: string) =>
    old
      ? `Textile sends a fresh link here and forgets ${old}. The old link stops working.`
      : 'Textile sends a fresh link here.',
  sendToNew: 'Send the link there instead',
  keepAddress: 'Keep the current one',

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

  approvedTitle: 'Confirmed. Starting the bot',
  approvedBody: 'Your address is confirmed. Starting the bot.',
  notQuotableTitle: 'Confirmed, with one thing left',
  notQuotableBody:
    'Your address is confirmed. The bot has not picked the seats up yet. This usually clears by itself in a few seconds.',
  notQuotableAgain:
    'The bot still has not picked the seats up. This page keeps checking and starts it as soon as it does.',
  finishSetup: 'Finish setting up',
  restartTitle: 'Confirmed. The bot needs a restart',
  restartBody:
    'Your address is confirmed, but the running bot could not be restarted onto the new config.',
  restartNow: 'Restart now',
  startFailedTitle: 'The bot did not start',
}

