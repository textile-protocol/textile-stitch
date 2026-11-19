import { describe, expect, it } from 'vitest'
import { allows, isDigestRef, profileOf, type Capability } from './profile'

const EVERY: Capability[] = [
  'addBot', 'removeBot', 'renameBot', 'recreate', 'migrateLayout', 'tagUpdate',
  'updateAll', 'panelUpdate', 'rawConfig', 'endpoints', 'poolChanges', 'rfqConnect',
  'vault', 'experimental', 'modulesEdit', 'withdraw', 'approve', 'oneShotRuns',
  'agentIdentity', 'feedPreview', 'composeExport',
]

describe('panel profile', () => {
  it('leaves the full Fleet exactly as it was', () => {
    for (const c of EVERY) expect(allows('full', c)).toBe(true)
    expect(allows('full', 'digestUpdate')).toBe(false)
  })

  it('hides everything hosted refuses and offers digest updates instead', () => {
    for (const c of EVERY) expect(allows('hosted', c)).toBe(false)
    expect(allows('hosted', 'digestUpdate')).toBe(true)
  })

  it('treats a session without a profile as the full panel', () => {
    expect(profileOf({})).toBe('full')
    expect(profileOf({ profile: null })).toBe('full')
    expect(profileOf({ profile: 'full' })).toBe('full')
    expect(profileOf({ profile: 'hosted' })).toBe('hosted')
  })

  it('accepts only sha256 digest references', () => {
    const repo = 'ghcr.io/textile-protocol/textile-stitch'
    expect(isDigestRef(`${repo}@sha256:${'a'.repeat(64)}`)).toBe(true)
    for (const image of [
      `${repo}:latest`,
      `${repo}:sha-abc1234`,
      `${repo}@sha256:${'a'.repeat(63)}`,
      `${repo}@sha256:${'A'.repeat(64)}`,
      `${repo}:latest@sha256:${'a'.repeat(64)}`,
      '',
    ]) {
      expect(isDigestRef(image)).toBe(false)
    }
  })
})
