import { Link, useNavigate } from 'react-router-dom'
import { botPath } from '../botRoutes'
import type { Bot } from '../types'
import type { PerpsState } from '../usePerps'
import { Banner, Toggle } from './ui'

export function PerpsFeature({
  bot,
  dirty,
  perps,
}: {
  bot: Bot
  dirty: boolean
  perps: PerpsState
}) {
  const navigate = useNavigate()
  const { view, loading, busy, error } = perps
  const modulePath = `${botPath(bot.name, 'modules')}&hedge=setup`
  return (
    <section className="border-line-soft space-y-3 border-t pt-5">
      <h3 className="font-bold">FX hedge</h3>
      <Toggle
        label="Enable FX hedging"
        checked={view?.enabled ?? false}
        disabled={
          busy || loading || !view || !bot.editable || (dirty && !view.enabled)
        }
        onChange={(enabled) => {
          if (enabled && !view?.assignmentId) {
            navigate(modulePath)
            return
          }
          void perps.save(enabled ? 'preview' : 'off').then((saved) => {
            if (saved && enabled) navigate(botPath(bot.name, 'modules'))
          })
        }}
      />
      <p className="text-muted text-sm">
        Starts in Preview. Read balances and manage Preview, Live or Off in the{' '}
        <Link className="text-accent underline" to={modulePath}>
          FX hedge module
        </Link>
        . Turning off stops new hedge commitments; existing positions stay
        visible until cleared.
      </p>
      {dirty && (
        <p className="text-muted text-xs">
          Save your corridor settings before enabling.
        </p>
      )}
      {error && <Banner tone="danger">{error}</Banner>}
    </section>
  )
}
