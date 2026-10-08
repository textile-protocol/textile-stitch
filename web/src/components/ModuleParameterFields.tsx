import type { ModulesConfig } from '../modules'
import { Disclosure, ModuleSwitch, NumberControl } from './ModuleControls'

type FieldProps = {
  draft: ModulesConfig
  onChange: (value: ModulesConfig) => void
  currency: string
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
      <Disclosure title="Advanced · price adjustments">
        <div className="space-y-5">
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
        </div>
      </Disclosure>
      <Disclosure title="How inventory balancing works">
        <p className="text-muted text-sm leading-relaxed">
          Above target, the bot lowers its buying price and makes selling more
          competitive. Below target, it does the reverse. It changes quotes; it
          does not place an immediate sale. Holdings are measured by value, not
          token count.
        </p>
        <p className="text-muted mt-3 text-xs">
          For precise settings: 1% = 100 basis points (bps). You can type values
          in steps of 0.01%.
        </p>
      </Disclosure>
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
      <div className="border-line-soft rounded-xl border p-4">
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
      <Disclosure title="Advanced · observation timing">
        <div className="space-y-5">
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
        </div>
      </Disclosure>
      <Disclosure title="How dynamic spreads work">
        <p className="text-muted text-sm leading-relaxed">
          A wider price range adds more margin, up to your limit. Inventory
          weighting reduces that addition on the side that brings holdings
          toward target. This responds to recent movement; it does not predict
          where prices go next. Additions round up to whole basis points before
          weighting. 1 bps = 0.01%.
        </p>
      </Disclosure>
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
      <div className="bg-accent-tint rounded-xl p-4 text-sm">
        <p className="font-bold">You decide when to sell</p>
        <p className="text-muted mt-2">
          Open Manual sale to choose an amount and minimum proceeds. Use your
          own wallet or invite a buyer. Each buyer reviews a fresh price and
          confirms the swap.
        </p>
        {draft.rebalance.method === 'dealer' && (
          <button
            type="button"
            className="mt-3 font-bold underline"
            onClick={() => rebalance({ method: 'manual' })}
          >
            Switch from dealer API to manual sales
          </button>
        )}
      </div>
      <Disclosure title="Advanced · execution limits">
        <div className="space-y-5">
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
            onChange={(order_lifetime_secs) =>
              rebalance({ order_lifetime_secs })
            }
          />
        </div>
      </Disclosure>
    </>
  )
}
