import type { BotTab } from '../botRoutes'
import { modulesOnTab, type BotModuleId } from '../moduleWorkspace'
import type { Bot, Funding } from '../types'
import type { PerpsState } from '../usePerps'
import ModuleWorkspace from './ModuleWorkspace'
import PerpsModule from './PerpsModule'
import { showHedgeModule } from '../perps'

export default function BotModules({
  ids,
  tab,
  bot,
  perps,
  funding,
}: {
  ids: readonly BotModuleId[]
  tab: BotTab
  bot: Bot
  perps: PerpsState
  funding: Funding | null
}) {
  if (tab === 'modules')
    return (
      <ModuleWorkspace
        key={bot.name}
        ids={ids}
        bot={bot}
        perps={perps}
        funding={funding}
      />
    )
  return modulesOnTab(ids, tab).includes('hedge') &&
    (perps.error || showHedgeModule(perps.view, Date.now())) ? (
    <PerpsModule bot={bot} perps={perps} funding={funding} />
  ) : null
}
