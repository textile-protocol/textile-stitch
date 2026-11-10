import { useEffect, useId, useRef, useState, type ReactNode } from 'react'
import {
  EMPTY_FILTER,
  type FilterOption,
  type FleetFilter,
  type LiveFilter,
  type VaultFilter,
} from '../fleetFilter'

interface SegmentOption<T extends string> {
  value: T
  label: string
  /** Shown below `sm` instead of `label`, when the full one won't fit a phone. */
  short?: string
  dot?: string
}

const LIVE_OPTIONS: readonly SegmentOption<LiveFilter>[] = [
  { value: 'all', label: 'All' },
  { value: 'live', label: 'Live', dot: 'bg-success' },
  { value: 'not-live', label: 'Not live', dot: 'bg-faint' },
]

const VAULT_OPTIONS: readonly SegmentOption<VaultFilter>[] = [
  { value: 'all', label: 'All' },
  { value: 'linked', label: 'With vault', short: 'Vault' },
  { value: 'unlinked', label: 'Without vault', short: 'No vault' },
]

const VAULT_CHIP: Record<Exclude<VaultFilter, 'all'>, string> = {
  linked: 'With vault',
  unlinked: 'Without vault',
}

/** Same height for every control on the bar, so the row reads as one piece. */
const CONTROL_H = 'h-9'

interface Chip {
  key: string
  label: string
  clear: () => void
}

/**
 * The fleet's filter bar. Search and the live switch are always out: they are
 * what an operator reaches for. Chain and vault sit behind Filters, and while
 * that is closed any of them in force show as chips, so a narrowed list never
 * hides why it is narrowed.
 *
 * A choice that can't narrow anything isn't offered: chain only shows once the
 * fleet spans two chains, vault only once some bots have one and some don't,
 * and Filters only when one of them shows.
 */
export default function FleetFilters({
  filter,
  onChange,
  liveCounts,
  chains,
  vaultCounts,
}: {
  filter: FleetFilter
  onChange: (filter: FleetFilter) => void
  liveCounts: Record<LiveFilter, number>
  chains: FilterOption[]
  vaultCounts: Record<VaultFilter, number>
}) {
  const set = (patch: Partial<FleetFilter>) => onChange({ ...filter, ...patch })
  const [open, setOpen] = useState(false)
  const panelId = useId()

  const chainChoices = withPicked(chains, filter.chain)
  const showChain = chainChoices.length > 1
  // Kept on screen while picked, so a link from a mixed fleet can be undone.
  const showVault =
    (vaultCounts.linked > 0 && vaultCounts.unlinked > 0) || filter.vault !== 'all'
  const advancedActive = (filter.chain !== '' ? 1 : 0) + (filter.vault !== 'all' ? 1 : 0)

  const chips: Chip[] = [
    filter.chain !== '' && {
      key: 'chain',
      label: `Chain: ${labelOf(chainChoices, filter.chain)}`,
      clear: () => set({ chain: '' }),
    },
    filter.vault !== 'all' && {
      key: 'vault',
      label: VAULT_CHIP[filter.vault],
      clear: () => set({ vault: 'all' }),
    },
  ].filter((c): c is Chip => !!c)

  return (
    <div className="space-y-2.5">
      {/* One wrapping row. On a phone: search and the Filters icon on top, the
          live switch full width under them. From `sm` up: search, switch,
          Filters, left to right. */}
      <div className="flex flex-wrap items-center gap-2">
        <SearchBox value={filter.query} onChange={(query) => set({ query })} />
        <Segmented
          label="Live"
          options={LIVE_OPTIONS}
          value={filter.live}
          counts={liveCounts}
          onChange={(live) => set({ live })}
          className="order-3 basis-full sm:order-2 sm:basis-auto"
        />
        {(showChain || showVault) && (
          <button
            type="button"
            onClick={() => setOpen((o) => !o)}
            aria-expanded={open}
            aria-controls={panelId}
            aria-label="Filters"
            data-fleet-filters
            className={`${CONTROL_H} order-2 inline-flex min-w-9 shrink-0 items-center justify-center gap-1.5 rounded-lg border px-2.5 text-sm sm:order-3 sm:px-3 font-bold transition focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent ${
              open || advancedActive > 0
                ? 'border-accent/40 bg-accent-tint text-accent'
                : 'border-line bg-surface text-ink hover:bg-hover'
            }`}
          >
            <SlidersIcon />
            <span className="hidden sm:inline">Filters</span>
            {advancedActive > 0 && (
              <span className="grid size-4.5 place-items-center rounded-full bg-accent text-[10px] leading-none text-on-accent tabular-nums">
                {advancedActive}
              </span>
            )}
            <ChevronIcon
              className={`hidden transition-transform sm:block ${open ? 'rotate-180' : ''}`}
            />
          </button>
        )}
      </div>

      {open && (
        <div
          id={panelId}
          className="rounded-xl border border-line-soft bg-surface p-3.5 shadow-sm transition duration-150 starting:-translate-y-1 starting:opacity-0 sm:p-4"
        >
          <div className="grid gap-3 sm:grid-cols-2">
            {showChain && (
              <PanelField label="Chain">
                <FilterSelect
                  label="Chain"
                  value={filter.chain}
                  onChange={(chain) => set({ chain })}
                  all="All chains"
                  options={chainChoices}
                />
              </PanelField>
            )}
            {showVault && (
              <PanelField label="Vault">
                <Segmented
                  label="Vault"
                  options={VAULT_OPTIONS}
                  value={filter.vault}
                  counts={vaultCounts}
                  onChange={(vault) => set({ vault })}
                  stretch
                />
              </PanelField>
            )}
          </div>
          <div className="mt-3.5 flex items-center justify-between gap-3 border-t border-line-soft pt-3 text-sm">
            <button
              type="button"
              onClick={() => set({ chain: '', vault: 'all' })}
              disabled={advancedActive === 0}
              className="font-bold text-muted hover:text-ink disabled:cursor-default disabled:opacity-40 disabled:hover:text-muted"
            >
              Reset
            </button>
            <button
              type="button"
              onClick={() => setOpen(false)}
              className="rounded-lg bg-active px-3.5 py-1.5 font-bold text-ink hover:bg-hover"
            >
              Done
            </button>
          </div>
        </div>
      )}

      {!open && chips.length > 0 && (
        <div className="flex flex-wrap items-center gap-1.5">
          {chips.map((chip) => (
            <span
              key={chip.key}
              className="inline-flex max-w-full items-center gap-1 rounded-full bg-accent-tint py-1 pr-1 pl-2.5 text-xs font-bold text-accent"
            >
              <span className="truncate">{chip.label}</span>
              <button
                type="button"
                onClick={chip.clear}
                aria-label={`Remove ${chip.label}`}
                className="grid size-4.5 shrink-0 place-items-center rounded-full hover:bg-accent hover:text-on-accent"
              >
                <CloseIcon size={10} />
              </button>
            </span>
          ))}
          <button
            type="button"
            onClick={() => onChange(EMPTY_FILTER)}
            className="px-1.5 py-1 text-xs font-bold text-muted hover:text-ink"
          >
            Clear all
          </button>
        </div>
      )}
    </div>
  )
}

