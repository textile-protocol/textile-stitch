import { describe, expect, it } from 'vitest'
import { regionalKmsEndpoint } from './gcpkms'

const version = (location: string) =>
  `projects/acme/locations/${location}/keyRings/stitch/cryptoKeys/operator/cryptoKeyVersions/1`

describe('regional Cloud KMS endpoint', () => {
  it("is the key's own location", () => {
    expect(regionalKmsEndpoint(version('us-east1'))).toBe('https://us-east1-cloudkms.googleapis.com')
    expect(regionalKmsEndpoint(`  ${version('nam-eur-asia1')} `)).toBe(
      'https://nam-eur-asia1-cloudkms.googleapis.com',
    )
  })
  it('is absent for a global key or a name without a location', () => {
    expect(regionalKmsEndpoint(version('global'))).toBeNull()
    expect(regionalKmsEndpoint('projects/acme/keyRings/stitch')).toBeNull()
    expect(regionalKmsEndpoint('')).toBeNull()
  })
})
