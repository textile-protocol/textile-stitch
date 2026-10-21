import type { ReactNode } from 'react'
import type { LiquidityConfig, ModulesConfig, Weekday } from '../modules'
import {
  clockMinutes,
  shiftClock,
  weekdayNames,
  weekdays,
  type ModuleKey,
} from '../modulePresentation'
import {
  SecondarySettings,
  ModuleSwitch,
  NumberControl,
} from './ModuleControls'
import { Field, Input } from './ui'

type FieldProps = {
  draft: ModulesConfig
  onChange: (value: ModulesConfig) => void
  currency: string
  settlement?: string
}

export function InventoryFields({ draft, onChange, currency }: FieldProps) {
  const inventory = (patch: Partial<ModulesConfig['inventory']>) =>
    onChange({ ...draft, inventory: { ...draft.inventory, ...patch } })
  return (
    <>
      <NumberControl
        label={`Target ${currency} holdings`}
        value={draft.inventory.target_bps}
        min={1}
        max={9998}
        sliderMax={9998}
        onChange={(target_bps) => inventory({ target_bps })}
        hint={`The share of vault value you want to hold in ${currency}.`}
      />
      <NumberControl
        label="Pause purchases at"
        value={draft.inventory.max_bps}
        min={2}
        max={9999}
        sliderMax={9999}
        onChange={(max_bps) => inventory({ max_bps })}
        hint="Stop buying more at this share. Sales can still reduce holdings."
      />
      <SecondarySettings title="Price adjustments">
        <NumberControl
          label="Maximum price adjustment"
          value={draft.inventory.max_skew_bps}
          onChange={(max_skew_bps) => inventory({ max_skew_bps })}
          hint="How far inventory balancing can move each base quote margin. A larger adjustment gives a stronger incentive to rebalance."
        />
        <NumberControl
          label="Minimum quote margin"
          value={draft.inventory.spread_floor_bps}
          onChange={(spread_floor_bps) => inventory({ spread_floor_bps })}
          hint="Inventory balancing never tightens a margin below this. Dynamic spreads may add more."
        />
      </SecondarySettings>
    </>
  )
}

export function SpreadFields({ draft, onChange }: FieldProps) {
  const spreads = (patch: Partial<ModulesConfig['spreads']>) =>
    onChange({ ...draft, spreads: { ...draft.spreads, ...patch } })
  return (
    <>
      <NumberControl
        label="Maximum extra margin"
        value={draft.spreads.max_extra_bps}
        sliderMax={100}
        onChange={(max_extra_bps) => spreads({ max_extra_bps })}
        hint="The most this module can add on either side, on top of your existing margins."
      />
      <NumberControl
        label="Sensitivity to price movement"
        value={draft.spreads.multiplier}
        scale={1}
        unit="×"
        min={0}
        max={10}
        step={0.1}
        sliderMax={10}
        onChange={(multiplier) => spreads({ multiplier })}
        hint="At 1×, a 0.20% price range adds up to 0.20% before the limit and inventory weighting."
      />
      <div>
        <div className="flex items-center justify-between gap-4">
          <span className="text-sm font-bold">Favor inventory reduction</span>
          <ModuleSwitch
            label="Favor inventory reduction"
            checked={draft.spreads.inventory_aware ?? false}
            onChange={(inventory_aware) => spreads({ inventory_aware })}
          />
        </div>
        <p className="text-muted mt-2 text-xs leading-relaxed">
          Reduce the extra margin on trades that move holdings toward your
          target.
        </p>
        {draft.spreads.inventory_aware && !draft.inventory.enabled && (
          <p className="mt-2 text-xs font-bold text-warning">
            Enable inventory balancing for this option to take effect.
          </p>
        )}
      </div>
      <SecondarySettings title="Observation timing">
        <NumberControl
          label="Price history window"
          value={draft.spreads.window_secs}
          unit="sec"
          scale={1}
          min={10}
          max={3600}
          onChange={(window_secs) => spreads({ window_secs })}
          hint="Measure the highest and lowest observed price within this period."
        />
        <NumberControl
          label="Warmup time"
          value={draft.spreads.warmup_secs}
          unit="sec"
          scale={1}
          min={1}
          max={3599}
          onChange={(warmup_secs) => spreads({ warmup_secs })}
          hint="Wait for this much fresh price history before offering module quotes. Must be shorter than the window."
        />
      </SecondarySettings>
    </>
  )
}