/** Keep a picked option on screen even if the fleet no longer has it (a bot
 * was removed, or the URL came from someone else), so the operator can see
 * why the list is empty and undo it. */
function withPicked(options: FilterOption[], picked: string): FilterOption[] {
  return picked === '' || options.some((o) => o.value === picked)
    ? options
    : [...options, { value: picked, label: picked, count: 0 }]
}

function labelOf(options: FilterOption[], value: string): string {
  return options.find((o) => o.value === value)?.label ?? value
}

/** Free text. `/` jumps to it from anywhere on the page, Escape empties it. */
function SearchBox({ value, onChange }: { value: string; onChange: (v: string) => void }) {
  const ref = useRef<HTMLInputElement>(null)
  // What the box shows. `value` comes back through the URL, which can lag a
  // fast typist by a keystroke or two; rendering it directly reset the box to
  // that older text and dropped letters. So the box keeps its own text while
  // focused and only takes `value` when the change came from elsewhere (Clear
  // all, another tab), which is never while someone is typing in it.
  const [draft, setDraft] = useState(value)
  useEffect(() => {
    if (document.activeElement !== ref.current) setDraft(value)
  }, [value])
  const change = (next: string) => {
    setDraft(next)
    onChange(next)
  }
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== '/' || e.metaKey || e.ctrlKey || e.altKey) return
      const target = e.target as HTMLElement | null
      if (target?.closest('input, textarea, select, [contenteditable="true"]')) return
      e.preventDefault()
      ref.current?.focus()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [])

  return (
    <div className="relative order-1 min-w-0 flex-1 sm:min-w-56">
      <span className="pointer-events-none absolute inset-y-0 left-3 flex items-center text-faint">
        <SearchIcon />
      </span>
      <input
        ref={ref}
        type="search"
        enterKeyHint="search"
        autoComplete="off"
        spellCheck={false}
        value={draft}
        onChange={(e) => change(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === 'Escape' && draft !== '') {
            e.preventDefault()
            change('')
          }
        }}
        placeholder="Name, signer or currency"
        aria-label="Search bots by name, signer address or currency"
        className={`${CONTROL_H} w-full rounded-lg border border-line bg-surface pr-9 pl-9 text-sm text-ink transition placeholder:text-faint focus:border-accent focus:ring-3 focus:ring-accent/15 focus:outline-none [&::-webkit-search-cancel-button]:appearance-none`}
      />
      <span className="absolute inset-y-0 right-2 flex items-center">
        {draft !== '' ? (
          <button
            type="button"
            onClick={() => {
              change('')
              ref.current?.focus()
            }}
            aria-label="Clear search"
            className="grid size-6 place-items-center rounded-md text-faint hover:bg-hover hover:text-ink"
          >
            <CloseIcon size={12} />
          </button>
        ) : (
          <kbd className="pointer-events-none hidden rounded border border-line-soft bg-canvas px-1.5 font-mono text-[11px] leading-5 text-faint sm:block">
            /
          </kbd>
        )}
      </span>
    </div>
  )
}

