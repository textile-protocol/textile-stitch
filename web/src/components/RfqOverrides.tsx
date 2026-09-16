// The Textile connection's manual overrides, under Tools.
//
// Connect fills every one of these in; an operator only types here when
// Textile handed them a value, they run a private venue, or they are rotating
// the maker key. A typo in any field silently stops quoting, which is why
// they sit under Tools and not on Corridors, collapsed until someone asks for
// them. Saved through the same settings endpoint the Spreads form uses, so the
// config is rewritten and the bot restarted the same way.

import { useEffect, useState } from 'react'
import { ApiError, api } from '../api'
import { changedFields } from './SettingsForm'
import { Banner, Button, Field, Input } from './ui'
import type { Settings } from '../types'

const KEYS = ['rfqUrl', 'rfqMakerId', 'rfqValidationContract', 'rfqCorridor'] as const
type Key = (typeof KEYS)[number]

const pick = (s: Settings): Record<Key, string> => ({
  rfqUrl: s.rfqUrl,
  rfqMakerId: s.rfqMakerId,
  rfqValidationContract: s.rfqValidationContract,
  rfqCorridor: s.rfqCorridor,
})

const EMPTY: Record<Key, string> = {
  rfqUrl: '',
  rfqMakerId: '',
  rfqValidationContract: '',
  rfqCorridor: '',
}

export default function RfqOverrides({ bot, onSaved }: { bot: string; onSaved?: () => void }) {
  const [loaded, setLoaded] = useState<Settings | null>(null)
  const [draft, setDraft] = useState<Record<Key, string>>(EMPTY)
  const [apiKey, setApiKey] = useState('')
  const [saving, setSaving] = useState(false)
  const [open, setOpen] = useState(false)
  const [note, setNote] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    let cancelled = false
    void api
      .settings(bot, 0)
      .then((s) => {
        if (cancelled) return
        setLoaded(s)
        setDraft(pick(s))
      })
      .catch((e: unknown) => {
        if (!cancelled) setError(e instanceof ApiError ? e.message : String(e))
      })
    return () => {
      cancelled = true
    }
  }, [bot])

  const dirty =
    loaded !== null &&
    (KEYS.some((k) => draft[k] !== loaded[k]) || apiKey.trim() !== '')

  async function save() {
    if (!loaded) return
    setSaving(true)
    setError(null)
    setNote(null)
    // Same patch the Corridors form sends: pool identity as a guard against a
    // config that changed underneath, and only the changed keys.
    const patch = changedFields(loaded, { ...loaded, ...draft }, apiKey)
    try {
      const res = await api.saveSettings(bot, patch)
      setLoaded(res.settings)
      setDraft(pick(res.settings))
      setApiKey('')
      setNote(res.message)
      onSaved?.()
    } catch (e) {
      setError(e instanceof ApiError ? e.message : String(e))
    } finally {
      setSaving(false)
    }
  }

  if (!loaded && !error) return null

  return (
    <div className="space-y-3">
      {loaded && (
        <div className="border-t border-line-soft pt-4">
          <button
            type="button"
            className={`-m-1 flex w-full items-center gap-2 rounded-lg p-1 text-left hover:bg-hover ${open ? 'mb-4' : ''}`}
            aria-expanded={open}
            onClick={() => setOpen((v) => !v)}
          >
            <span
              aria-hidden
              className={`inline-block text-xs text-muted transition-transform ${open ? 'rotate-90' : ''}`}
            >
              ▸
            </span>
            <span className="text-sm font-bold">Manual overrides</span>
          </button>
          {open && (
            <div className="space-y-4">
              <p className="text-xs text-faint">
                Connect fills these in; only change them if Textile gave you a
                value to paste, or you run a private venue.
              </p>
              <Field
                label="Quote stream URL"
                hint="Where the bot listens for private quote requests. Production is wss://. ws:// is allowed only on localhost."
              >
                <Input
                  value={draft.rfqUrl}
                  disabled={!loaded.editable}
                  placeholder="wss://api.textilecredit.com/v2/maker/stream"
                  onChange={(e) => setDraft({ ...draft, rfqUrl: e.target.value })}
                />
              </Field>
              <Field
                label="Maker ID"
                hint="Textile's maker record ID (starts with cl or cm). Not the short display name."
              >
                <Input
                  value={draft.rfqMakerId}
                  disabled={!loaded.editable}
                  onChange={(e) => setDraft({ ...draft, rfqMakerId: e.target.value })}
                />
              </Field>
              <Field
                label="Fill validation contract"
                hint="The chain's PreferredFillerValidation contract. Every quote binds its taker through it, so nobody else can fill a quote that lost."
              >
                <Input
                  value={draft.rfqValidationContract}
                  disabled={!loaded.editable}
                  onChange={(e) => setDraft({ ...draft, rfqValidationContract: e.target.value })}
                />
              </Field>
              <Field
                label="Corridor"
                hint="Trading corridor this bot quotes on (for example cngn-usdt-celo). Usually matches the bot's corridor."
              >
                <Input
                  value={draft.rfqCorridor}
                  disabled={!loaded.editable}
                  onChange={(e) => setDraft({ ...draft, rfqCorridor: e.target.value })}
                />
              </Field>
              <Field
                label="API key"
                hint={
                  loaded.rfqApiKeySet
                    ? 'A key is already saved. Paste a new one only to rotate it. The current value is never shown.'
                    : 'Starts with tx_live_…. Saved on disk for the panel owner only — never written to stitch.toml.'
                }
              >
                <Input
                  type="password"
                  value={apiKey}
                  disabled={!loaded.editable}
                  placeholder={loaded.rfqApiKeySet ? '••••••••' : 'tx_live_…'}
                  autoComplete="off"
                  onChange={(e) => setApiKey(e.target.value)}
                />
              </Field>
              {note && <Banner tone="info">{note}</Banner>}
              {error && <Banner tone="danger">{error}</Banner>}
              <div className="flex flex-wrap items-center gap-3">
                <Button variant="primary" busy={saving} disabled={!dirty || !loaded.editable} onClick={() => void save()}>
                  Save and restart
                </Button>
                {dirty && (
                  <Button
                    onClick={() => {
                      setDraft(pick(loaded))
                      setApiKey('')
                      setError(null)
                    }}
                  >
                    Discard
                  </Button>
                )}
              </div>
            </div>
          )}
        </div>
      )}
      {!loaded && error && <Banner tone="danger">{error}</Banner>}
    </div>
  )
}
