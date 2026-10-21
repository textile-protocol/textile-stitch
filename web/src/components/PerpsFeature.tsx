import { useEffect, useRef, useState } from 'react'
import { api } from '../api'
import type { Bot } from '../types'
import { Banner, Button, Input } from './ui'

export function PerpsFeature({ bot, dirty }: { bot: Bot; dirty: boolean }) {
  const initialized = useRef(false)
  const [id, setId] = useState('')
  const [enabled, setEnabled] = useState(false)
  const [mode, setMode] = useState('off')
  const [effective, setEffective] = useState('shadow')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(true)
  useEffect(() => {
    let active = true
    setLoading(true)
    setError(null)
    initialized.current = false
    async function refresh() {
      try {
        const value = await api.perps(bot.name)
        if (!active) return
        if (!initialized.current) setId(value.assignmentId)
        initialized.current = true
        setEnabled(value.enabled)
        setMode(
          value.enabled ? (value.remote?.control?.mode ?? 'unknown') : 'off'
        )
        setEffective(
          value.unavailable
            ? 'unavailable'
            : (value.remote?.effectiveMode ?? 'shadow')
        )
        setError(
          value.unavailable
            ? 'Service unavailable. Turning off still stops local authorization.'
            : null
        )
      } catch (e) {
        if (active) setError(String(e))
      } finally {
        if (active) setLoading(false)
      }
    }
    void refresh()
    const interval = window.setInterval(() => void refresh(), 5000)
    return () => {
      active = false
      window.clearInterval(interval)
    }
  }, [bot.name])
  async function save(next: 'off' | 'preview' | 'live') {
    if (
      next === 'live' &&
      !window.confirm(
        'Enable FX hedge execution for this Stitch using its assigned margin account and caps? This restarts the bot.'
      )
    )
      return
    setBusy(true)
    setError(null)
    try {
      await api.setPerps(bot.name, id, next)
      const value = await api.perps(bot.name)
      setEnabled(value.enabled)
      setMode(
        value.enabled ? (value.remote?.control?.mode ?? 'unknown') : 'off'
      )
      setEffective(value.remote?.effectiveMode ?? 'shadow')
    } catch (e) {
      setError(String(e))
    } finally {
      setBusy(false)
    }
  }
  return (
    <section className="border-line-soft space-y-3 border-t pt-5">
      <h3 className="font-bold">FX hedge</h3>
      <p className="text-muted text-sm">
        Hedge this Stitch’s FX inventory with its assigned margin account.
      </p>
      <p className="text-sm">
        Feature: {mode}. Authorization:{' '}
        {effective === 'shadow' ? 'preview only' : effective}.
      </p>
      <label className="block text-sm">
        Hedge assignment
        <Input
          value={id}
          onChange={(e) => setId(e.target.value)}
          disabled={
            busy || loading || enabled || mode !== 'off' || !bot.editable
          }
          placeholder="Assignment supplied by Textile ops"
        />
      </label>
      <div className="flex gap-2">
        {(['off', 'preview', 'live'] as const).map((next) => (
          <Button
            key={next}
            disabled={
              busy ||
              loading ||
              (dirty && next !== 'off') ||
              !bot.editable ||
              !id ||
              (mode === next && !(next === 'off' && enabled))
            }
            onClick={() => void save(next)}
          >
            {next === 'off'
              ? 'Turn off'
              : next === 'preview'
                ? 'Preview'
                : 'Enable hedging'}
          </Button>
        ))}
      </div>
      <p className="text-muted text-xs">
        Off stops new hedges. Open positions can still be reduced, closed or
        liquidated. Preview checks the feed without trading. Only explicitly
        assigned Stitches can enable execution.
      </p>
      {dirty && (
        <p className="text-muted text-xs">Save your other settings first.</p>
      )}
      {error && <Banner tone="danger">{error}</Banner>}
    </section>
  )
}