/**
 * A small set of exclusive choices with a count on each: one radio group, so
 * "none of them" can't be picked. On the bar it fills the row on a phone and
 * sits at its own width from `sm` up; `stretch` keeps the segments equal at
 * every width, for the Filters panel.
 */
function Segmented<T extends string>({
  label,
  options,
  value,
  counts,
  onChange,
  stretch = false,
  className = '',
}: {
  label: string
  options: readonly SegmentOption<T>[]
  value: T
  counts: Record<T, number>
  onChange: (value: T) => void
  stretch?: boolean
  className?: string
}) {
  const name = useId()
  return (
    <div
      role="radiogroup"
      aria-label={label}
      className={`${CONTROL_H} flex min-w-0 rounded-lg bg-active p-0.5 ${className}`}
    >
      {options.map((option) => {
        const active = value === option.value
        return (
          <label
            key={option.value}
            className={`inline-flex min-w-0 flex-1 cursor-pointer items-center justify-center gap-1.5 rounded-md px-2.5 text-sm font-bold whitespace-nowrap transition has-[:focus-visible]:outline has-[:focus-visible]:outline-2 has-[:focus-visible]:outline-offset-2 has-[:focus-visible]:outline-accent ${
              stretch ? '' : 'sm:flex-none sm:px-3'
            } ${active ? 'bg-surface text-ink shadow-sm ring-1 ring-line-soft' : 'text-muted hover:text-ink'}`}
          >
            <input
              type="radio"
              name={name}
              value={option.value}
              checked={active}
              onChange={() => onChange(option.value)}
              className="sr-only"
            />
            {option.dot && <span aria-hidden className={`size-1.5 rounded-full ${option.dot}`} />}
            {option.short ? (
              <>
                <span className="sm:hidden">{option.short}</span>
                <span className="hidden sm:inline">{option.label}</span>
              </>
            ) : (
              option.label
            )}
            <span
              className={`text-xs font-normal tabular-nums ${active ? 'text-muted' : 'text-faint'}`}
            >
              {counts[option.value]}
            </span>
          </label>
        )
      })}
    </div>
  )
}

/** A caption over one panel control. A div, not a <label>: the vault field is a
 * radio group, and each control names itself with aria-label. */
function PanelField({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="min-w-0">
      <span className="mb-1.5 block text-[11px] font-bold tracking-wider text-faint uppercase">
        {label}
      </span>
      {children}
    </div>
  )
}

/** A native select (so phones get their own picker) dressed to match the bar. */
function FilterSelect({
  label,
  value,
  onChange,
  all,
  options,
}: {
  label: string
  value: string
  onChange: (value: string) => void
  all: string
  options: FilterOption[]
}) {
  return (
    <span className="relative block">
      <select
        aria-label={label}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        className={`${CONTROL_H} w-full cursor-pointer appearance-none truncate rounded-lg border bg-canvas pr-9 pl-3 text-sm text-ink transition focus:border-accent focus:ring-3 focus:ring-accent/15 focus:outline-none ${
          value === '' ? 'border-line' : 'border-accent/50 font-bold'
        }`}
      >
        <option value="">{all}</option>
        {options.map((o) => (
          <option key={o.value} value={o.value}>
            {o.label} · {o.count}
          </option>
        ))}
      </select>
      <span className="pointer-events-none absolute inset-y-0 right-3 flex items-center text-faint">
        <ChevronIcon />
      </span>
    </span>
  )
}

function SearchIcon() {
  return (
    <svg width="15" height="15" viewBox="0 0 16 16" fill="none" aria-hidden>
      <circle cx="7" cy="7" r="4.75" stroke="currentColor" strokeWidth="1.6" />
      <path d="M10.5 10.5 14 14" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
    </svg>
  )
}

function SlidersIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 16 16" fill="none" aria-hidden>
      <path
        d="M2 4h7M13 4h1M2 12h1M7 12h7"
        stroke="currentColor"
        strokeWidth="1.6"
        strokeLinecap="round"
      />
      <circle cx="11" cy="4" r="1.9" stroke="currentColor" strokeWidth="1.6" />
      <circle cx="5" cy="12" r="1.9" stroke="currentColor" strokeWidth="1.6" />
    </svg>
  )
}

function ChevronIcon({ className = '' }: { className?: string }) {
  return (
    <svg width="12" height="12" viewBox="0 0 12 12" fill="none" aria-hidden className={className}>
      <path
        d="m3 4.5 3 3 3-3"
        stroke="currentColor"
        strokeWidth="1.6"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  )
}

function CloseIcon({ size }: { size: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 12 12" fill="none" aria-hidden>
      <path d="m3 3 6 6M9 3 3 9" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" />
    </svg>
  )
}
