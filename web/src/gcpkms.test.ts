import { describe, expect, it } from 'vitest'
import {
  GUIDE_DEFAULTS,
  createKeyCommands,
  createSignerCommands,
  parseKeyVersion,
  regionalKmsEndpoint,
} from './gcpkms'

const version = (location: string) =>
  `projects/acme/locations/${location}/keyRings/stitch/cryptoKeys/operator/cryptoKeyVersions/1`

describe('regional Cloud KMS endpoint', () => {
  it("is the key's own location", () => {
    expect(regionalKmsEndpoint(version('us-east1'))).toBe(
      'https://us-east1-cloudkms.googleapis.com'
    )
    expect(regionalKmsEndpoint(`  ${version('nam-eur-asia1')} `)).toBe(
      'https://nam-eur-asia1-cloudkms.googleapis.com'
    )
  })
  it('is absent for a global key or a name without a location', () => {
    expect(regionalKmsEndpoint(version('global'))).toBeNull()
    expect(regionalKmsEndpoint('projects/acme/keyRings/stitch')).toBeNull()
    expect(regionalKmsEndpoint('')).toBeNull()
  })
})

describe('parseKeyVersion', () => {
  it('reads the ids out of a full key version name', () => {
    expect(parseKeyVersion(` ${version('europe-west1')}\n`)).toEqual({
      project: 'acme',
      location: 'europe-west1',
      keyRing: 'stitch',
      key: 'operator',
    })
  })
  it('rejects a key name without its version, a URL, or a partial path', () => {
    expect(
      parseKeyVersion(
        'projects/acme/locations/us-east1/keyRings/stitch/cryptoKeys/operator'
      )
    ).toBeNull()
    expect(
      parseKeyVersion(
        `https://cloudkms.googleapis.com/v1/${version('us-east1')}`
      )
    ).toBeNull()
    expect(parseKeyVersion('projects/acme/locations/us-east1')).toBeNull()
    expect(parseKeyVersion('')).toBeNull()
  })
})

describe('guide commands', () => {
  const parts = parseKeyVersion(version('europe-west1'))!
  it('create the key where the pasted version says it lives', () => {
    const cmd = createKeyCommands(parts)
    expect(cmd).toContain('gcloud config set project acme')
    expect(cmd).toContain(
      'gcloud kms keyrings create stitch --location europe-west1'
    )
    expect(cmd).toContain('--default-algorithm ec-sign-secp256k1-sha256')
    expect(cmd).toContain('--protection-level hsm')
  })
  it('grant the signer role on the key itself, to the account the JSON key is for', () => {
    const cmd = createSignerCommands(parts)
    expect(cmd).toContain(
      'gcloud kms keys add-iam-policy-binding operator --keyring stitch --location europe-west1'
    )
    expect(cmd).toContain(
      '--member serviceAccount:stitch-signer@acme.iam.gserviceaccount.com'
    )
    expect(cmd).toContain('--role roles/cloudkms.signerVerifier')
    expect(cmd).toContain(
      '--iam-account stitch-signer@acme.iam.gserviceaccount.com'
    )
  })
  it('fall back to placeholders before anything is pasted', () => {
    expect(createSignerCommands(GUIDE_DEFAULTS)).toContain(
      'stitch-signer@PROJECT_ID.iam.gserviceaccount.com'
    )
  })
})
