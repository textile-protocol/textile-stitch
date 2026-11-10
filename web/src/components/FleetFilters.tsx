import { useId } from 'react'
import {
  type FilterOption,
  type FleetFilter,
  type LiveFilter,
  EMPTY_FILTER,
  isFiltered,
} from '../fleetFilter'
import { Input, Select } from './ui'

const LIVE_OPTIONS: readonly { value: LiveFilter; label: string; dot: string | null }[] = [
  { value: 'all', label: 'All', dot: null },
  { value: 'live', label: 'Live', dot: 'bg-success' },
  { value: 'not-live', label: 'Not live', dot: 'bg-faint' },
]

/**
 * The fleet's filter bar: free text, live or not, chain and vault. A dropdown
 * with a single choice filters nothing, so chain and vault only show once the
 * fleet spans more than one of them.
 */
export default function FleetFilters({
  filter,
  onChange,
  liveCounts,
  chains,
  vaults,
}: {
  filter: FleetFilter
  onChange: (filter: FleetFilter) => void
  liveCounts: Record<LiveFilter, number>
  chains: FilterOption[]
  vaults: FilterOption[]
}) {
  const set = (patch: Partial<FleetFilter>) => onChange({ ...filter, ...patch })
  // Keep a picked option on screen even if the fleet no longer has it (a bot
  // was removed, or the URL came from someone else), so the operator can see
  // why the list is empty and undo it.
  const withPicked = (options: FilterOption[], picked: string) =>
    picked === '' || options.some((o) => o.value === picked)
      ? options
      : [...options, { value: picked, label: picked, count: 0 }]
  const chainChoices = withPicked(chains, filter.chain)
  const vaultChoices = withPicked(vaults, filter.vault)

  return (
    <div className="flex flex-col gap-2 sm:flex-row sm:flex-wrap sm:items-center">
      <Input
        type="search"
        value={filter.query}
        onChange={(e) => set({ query: e.target.value })}
        placeholder="Search name, signer address or currency"
        aria-label="Search bots"
        className="sm:min-w-56 sm:!w-auto sm:flex-1"
      />
      <LiveSwitch value={filter.live} counts={liveCounts} onChange={(live) => set({ live })} />
      {/* Stacked on a phone, the two dropdowns and Clear share one row. */}
      <div className="flex items-center gap-2">
        {chainChoices.length > 1 && (
          <Select
            value={filter.chain}
            onChange={(e) => set({ chain: e.target.value })}
            aria-label="Chain"
            className="min-w-0 flex-1 sm:!w-auto sm:flex-none"
          >
            <option value="">All chains</option>
            {chainChoices.map((o) => (
              <option key={o.value} value={o.value}>
                {o.label} ({o.count})
              </option>
            ))}
          </Select>
        )}
        {vaultChoices.length > 1 && (
          <Select
            value={filter.vault}
            onChange={(e) => set({ vault: e.target.value })}
            aria-label="Vault"
            className="min-w-0 flex-1 sm:!w-auto sm:flex-none"
          >
            <option value="">All vaults</option>
            {vaultChoices.map((o) => (
              <option key={o.value} value={o.value}>
                {o.label} ({o.count})
              </option>
            ))}
          </Select>
        )}
        {isFiltered(filter) && (
          <button
            type="button"
            onClick={() => onChange(EMPTY_FILTER)}
            className="shrink-0 px-1 text-sm font-bold text-muted hover:text-ink"
          >
            Clear
          </button>
        )}
      </div>
    </div>
  )
}

/** All, Live, Not live: one radio group, so "neither" can't be picked. */
function LiveSwitch({
  value,
  counts,
  onChange,
}: {
  value: LiveFilter
  counts: Record<LiveFilter, number>
  onChange: (value: LiveFilter) => void
}) {
  const name = useId()
  return (
    <div role="radiogroup" aria-label="Live" className="bg-active inline-flex self-start rounded-lg p-0.5 sm:self-auto">
      {LIVE_OPTIONS.map((option) => (
        <label
          key={option.value}
          className={`inline-flex cursor-pointer items-center gap-1.5 rounded-md px-3 py-1 text-sm font-bold transition has-[:focus-visible]:outline has-[:focus-visible]:outline-2 has-[:focus-visible]:outline-offset-2 has-[:focus-visible]:outline-accent ${value === option.value ? 'bg-canvas text-ink ring-line-soft shadow-sm ring-1' : 'text-muted hover:text-ink'}`}
        >
          <input
            type="radio"
            name={name}
            value={option.value}
            checked={value === option.value}
            onChange={() => onChange(option.value)}
            className="sr-only"
          />
          {option.dot && <span aria-hidden className={`size-1.5 rounded-full ${option.dot}`} />}
          {option.label}
          <span className="text-xs font-normal tabular-nums text-faint">{counts[option.value]}</span>
        </label>
      ))}
    </div>
  )
}
