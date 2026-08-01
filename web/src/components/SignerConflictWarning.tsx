import { useEffect, useState } from 'react'
import { ApiError, api } from '../api'
import { Banner } from './ui'
import { buildSigner, isSignerComplete, type SignerState } from './SignerFields'
import { shortAddress } from '../format'

export interface SignerConflict {
  name: string
  chainId: number | null
  operatorAddress: string | null
  /**
   * Sibling is live with taker/closer on — change-signer (while this bot is up)
   * or Start will refuse until it's stopped.
   */
  blocksLiveSwitch: boolean
}

/**
 * Warn when the chosen signer already belongs to another bot on the same chain.
 *
 * Sharing one wallet across two bots on one chain races nonces and quotes. The
 * create / change-signer flows still allow it (an operator may be mid-migration),
 * but the warning has to be loud before they confirm.
 */
export default function SignerConflictWarning({
  chainId,
  signer,
  excludeBot,
}: {
  chainId: number | null | undefined
  signer: SignerState
  /** Bot being re-signed — it already owns this wallet. */
  excludeBot?: string
}) {
  const [conflicts, setConflicts] = useState<SignerConflict[] | null>(null)
  const [address, setAddress] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    if (chainId == null || !isSignerComplete(signer)) {
      setConflicts(null)
      setAddress(null)
      setError(null)
      return
    }
    let cancelled = false
    const timer = setTimeout(() => {
      api
        .checkSigner({
          chainId,
          // Wire shape, not the UI state — local keys need privateKey/seedPhrase,
          // and MPC fields are flat, matching SignerRequest on the server.
          signer: buildSigner(signer),
          excludeBot,
        })
        .then((r) => {
          if (cancelled) return
          setConflicts(r.conflicts)
          setAddress(r.operatorAddress)
          setError(null)
        })
        .catch((e) => {
          if (cancelled) return
          // A bad key surfaces here before create — useful, but don't block the
          // form; create will refuse with the same message.
          setConflicts(null)
          setAddress(null)
          setError(e instanceof ApiError ? e.message : String(e))
        })
    }, 250)
    return () => {
      cancelled = true
      clearTimeout(timer)
    }
  }, [chainId, signer, excludeBot])

  if (error) {
    return (
      <Banner tone="warning">
        Couldn&apos;t check the fleet for wallet conflicts: {error}
      </Banner>
    )
  }
  if (!conflicts || conflicts.length === 0) return null

  const names = conflicts.map((c) => c.name).join(', ')
  const blockers = conflicts.filter((c) => c.blocksLiveSwitch).map((c) => c.name)
  return (
    <Banner tone="warning">
      Another bot in this fleet already uses{' '}
      <code title={address ?? undefined}>
        {address ? shortAddress(address) : 'this wallet'}
      </code>{' '}
      on chain {chainId}: <strong>{names}</strong>. Sharing one wallet across bots
      on the same chain races nonces and will cause issues — use a different
      signer.
      {blockers.length > 0 && (
        <>
          {' '}
          <strong>{blockers.join(', ')}</strong> {blockers.length === 1 ? 'is' : 'are'}{' '}
          live with taker/closer on — stop {blockers.length === 1 ? 'it' : 'them'}{' '}
          before switching a running bot onto this wallet.
        </>
      )}
    </Banner>
  )
}
