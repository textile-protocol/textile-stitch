import { describe, expect, it } from 'vitest'
import type { PerpsView } from './api'
import {
  modulesOnTab,
  visibleBotModules,
  workspaceModules,
  quoteConfigForScope,
  remainingQuoteDraft,
  selectedWorkspaceModule,
  quoteModuleStatus,
  quoteModuleUnsaved,
} from './moduleWorkspace'
import {
  defaultCurve,
  defaultLiquidity,
  defaultProfitUnwind,
} from './modulePresentation'
import type { ModulesConfig, ModulesView } from './modules'

const now = Date.parse('2026-10-28T19:00:00Z')
const hedge = (enabled: boolean, hasFunds = false): PerpsView => ({
  enabled,
  assignmentId: 'idrx-fleet-production-1',
  recallEnabled: false,
  fundingTargetAtoms: '0',
  unavailable: false,
  remote: {
    control: { mode: enabled ? 'preview' : 'off' },
    effectiveMode: 'shadow',
    monitoring: {
      hasFunds,
      observedAt: new Date(now).toISOString(),
      pendingQuotes: 0,
      pendingActions: 0,
      inventoryOperations: [],
    },
  },
})

describe('independent module composition', () => {
  it('keeps assigned hedge and quote navigation when an empty hedge is turned off', () => {
    const before = { quoteModulesEnabled: true, hedge: hedge(true) }
    expect(visibleBotModules(before, now)).toEqual(['hedge', 'quotes'])
    expect(visibleBotModules({ ...before, hedge: hedge(false) }, now)).toEqual([
      'hedge',
      'quotes',
    ])
  })

  it('keeps hedge monitoring when quote modules are disabled', () => {
    const view = hedge(true)
    expect(
      visibleBotModules({ quoteModulesEnabled: false, hedge: view }, now)
    ).toEqual(['hedge'])
    expect(view.enabled).toBe(true)
  })

  it('retains hedge cleanup beside quote modules after disabling it', () => {
    expect(
      visibleBotModules(
        { quoteModulesEnabled: true, hedge: hedge(false, true) },
        now
      )
    ).toEqual(['hedge', 'quotes'])
  })

  it('keeps quote controls available during hedge outages and initial loading', () => {
    expect(
      visibleBotModules({ quoteModulesEnabled: true, hedge: null }, now)
    ).toEqual(['quotes'])
    expect(
      visibleBotModules(
        { quoteModulesEnabled: true, hedge: null, hedgeUnavailable: true },
        now
      )
    ).toEqual(['hedge', 'quotes'])
  })

  it('shows both groups on Modules and only the hedge summary on the dashboard', () => {
    const ids = visibleBotModules(
      { quoteModulesEnabled: true, hedge: hedge(true) },
      now
    )
    expect(modulesOnTab(ids, 'modules')).toEqual(['hedge', 'quotes'])
    expect(modulesOnTab(ids, 'dashboard')).toEqual(['hedge'])
    expect(modulesOnTab(ids, 'settings')).toEqual([])
  })
})

const config = (): ModulesConfig => ({
  mode: 'live',
  inventory: {
    enabled: false,
    target_bps: 4000,
    max_bps: 8000,
    max_skew_bps: 40,
    spread_floor_bps: 5,
  },
  spreads: {
    enabled: false,
    inventory_aware: true,
    multiplier: 1,
    max_extra_bps: 100,
    window_secs: 300,
    warmup_secs: 30,
  },
  rebalance: {
    enabled: false,
    trigger_bps: 5000,
    max_trade_bps: 200,
    max_slippage_bps: 50,
    cooldown_secs: 300,
    order_lifetime_secs: 60,
    dealer: null,
  },
  liquidity: defaultLiquidity(),
  curve: defaultCurve(),
})

const quoteView = (vault: boolean): ModulesView => {
  const c = config()
  return {
    config: { ...c, curve: { ...c.curve, enabled: true } },
    revision: 'one',
    status: null,
    running: true,
    vault,
    settlement_decimals: 6,
    corridor_decimals: 2,
    corridor_spreads: { buy: null, sell: null },
  }
}

