import { describe, expect, it } from 'vitest'
import {
  ARN_PLACEHOLDER,
  createKeyCommand,
  kmsSignPolicy,
  parseKmsArn,
  policyKeyArn,
} from './awskms'

const KEY =
  'arn:aws:kms:eu-west-1:111122223333:key/1234abcd-12ab-34cd-56ef-1234567890ab'
const ALIAS = 'arn:aws:kms:eu-west-1:111122223333:alias/stitch-operator'

const resource = (arn: string) =>
  JSON.parse(kmsSignPolicy(arn)).Statement[0].Resource

describe('parseKmsArn', () => {
  it('reads the region and account from a key or alias ARN', () => {
    expect(parseKmsArn(` ${KEY} `)).toEqual({
      partition: 'aws',
      region: 'eu-west-1',
      account: '111122223333',
    })
    expect(parseKmsArn(ALIAS)?.region).toBe('eu-west-1')
    expect(
      parseKmsArn('arn:aws-us-gov:kms:us-gov-west-1:111122223333:key/x')
        ?.partition
    ).toBe('aws-us-gov')
  })
  it('rejects a bare key id, an alias name, or another service', () => {
    expect(parseKmsArn('1234abcd-12ab-34cd-56ef-1234567890ab')).toBeNull()
    expect(parseKmsArn('alias/stitch-operator')).toBeNull()
    expect(parseKmsArn('arn:aws:s3:::bucket')).toBeNull()
    expect(parseKmsArn('')).toBeNull()
  })
})

describe('policyKeyArn', () => {
  it('is the trimmed key ARN, and null for an alias ARN or anything unparsed', () => {
    expect(policyKeyArn(` ${KEY}\n`)).toBe(KEY)
    expect(policyKeyArn(ALIAS)).toBeNull()
    expect(policyKeyArn('key/1234')).toBeNull()
  })
})

describe('kmsSignPolicy', () => {
  it('grants Sign and GetPublicKey on the pasted key only', () => {
    const policy = JSON.parse(kmsSignPolicy(KEY))
    expect(policy.Statement).toHaveLength(1)
    expect(policy.Statement[0].Action).toEqual(['kms:Sign', 'kms:GetPublicKey'])
    expect(resource(`  ${KEY}\n`)).toBe(KEY)
  })
  it('falls back to the placeholder for an alias or anything unparsed, since IAM matches key ARNs', () => {
    expect(resource(ALIAS)).toBe(ARN_PLACEHOLDER)
    expect(resource('')).toBe(ARN_PLACEHOLDER)
    expect(resource('not an arn')).toBe(ARN_PLACEHOLDER)
  })
})

describe('createKeyCommand', () => {
  it('creates a secp256k1 signing key in the given region', () => {
    const cmd = createKeyCommand('eu-west-1')
    expect(cmd).toContain('--region eu-west-1')
    expect(cmd).toContain('--key-spec ECC_SECG_P256K1 --key-usage SIGN_VERIFY')
    expect(cmd).toContain('--query KeyMetadata.Arn --output text')
  })
  it('asks for a region rather than guessing one', () => {
    expect(createKeyCommand()).toContain('--region <region>')
  })
})
