import { describe, expect, it } from 'vitest'
import {
  MAX_RPC_FALLBACKS,
  addFallback,
  canAddFallback,
  removeFallback,
  setFallback,
} from './rpcFallbacks'

describe('rpc fallbacks', () => {
  it('adds empty rows up to the limit and no further', () => {
    const full = Array.from({ length: MAX_RPC_FALLBACKS }, (_, i) => `https://rpc${i}.example`)
    expect(addFallback([])).toEqual([''])
    expect(canAddFallback(full)).toBe(false)
    expect(addFallback(full)).toEqual(full)
  })

  it('edits and removes one row without touching the input', () => {
    const urls = ['https://a.example', 'https://b.example']
    expect(setFallback(urls, 1, 'https://c.example')).toEqual([
      'https://a.example',
      'https://c.example',
    ])
    expect(removeFallback(urls, 0)).toEqual(['https://b.example'])
    expect(urls).toEqual(['https://a.example', 'https://b.example'])
  })
})