describe('focused module workspace', () => {
  it('marks the curve unsaved when only its profit unwind changes', () => {
    const view = quoteView(true)
    const draft = {
      ...view.config,
      profit_unwind: { ...defaultProfitUnwind(), enabled: true },
    }
    expect(quoteModuleUnsaved(view, view.config, 'curve')).toBe(false)
    expect(quoteModuleUnsaved(view, draft, 'curve')).toBe(true)
    expect(quoteModuleUnsaved(view, draft, 'inventory')).toBe(false)
    // Shared mode has its own draft; it does not dirty individual modules.
    const other = view.config.mode === 'live' ? 'shadow' : 'live'
    const mode = { ...view.config, mode: other } as const
    expect(quoteModuleUnsaved(view, mode, 'inventory')).toBe(false)
  })

  it('does not offer vault modules to a wallet or while eligibility is unknown', () => {
    expect(workspaceModules(['hedge', 'quotes'], null)).toEqual(['hedge'])
    expect(workspaceModules(['hedge', 'quotes'], quoteView(false))).toEqual([
      'hedge',
      'curve',
    ])
    expect(workspaceModules(['hedge'], quoteView(true))).toEqual(['hedge'])
  })

  it('selects the configured module while keeping every eligible module in navigation', () => {
    const view = quoteView(true)
    const all = workspaceModules(['quotes'], view)
    const selected = selectedWorkspaceModule(all, null, view)
    expect(selected).toBe('curve')
    expect(all).toEqual([
      'inventory',
      'spreads',
      'rebalance',
      'liquidity',
      'curve',
    ])
    expect(all).toContain('inventory')
  })

  it('can inspect a disabled module without enabling it or changing policy', () => {
    const view = quoteView(true)
    const before = structuredClone(view)
    const all = workspaceModules(['quotes'], view)
    expect(selectedWorkspaceModule(all, 'spreads', view)).toBe('spreads')
    expect(view).toEqual(before)
  })

  it('rejects a stale deep link to an ineligible vault module', () => {
    const view = quoteView(false)
    expect(selectedWorkspaceModule(['curve'], 'inventory', view)).toBe('curve')
  })

  it('keeps all modules during a saved/running configuration mismatch', () => {
    const view = quoteView(true)
    const running = {
      ...view.config,
      inventory: { ...view.config.inventory, enabled: true },
    }
    view.status = {
      version: 1,
      config: running,
      at: now / 1000,
      decisions: [],
      rebalance_status: 'Off',
      next_attempt_at: 0,
    }
    expect(workspaceModules(['quotes'], view)).toEqual([
      'inventory',
      'spreads',
      'rebalance',
      'liquidity',
      'curve',
    ])
    expect(quoteModuleStatus(view, 'inventory', now / 1000)).toBe(
      'Update pending'
    )
  })

  it('does not label an enabled curve Live when it lacks a current reading', () => {
    const view = quoteView(false)
    expect(quoteModuleStatus(view, 'curve', now / 1000)).toBe('Waiting')
    expect(
      quoteModuleStatus({ ...view, running: false }, 'curve', now / 1000)
    ).toBe('Bot stopped')
    expect(
      quoteModuleStatus(
        { ...view, config: { ...view.config, mode: 'off' } },
        'curve',
        now / 1000
      )
    ).toBe('Off')
  })
  it('uses the liquidity keeper reading independently of quote evaluations', () => {
    const view = quoteView(true)
    view.config.liquidity.enabled = true
    view.status = {
      version: 1,
      config: view.config,
      at: now / 1000,
      decisions: [],
      rebalance_status: 'Off',
      next_attempt_at: 0,
      liquidity: {
        at: now / 1000,
        market_hours: true,
        next_change_at: now / 1000 + 3600,
        sender: '0xbot',
        operator_admin: '0xsafe',
        authority_target: '0xsafe',
        floor: '1000000',
        target: '5000000',
        liquid: '1000000',
        free: '10000000',
        committed: '0',
        quote_cap: null,
        step: 'hold',
        message: 'No change.',
        in_flight: null,
        actions: [],
      },
    }
    expect(quoteModuleStatus(view, 'liquidity', now / 1000)).toBe('Live')
    expect(quoteModuleStatus(view, 'liquidity', now / 1000 + 46)).toBe(
      'Waiting'
    )
  })
})

describe('independent module saves', () => {
  it.each(['inventory', 'spreads', 'rebalance', 'liquidity', 'curve'] as const)(
    'saves only %s even with unsaved sibling enable and mode changes',
    (key) => {
      const saved = config()
      const draft = {
        ...saved,
        mode: 'off' as const,
        ...Object.fromEntries(
          ['inventory', 'spreads', 'rebalance', 'liquidity', 'curve'].map(
            (id) => [id, { ...saved[id as typeof key], enabled: true }]
          )
        ),
      }
      const payload = quoteConfigForScope(saved, draft, key)
      expect(payload).toEqual({
        ...saved,
        [key]: { ...saved[key], enabled: true },
      })
      const remaining = remainingQuoteDraft(saved, draft, payload, key)
      expect(remaining).toEqual(draft)
      expect(saved[key].enabled).toBe(false)
    }
  )

  it('saves group mode without applying pending module toggles', () => {
    const saved = config()
    const draft = {
      ...saved,
      mode: 'shadow' as const,
      spreads: { ...saved.spreads, enabled: true },
    }
    expect(quoteConfigForScope(saved, draft, 'policy')).toEqual({
      ...saved,
      mode: 'shadow',
    })
  })

  it('owns profit unwind only with the curve and preserves sibling parameters', () => {
    const saved = config()
    const draft = {
      ...saved,
      profit_unwind: { ...defaultProfitUnwind(), enabled: true },
      spreads: { ...saved.spreads, max_extra_bps: 75 },
    }
    const payload = quoteConfigForScope(saved, draft, 'curve')
    expect(payload.profit_unwind).toEqual(draft.profit_unwind)
    expect(payload.spreads).toEqual(saved.spreads)
    expect(remainingQuoteDraft(saved, draft, payload, 'curve')).toEqual(draft)
  })

  it('discards only the selected scope, retaining sibling edits', () => {
    const saved = config()
    const draft = {
      ...saved,
      mode: 'shadow' as const,
      spreads: { ...saved.spreads, enabled: true },
      curve: { ...saved.curve, enabled: true },
    }
    expect(quoteConfigForScope(draft, saved, 'curve')).toEqual({
      ...draft,
      curve: saved.curve,
    })
    expect(quoteConfigForScope(draft, saved, 'policy')).toEqual({
      ...draft,
      mode: saved.mode,
    })
  })

  it('takes refreshed server values for untouched scopes after saving', () => {
    const saved = config()
    const draft = { ...saved, curve: { ...saved.curve, shape: 3 } }
    const next = { ...draft, spreads: { ...saved.spreads, max_extra_bps: 80 } }
    expect(remainingQuoteDraft(saved, draft, next, 'curve')).toEqual(next)
  })
})