export function RebalanceFields({ draft, onChange, currency }: FieldProps) {
  const rebalance = (patch: Partial<ModulesConfig['rebalance']>) =>
    onChange({ ...draft, rebalance: { ...draft.rebalance, ...patch } })
  return (
    <>
      <NumberControl
        label="Suggest a sale above"
        value={draft.rebalance.trigger_bps}
        min={1}
        max={9999}
        sliderMax={9999}
        onChange={(trigger_bps) => rebalance({ trigger_bps })}
        hint={`Suggest a sale when ${currency} reaches this share of vault value. Must be above the inventory target and no higher than the purchase limit.`}
      />
      <NumberControl
        label="Maximum per sale"
        value={draft.rebalance.max_trade_bps}
        min={1}
        max={1000}
        sliderMax={1000}
        onChange={(max_trade_bps) => rebalance({ max_trade_bps })}
        hint="Share of total vault value, not a share of the currency balance."
      />
      {draft.rebalance.method === 'dealer' && (
        <div className="bg-accent-tint rounded-xl p-4 text-sm">
          <p>This bot requests sales from a dealer API.</p>
          <button
            type="button"
            className="mt-2 font-bold underline"
            onClick={() => rebalance({ method: 'manual' })}
          >
            Switch from dealer API to manual sales
          </button>
        </div>
      )}
      <SecondarySettings title="Execution limits">
        <NumberControl
          label="Maximum price discount"
          value={draft.rebalance.max_slippage_bps}
          max={999}
          onChange={(max_slippage_bps) => rebalance({ max_slippage_bps })}
          hint="Maximum acceptable discount to the reference price when selling."
        />
        <NumberControl
          label="Wait between sales"
          value={draft.rebalance.cooldown_secs}
          unit="sec"
          scale={1}
          min={40}
          max={Number.MAX_SAFE_INTEGER}
          onChange={(cooldown_secs) => rebalance({ cooldown_secs })}
          hint="Must be at least 30 seconds longer than the order lifetime."
        />
        <NumberControl
          label="Order lifetime"
          value={draft.rebalance.order_lifetime_secs}
          unit="sec"
          scale={1}
          min={10}
          max={300}
          onChange={(order_lifetime_secs) => rebalance({ order_lifetime_secs })}
        />
      </SecondarySettings>
    </>
  )
}

/** What each module does, behind its "How … works" link. Behavior only;
 * the fields carry their own hints. */
export const moduleGuides: Record<
  ModuleKey,
  {
    title: string
    body: (currency: string, settlement: string) => ReactNode
  }
> = {
  inventory: {
    title: 'How inventory balancing works',
    body: () => (
      <>
        <p>
          Above target, the bot lowers its buying price and makes selling more
          competitive. Below target, it does the reverse. It changes quotes; it
          does not place an immediate sale. Holdings are measured by value, not
          token count.
        </p>
        <p className="mt-3 text-xs">
          For precise settings: 1% = 100 basis points (bps). You can type values
          in steps of 0.01%.
        </p>
      </>
    ),
  },
  spreads: {
    title: 'How dynamic spreads work',
    body: () => (
      <p>
        A wider price range adds more margin, up to your limit. Inventory
        weighting reduces that addition on the side that brings holdings toward
        target. This responds to recent movement; it does not predict where
        prices go next. Additions round up to whole basis points before
        weighting. 1 bps = 0.01%.
      </p>
    ),
  },
  rebalance: {
    title: 'How spot rebalancing works',
    body: (currency) => (
      <>
        <p>
          When {currency} holdings reach your threshold, Stitch suggests a sale.
          You decide when to sell.
        </p>
        <p className="mt-3">
          Open Manual sale to choose an amount and minimum proceeds. Use your
          own wallet or invite a buyer. Each buyer reviews a fresh price and
          confirms the swap.
        </p>
      </>
    ),
  },
  liquidity: {
    title: 'How the liquidity schedule works',
    body: (_, settlement) => (
      <>
        <p>
          The vault stakes idle {settlement} in Aave. Bundled fills from several
          makers can only use settlement kept liquid up to the vault’s floor.
          This module raises the floor for market hours and lowers it after, so
          more earns yield when bundles are less likely. After raising the
          floor, the bot pulls cash back from Aave with{' '}
          <code>prepareSettlement</code>, which anyone can call.
        </p>
        <p className="mt-3">
          Before lowering the floor, the bot caps how much {settlement} new
          quotes can commit and waits for live quotes above the new floor to
          expire. In Preview it only records what it would do.
        </p>
      </>
    ),
  },
}

/** Operator's local equivalent of a UTC clock time, using today's offset. */
function localHint(utc: string): string {
  const shifted = shiftClock(utc, -new Date().getTimezoneOffset())
  if (!shifted) return 'Use 24-hour HH:MM.'
  const day =
    shifted.dayShift > 0
      ? ' (next day)'
      : shifted.dayShift < 0
        ? ' (previous day)'
        : ''
  return `${shifted.time}${day} your time, ${Intl.DateTimeFormat().resolvedOptions().timeZone}`
}

