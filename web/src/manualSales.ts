export interface ManualSale {
  id: string
  chainId: number
  vault: string
  taker: string
  corridorToken: string
  settlementToken: string
  corridorAmount: string
  minSettlement: string
  corridorSymbol: string
  settlementSymbol: string
  corridorDecimals: number
  settlementDecimals: number
  expiresAt: string
  createdAt: string
  status: 'open' | 'quoted' | 'submitted' | 'filled' | 'closed' | 'expired'
  txHash: string | null
  signedUntil: string | null
  buyerUrl: string
}
/** Never round money entered by an operator. Reject excess token precision. */
export function saleAtomic(raw: string, decimals: number): string | null {
  if (
    !Number.isInteger(decimals) ||
    decimals < 0 ||
    decimals > 18 ||
    !/^\d+(\.\d+)?$/.test(raw)
  )
    return null
  const [whole = '', fraction = ''] = raw.split('.')
  if (fraction.length > decimals || whole.length > 78) return null
  const amount = BigInt(whole + fraction.padEnd(decimals, '0'))
  return amount > 0n && amount < 2n ** 256n ? amount.toString() : null
}
export function saleDecimal(raw: string, decimals: number): string {
  const amount = BigInt(raw)
  const unit = 10n ** BigInt(decimals)
  const fraction = (amount % unit)
    .toString()
    .padStart(decimals, '0')
    .replace(/0+$/, '')
  return `${amount / unit}${fraction ? `.${fraction}` : ''}`
}
export const saleLabels: Record<ManualSale['status'], string> = {
  open: 'Ready for buyer',
  quoted: 'Buyer reviewing quote',
  submitted: 'Confirming payment',
  filled: 'Sale completed',
  closed: 'Request closed',
  expired: 'Request expired',
}
