// Which panel this is: the full Fleet every install runs, or the opt-in hosted
// profile on a Terraform-built client host. The server refuses what hosted
// doesn't allow (src/panel/hosted/policy.rs); this only hides it, so nobody
// clicks a button that can only fail.

import { createContext, useContext } from 'react'

export type PanelProfile = 'full' | 'hosted'

/** Things the UI offers that a hosted panel may not. */
export type Capability =
  | 'addBot'
  | 'removeBot'
  | 'renameBot'
  | 'recreate'
  | 'migrateLayout'
  | 'tagUpdate'
  | 'digestUpdate'
  | 'updateAll'
  | 'panelUpdate'
  | 'rawConfig'
  | 'endpoints'
  | 'poolChanges'
  | 'rfqConnect'
  | 'vault'
  | 'experimental'
  | 'modulesEdit'
  | 'withdraw'
  | 'approve'
  | 'oneShotRuns'
  | 'agentIdentity'
  | 'feedPreview'
  | 'composeExport'

/** Everything a hosted panel offers beyond status, logs, change log and lifecycle. */
const HOSTED: ReadonlySet<Capability> = new Set<Capability>(['digestUpdate'])

/** Only on hosted: Update/Rollback by digest replaces the tag-based ones. */
const HOSTED_ONLY: ReadonlySet<Capability> = new Set<Capability>(['digestUpdate'])

export function allows(profile: PanelProfile, capability: Capability): boolean {
  return profile === 'hosted' ? HOSTED.has(capability) : !HOSTED_ONLY.has(capability)
}

/** A session from an older panel has no profile: that panel is the full one. */
export function profileOf(session: { profile?: PanelProfile | null }): PanelProfile {
  return session.profile === 'hosted' ? 'hosted' : 'full'
}

/** `<repository>@sha256:<64 hex>`, the only image reference a hosted panel takes. */
export function isDigestRef(image: string): boolean {
  return /^[a-z0-9][a-z0-9./_-]*@sha256:[0-9a-f]{64}$/.test(image.trim())
}

export const ProfileContext = createContext<PanelProfile>('full')

export function useProfile(): PanelProfile {
  return useContext(ProfileContext)
}

export function useAllows(capability: Capability): boolean {
  return allows(useProfile(), capability)
}
