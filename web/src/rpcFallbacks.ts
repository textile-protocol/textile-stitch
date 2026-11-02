/**
 * Editing the optional backup RPC list on the Corridors tab. The bot tries
 * its primary RPC first, then these in order, whenever an endpoint can't be
 * reached. Mirrors `MAX_RPC_FALLBACKS` in `src/chain/rpc.rs`.
 */
export const MAX_RPC_FALLBACKS = 3

export const canAddFallback = (urls: readonly string[]): boolean =>
  urls.length < MAX_RPC_FALLBACKS

/** A new empty row at the end, unless the list is already full. */
export const addFallback = (urls: readonly string[]): string[] =>
  canAddFallback(urls) ? [...urls, ''] : [...urls]

export const setFallback = (urls: readonly string[], index: number, url: string): string[] =>
  urls.map((current, i) => (i === index ? url : current))

export const removeFallback = (urls: readonly string[], index: number): string[] =>
  urls.filter((_, i) => i !== index)
