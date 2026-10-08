import { useEffect, useState } from 'react'
import { api } from '../api'
import {
  saleAtomic,
  saleDecimal,
  saleLabels,
  type ManualSale,
} from '../manualSales'
import { moduleStatusFresh, type ModulesView } from '../modules'
import { AllocationBar, Disclosure } from './ModuleControls'
import { Banner, Button, Card, Field, Input } from './ui'

export default function ManualSales({
  name,
  view,
  editable,
  currency,
  settlement,
  onConfigure,
}: {
  name: string
  view: ModulesView
  editable: boolean
  currency: string
  settlement: string
  onConfigure: () => void
}) {
  const [sales, setSales] = useState<ManualSale[] | null>(null)
  const [buyer, setBuyer] = useState('')
  const [amount, setAmount] = useState('')
  const [minimum, setMinimum] = useState('')
  const [ownWallet, setOwnWallet] = useState(false)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')
  const [notice, setNotice] = useState('')
  useEffect(() => {
    let active = true
    const load = () =>
      api
        .manualSales(name)
        .then((rows) => {
          if (active) setSales(rows)
        })
        .catch((e) => {
          if (active) setError(String(e))
        })
    void load()
    const timer = setInterval(() => void load(), 5000)
    return () => {
      active = false
      clearInterval(timer)
    }
  }, [name])
  const c = view.config
  const ready =
    c.mode === 'live' && c.rebalance.enabled && c.rebalance.method !== 'dealer'
  const decimals = Number(view.dataset_template.corridor_decimals)
  const atomicAmount = saleAtomic(amount, decimals)
  const atomicMinimum = saleAtomic(minimum, view.settlement_decimals)
  const valid =
    atomicAmount &&
    atomicMinimum &&
    /^0x[0-9a-fA-F]{40}$/.test(buyer) &&
    !/^0x0{40}$/i.test(buyer)
  const observation = moduleStatusFresh(
    view.status,
    view.running,
    Date.now() / 1000
  )
    ? view.status?.decisions.at(-1)
    : undefined
  const share = observation?.decision.inventory_bps
  const suggested = observation?.decision.rebalance_sell
  const corridorBalance = observation
    ? Number(saleDecimal(observation.corridor, decimals))
    : 0
  const settlementBalance = observation
    ? Number(saleDecimal(observation.settlement, view.settlement_decimals))
    : 0
  const afterCorridor = observation
    ? (corridorBalance - Number(amount || 0)) * observation.price
    : 0
  const afterNav = afterCorridor + settlementBalance + Number(minimum || 0)
  const projected =
    atomicAmount &&
    atomicMinimum &&
    Number(amount) <= corridorBalance &&
    afterNav > 0
      ? Math.round((afterCorridor / afterNav) * 10000)
      : null
  async function create() {
    if (!valid || !ready || !editable || busy) return
    setBusy(true)
    setError('')
    setNotice('')
    try {
      const sale = await api.createManualSale(name, {
        taker: buyer,
        corridorAmount: atomicAmount!,
        minSettlement: atomicMinimum!,
      })
      setSales((current) => [
        sale,
        ...(current ?? []).filter((row) => row.id !== sale.id),
      ])
      setNotice(
        'Request created. Open the buyer page or copy its private link below.'
      )
    } catch (e) {
      setError(String(e))
    } finally {
      setBusy(false)
    }
  }
  async function close(sale: ManualSale) {
    setBusy(true)
    setError('')
    try {
      const closed = await api.closeManualSale(name, sale.id)
      setSales(
        (current) =>
          current?.map((row) => (row.id === sale.id ? closed : row)) ?? []
      )
      setNotice(
        'New quotes stopped. A signed quote can still settle until its deadline.'
      )
    } catch (e) {
      setError(String(e))
    } finally {
      setBusy(false)
    }
  }
  return (
    <div className="space-y-5">
      <Card>
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div>
            <p className="text-muted text-xs font-bold uppercase tracking-wider">
              Spot rebalancing · manual
            </p>
            <h3 className="mt-2 text-xl font-bold">
              Less {currency}. More {settlement}.
            </h3>
            <p className="text-muted mt-2 max-w-xl text-sm">
              Choose what to sell, then complete the swap with your wallet or a
              buyer you invite.
            </p>
          </div>
          <Button variant="ghost" onClick={onConfigure}>
            Sale limits
          </Button>
        </div>
        {!ready && (
          <div className="mt-4">
            <Banner tone="info">
              Enable spot rebalancing in Live mode and save in Parameters to
              create a sale.{' '}
              <button className="font-bold underline" onClick={onConfigure}>
                Open settings
              </button>
            </Banner>
          </div>
        )}
        {!view.running && (
          <p className="text-muted mt-3 text-sm">
            Start this bot before the buyer requests a quote.
          </p>
        )}
        <div className="mt-6 grid gap-6 lg:grid-cols-[1.1fr_1fr]">
          <div className="space-y-5">
            <Field
              label={`Sell from vault · ${currency}`}
              hint="The exact amount the buyer receives. The bot checks available holdings and your sale limits."
            >
              <Input
                inputMode="decimal"
                placeholder="0.00"
                value={amount}
                onChange={(e) => setAmount(e.target.value)}
              />
            </Field>
            {suggested && suggested !== '0' && (
              <button
                className="text-sm font-bold text-accent"
                onClick={() => setAmount(saleDecimal(suggested, decimals))}
              >
                Use suggested amount:{' '}
                {Number(saleDecimal(suggested, decimals)).toLocaleString(
                  undefined,
                  { maximumFractionDigits: 4 }
                )}{' '}
                {currency}
              </button>
            )}
            <Field
              label={`Minimum the vault receives · ${settlement}`}
              hint="Net proceeds, after the trading fee. The buyer sees the actual cost in their fresh quote."
            >
              <Input
                inputMode="decimal"
                placeholder="0.00"
                value={minimum}
                onChange={(e) => setMinimum(e.target.value)}
              />
            </Field>
            <fieldset>
              <legend className="mb-2 text-sm font-bold">Who is buying?</legend>
              <div className="grid grid-cols-2 gap-2">
                {[false, true].map((own) => (
                  <button
                    type="button"
                    key={String(own)}
                    aria-pressed={ownWallet === own}
                    onClick={() => setOwnWallet(own)}
                    className={`rounded-xl border px-3 py-3 text-sm font-bold ${ownWallet === own ? 'bg-accent-tint border-accent' : 'border-line-soft'}`}
                  >
                    {own ? 'Use my wallet' : 'Invite a buyer'}
                  </button>
                ))}
              </div>
            </fieldset>
            <Field
              label={ownWallet ? 'Your buying wallet' : 'Buyer wallet address'}
              hint="Only this wallet can accept the quote. Use a wallet other than the vault."
            >
              <Input
                value={buyer}
                placeholder="0x…"
                spellCheck={false}
                onChange={(e) => setBuyer(e.target.value.trim())}
              />
            </Field>
            {ownWallet && (
              <p className="bg-hover rounded-xl p-3 text-xs">
                This reduces the vault’s FX exposure. You personally receive the{' '}
                {currency}, so the exposure moves to your wallet.
              </p>
            )}
            <Button
              variant="primary"
              busy={busy}
              disabled={!valid || !ready || !editable}
              onClick={() => void create()}
            >
              Create private sale request
            </Button>
            <p className="text-muted text-xs">
              No funds move when you create a request. The buyer reviews the
              price and confirms in their wallet.
            </p>
          </div>
          <div className="bg-canvas h-fit rounded-xl p-5">
            <p className="font-bold">Your vault after this sale</p>
            <p className="text-muted mt-1 text-xs">
              Estimated at the current reference price and your minimum
              proceeds.
            </p>
            <div className="mt-5 space-y-5">
              <div>
                <p className="mb-2 text-sm">
                  {currency} now{' '}
                  <strong className="float-right">
                    {share == null
                      ? 'Waiting for balances'
                      : `${(share / 100).toFixed(1)}%`}
                  </strong>
                </p>
                {share != null && (
                  <AllocationBar
                    share={share}
                    target={c.inventory.target_bps}
                    limit={c.inventory.max_bps}
                    currency={currency}
                  />
                )}
              </div>
              <div>
                <p className="mb-2 text-sm">
                  After sale{' '}
                  <strong className="float-right">
                    {projected == null
                      ? '—'
                      : `${(projected / 100).toFixed(1)}%`}
                  </strong>
                </p>
                {projected != null && (
                  <AllocationBar
                    share={projected}
                    target={c.inventory.target_bps}
                    limit={c.inventory.max_bps}
                    currency={currency}
                  />
                )}
              </div>
            </div>
            <p className="text-muted mt-5 text-xs">
              Target: {c.inventory.target_bps / 100}% {currency}. Prices and
              other trades may change the outcome.
            </p>
          </div>
        </div>
      </Card>
      {error && (
        <Banner tone="danger" onDismiss={() => setError('')}>
          {error}
        </Banner>
      )}
      {notice && (
        <Banner tone="info" onDismiss={() => setNotice('')}>
          {notice}
        </Banner>
      )}
      <Card title="Your sale requests">
        {sales === null ? (
          <p className="text-muted text-sm">Loading requests…</p>
        ) : sales.length === 0 ? (
          <p className="text-muted text-sm">
            Your requests and confirmed sales will appear here.
          </p>
        ) : (
          <div className="space-y-3">
            {sales.map((sale) => (
              <div
                key={sale.id}
                className="border-line-soft rounded-xl border p-4"
              >
                <div className="flex flex-wrap items-center justify-between gap-2">
                  <p className="font-bold">
                    {Number(
                      saleDecimal(sale.corridorAmount, sale.corridorDecimals)
                    ).toLocaleString(undefined, {
                      maximumFractionDigits: 4,
                    })}{' '}
                    {sale.corridorSymbol}
                  </p>
                  <span className="bg-hover rounded-full px-3 py-1 text-xs font-bold">
                    {saleLabels[sale.status]}
                  </span>
                </div>
                <p className="text-muted mt-2 break-all text-xs">
                  Buyer: {sale.taker}
                </p>
                <p className="text-muted mt-1 text-xs">
                  Minimum:{' '}
                  {saleDecimal(sale.minSettlement, sale.settlementDecimals)}{' '}
                  {sale.settlementSymbol} · Link expires{' '}
                  {new Date(sale.expiresAt).toLocaleString()}
                </p>
                {sale.txHash && (
                  <p className="text-muted mt-2 break-all text-xs">
                    Transaction: {sale.txHash}
                  </p>
                )}
                {['open', 'quoted', 'submitted'].includes(sale.status) && (
                  <div className="mt-3 flex flex-wrap items-center gap-3">
                    <a
                      className="text-sm font-bold text-accent underline"
                      href={sale.buyerUrl}
                      target="_blank"
                      rel="noreferrer"
                    >
                      Open buyer page ↗
                    </a>
                    <Button
                      onClick={() => {
                        void navigator.clipboard
                          .writeText(sale.buyerUrl)
                          .then(() => setNotice('Private buyer link copied.'))
                          .catch(() =>
                            setError(
                              'Could not copy. Open the buyer page and copy its URL.'
                            )
                          )
                      }}
                    >
                      Copy private link
                    </Button>
                    <Button
                      variant="ghost"
                      disabled={busy || !editable}
                      onClick={() => void close(sale)}
                    >
                      Close request
                    </Button>
                  </div>
                )}
              </div>
            ))}
          </div>
        )}
      </Card>
      <Disclosure title="How settlement and closing work">
        <div className="text-muted space-y-3 text-sm">
          <p>
            The link lasts 24 hours. A price is only signed when the buyer
            requests it. Quotes are short-lived; the buyer can explicitly
            request a fresh price after expiry.
          </p>
          <p>
            Closing stops new quotes. It does not cancel signatures already
            issued. A sale is only marked completed after Textile confirms the
            on-chain fill.
          </p>
          <p>
            Safe and custody wallets can wait for approvals. If approval takes
            longer than the quote, request a fresh price before submitting the
            swap. Each sale request can settle only once.
          </p>
        </div>
      </Disclosure>
    </div>
  )
}
