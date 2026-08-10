import { useCallback, useEffect, useState } from 'react'
import { ApiError, api } from '../api'
import { Banner, Button, Loading, Tag } from './ui'
import { formatDate } from '../format'
import type { Bot, BotVersion, BotVersions } from '../types'

/**
 * Put a bot back on an earlier published build.
 *
 * The escape hatch for a bad release: a bot that started mispricing after an
 * update has no other way back, short of hand-editing a compose file at 2am.
 *
 * It is also the most destructive button in the panel, so nothing here is one
 * click. The list refuses to preselect anything, the consequences are on screen
 * before the button is reachable rather than only in the confirm dialog, and the
 * build the bot is already on is marked and unselectable. The three things an
 * operator actually gets wrong — that old code means old bugs, that the config
 * does *not* travel back with the image, and that the bot stops following
 * updates afterwards — are each said plainly.
 */
export default function VersionRollback({
  bot,
  onRolledBack,
}: {
  bot: Bot
  /** Fired with the server's confirmation, so the page can refresh the bot. */
  onRolledBack: (message: string) => void
}) {
  const [data, setData] = useState<BotVersions | null>(null)
  // Split, because they need different treatment: a list that wouldn't load
  // leaves nothing to show, while a refused rollback has to keep the picker on
  // screen — the operator's next move is usually to pick something else.
  const [loadError, setLoadError] = useState<string | null>(null)
  const [actionError, setActionError] = useState<string | null>(null)
  const [chosen, setChosen] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  const load = useCallback(async () => {
    try {
      setData(await api.botVersions(bot.name))
      setLoadError(null)
    } catch (e) {
      setLoadError(e instanceof ApiError ? e.message : String(e))
    }
  }, [bot.name])

  useEffect(() => {
    void load()
  }, [load])

  if (loadError) {
    return (
      <div className="space-y-3">
        <Banner tone="danger">{loadError}</Banner>
        <Button onClick={() => void load()}>Try again</Button>
      </div>
    )
  }
  if (!data) return <Loading what="published versions" />

  const { versions } = data
  // Only a commit-ranked list is newest first. Without that, position carries no
  // meaning at all, so nothing below may read anything into it — no "newest"
  // badge, and no telling the operator which way along the list they're moving.
  const ranked = data.ordering === 'commit'
  const currentIndex = versions.findIndex((v) => v.current)
  const selected = versions.find((v) => v.tag === chosen) ?? null
  const selectedIndex = versions.findIndex((v) => v.tag === chosen)
  const goingForward =
    ranked && currentIndex !== -1 && selectedIndex !== -1 && selectedIndex < currentIndex
  // Matches the server's own rule for whether the replacement is started.
  const willRestart = bot.state === 'running' || bot.state === 'restarting'

  async function rollBack(version: BotVersion) {
    const published = [
      version.publishedAt ? `Published ${formatDate(version.publishedAt)}.` : null,
      version.subject,
    ]
      .filter(Boolean)
      .join(' ')
    const lifecycle = willRestart
      ? 'The bot stops and starts again on that build: a few seconds without quotes.'
      : 'The bot stays stopped, because it is not up now.'
    if (
      !window.confirm(
        `Roll ${bot.name} back to ${version.tag}?\n\n` +
          (published ? `${published}\n\n` : '') +
          'This recreates its container on older code. Everything fixed since that build — ' +
          'pricing, nonce handling, security — is gone until you update again.\n\n' +
          'Its config is NOT rolled back. If stitch.toml uses settings that build does not ' +
          'know, it will refuse to start, so watch the logs straight after.\n\n' +
          'It then stays on that build until you press Update — Recreate keeps the pin.\n\n' +
          lifecycle,
      )
    ) {
      return
    }
    setBusy(true)
    setActionError(null)
    try {
      const res = await api.rollback(bot.name, version.tag)
      setChosen(null)
      onRolledBack(res.message ?? `${bot.name} was rolled back to ${version.tag}.`)
      // The "running now" marker has moved; re-ask rather than guess.
      await load()
    } catch (e) {
      setActionError(e instanceof ApiError ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="space-y-3">
      {actionError && (
        <Banner tone="danger" onDismiss={() => setActionError(null)}>
          {actionError}
        </Banner>
      )}
      {data.blockedReason && <Banner tone="warning">{data.blockedReason}</Banner>}
      {data.listingError && (
        <Banner tone="warning">
          Couldn&apos;t list published versions: {data.listingError}
        </Banner>
      )}

      {data.canRollBack && (
        <Banner tone="warning">
          <strong>Rolling back runs older code.</strong> Every fix published since the
          build you pick — pricing, nonce handling, security — goes with it. Use this
          to get out from under a bad release, then come back to the newest build that
          works.
        </Banner>
      )}

      {versions.length === 0 ? (
        <p className="text-sm text-muted">
          No published versions to choose from.
        </p>
      ) : (
        <>
          <ul className="divide-y divide-line-soft rounded-lg border border-line-soft">
            {versions.map((v, i) => (
              <li key={v.tag}>
                <label
                  className={`flex items-start gap-3 px-3 py-2 text-sm ${
                    v.current || !data.canRollBack
                      ? 'cursor-not-allowed opacity-60'
                      : 'cursor-pointer hover:bg-hover'
                  }`}
                >
                  <input
                    type="radio"
                    name={`rollback-${bot.name}`}
                    className="mt-1 size-4 accent-[var(--tx-accent)]"
                    checked={chosen === v.tag}
                    disabled={v.current || !data.canRollBack || busy}
                    onChange={() => setChosen(v.tag)}
                  />
                  <span className="min-w-0 flex-1">
                    <span className="flex flex-wrap items-center gap-2">
                      <span className="font-mono text-xs">{v.tag}</span>
                      {ranked && i === 0 && <Tag>newest</Tag>}
                      {v.current && <Tag>running now</Tag>}
                      <span className="text-xs text-faint">
                        {formatDate(v.publishedAt)}
                      </span>
                    </span>
                    {v.subject && (
                      <span className="mt-0.5 block truncate text-xs text-muted">
                        {v.subject}
                      </span>
                    )}
                  </span>
                </label>
              </li>
            ))}
          </ul>

          {!ranked && (
            <Banner tone="warning">
              These aren&apos;t in any particular order. Nothing could say when they
              were built — the image isn&apos;t published from a public GitHub
              repository of the same name, or the lookup failed — so this is the
              registry&apos;s own tag order, which says nothing about age. Check a
              tag against your own build history before rolling back to it.
            </Banner>
          )}

          {currentIndex === -1 && (
            <p className="text-xs text-faint">
              None of these is marked as running, so the panel couldn&apos;t match
              this bot&apos;s image ({bot.image ?? 'unknown'}) to a published build.
              Check which one you&apos;re on before picking.
            </p>
          )}

          <ul className="space-y-1 text-xs text-faint">
            <li>
              The container is recreated on the build you pick.{' '}
              {willRestart
                ? 'Quoting stops for a few seconds, then it comes back up.'
                : 'It stays stopped afterwards, because it is not up now.'}{' '}
              The nonce ledger stays on the host, so orders already on the book can
              still be replaced.
            </li>
            <li>
              Your config does not travel back with the image.{' '}
              <code>stitch.toml</code> keeps what it says today, and a build that
              predates a setting in it will refuse to start — read the logs right
              after.
            </li>
            <li>
              The bot pins to that version and stops picking up releases until you
              press Update again. Recreate keeps the pin, so recovering a stuck
              container won&apos;t put the release you just left back on.
            </li>
          </ul>

          {goingForward && (
            <Banner tone="info">
              That build is newer than the one this bot runs, so this isn&apos;t a
              rollback. Update does the same thing and always lands on the newest
              build.
            </Banner>
          )}

          <Button
            variant="danger"
            busy={busy}
            disabled={!selected || !data.canRollBack}
            onClick={() => selected && void rollBack(selected)}
          >
            {selected ? `Roll back to ${selected.tag}` : 'Pick a version'}
          </Button>
        </>
      )}
    </div>
  )
}
