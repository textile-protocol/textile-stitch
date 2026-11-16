import { Link, useLocation } from 'react-router-dom'

/** The fleet's two views: its bots, and what changed on them. */
export default function FleetTabs({ bots }: { bots: number | null }) {
  const { pathname } = useLocation()
  const tabs = [
    { to: '/', label: 'Bots', count: bots },
    { to: '/changes', label: 'Change log', count: null },
  ]
  return (
    <nav className="flex gap-x-1 border-b border-line-soft" aria-label="Fleet sections">
      {tabs.map((t) => (
        <Link
          key={t.to}
          to={t.to}
          aria-current={pathname === t.to ? 'page' : undefined}
          className={`-mb-px flex items-center gap-1.5 border-b-2 px-3 py-2 text-sm font-bold no-underline transition ${
            pathname === t.to ? 'border-accent text-ink' : 'border-transparent text-muted hover:text-ink'
          }`}
        >
          {t.label}
          {t.count !== null && (
            <span className="text-xs font-normal text-faint tabular-nums">{t.count}</span>
          )}
        </Link>
      ))}
    </nav>
  )
}