export function LiquidityFields({
  draft,
  onChange,
  settlement = 'settlement',
}: FieldProps) {
  const l = draft.liquidity
  const liquidity = (patch: Partial<LiquidityConfig>) =>
    onChange({ ...draft, liquidity: { ...l, ...patch } })
  const toggleDay = (day: Weekday) =>
    liquidity({
      days: l.days.includes(day)
        ? l.days.filter((d) => d !== day)
        : weekdays.filter((d) => d === day || l.days.includes(d)),
    })
  // Both fields empty means "no authority"; a half-filled pair stays in the
  // draft so validation can point at the missing value.
  const authority = (
    patch: Partial<NonNullable<LiquidityConfig['authority']>>
  ) => {
    const next = {
      roles_modifier: l.authority?.roles_modifier ?? '',
      role_key: l.authority?.role_key ?? '',
      ...patch,
    }
    liquidity({
      authority: next.roles_modifier || next.role_key ? next : null,
    })
  }
  const open = clockMinutes(l.open_utc)
  const close = clockMinutes(l.close_utc)
  return (
    <>
      <div className="grid gap-4 sm:grid-cols-2">
        <Field label="Market opens (UTC)" hint={localHint(l.open_utc)}>
          <Input
            type="time"
            step={60}
            value={l.open_utc}
            aria-invalid={open === null || open === close}
            onChange={(e) => liquidity({ open_utc: e.target.value })}
          />
        </Field>
        <Field label="Market closes (UTC)" hint={localHint(l.close_utc)}>
          <Input
            type="time"
            step={60}
            value={l.close_utc}
            aria-invalid={close === null || open === close}
            onChange={(e) => liquidity({ close_utc: e.target.value })}
          />
        </Field>
      </div>
      {open !== null && close !== null && close < open && (
        <p className="text-muted -mt-3 text-xs">
          Closes the next day: each window runs past midnight UTC.
        </p>
      )}
      <fieldset className="space-y-2">
        <legend className="text-sm font-bold">Market days (UTC)</legend>
        <div className="flex flex-wrap gap-2">
          {weekdays.map((day) => (
            <button
              key={day}
              type="button"
              aria-pressed={l.days.includes(day)}
              onClick={() => toggleDay(day)}
              className={`min-w-12 rounded-lg border px-3 py-1.5 text-sm font-bold transition ${l.days.includes(day) ? 'bg-accent-tint border-accent text-accent' : 'border-line-soft hover:bg-canvas text-muted'}`}
            >
              {weekdayNames[day]}
            </button>
          ))}
        </div>
        <p className="text-muted text-xs leading-relaxed">
          The UTC day each window opens on. A Friday window that closes after
          midnight ends on Saturday.
        </p>
      </fieldset>
      <NumberControl
        label="Keep liquid during market hours"
        value={l.market_floor_bps}
        max={10000}
        sliderMax={10000}
        onChange={(market_floor_bps) => liquidity({ market_floor_bps })}
        hint={`Share of free ${settlement} (liquid plus staked in Aave) kept liquid for bundled fills.`}
      />
      <NumberControl
        label="Keep liquid off-hours"
        value={l.off_hours_floor_bps}
        max={10000}
        sliderMax={10000}
        onChange={(off_hours_floor_bps) => liquidity({ off_hours_floor_bps })}
        hint="The rest earns yield in Aave. Must be at or below the market-hours share."
      />
      <SecondarySettings title="Authority">
        <p className="text-muted text-xs leading-relaxed">
          The operator-admin Safe enables a Zodiac Roles Modifier and assigns a
          role to this bot’s signer, scoped to{' '}
          <code>setMinLiquidSettlement(uint256)</code> on this vault. The bot
          sends the change through that role and pays the gas. Required in Live
          mode; leave both empty to remove it.
        </p>
        <Field label="Roles modifier address">
          <Input
            value={l.authority?.roles_modifier ?? ''}
            placeholder="0x…"
            spellCheck={false}
            autoComplete="off"
            className="font-mono"
            onChange={(e) =>
              authority({ roles_modifier: e.target.value.trim() })
            }
          />
        </Field>
        <Field label="Role key">
          <Input
            value={l.authority?.role_key ?? ''}
            placeholder="0x… (32 bytes)"
            spellCheck={false}
            autoComplete="off"
            className="font-mono"
            onChange={(e) => authority({ role_key: e.target.value.trim() })}
          />
        </Field>
      </SecondarySettings>
      <SecondarySettings title="Tolerance">
        <NumberControl
          label="Ignore gaps smaller than"
          value={l.tolerance_bps}
          min={1}
          max={5000}
          onChange={(tolerance_bps) => liquidity({ tolerance_bps })}
          hint="Share of free settlement. The bot leaves the floor and liquid balance alone while they are this close to the target, so small moves don't cost gas."
        />
      </SecondarySettings>
    </>
  )
}
