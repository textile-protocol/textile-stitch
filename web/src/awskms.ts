/** The parts of an AWS KMS key ARN the setup guide fills into its commands. */
export interface KmsArn {
  partition: string
  region: string
  account: string
}

const ARN = /^arn:(aws[a-z-]*):kms:([a-z0-9-]+):(\d{12}):(?:key|alias)\/\S+$/

/** The partition, region and account of a key or alias ARN, or null for anything else. */
export function parseKmsArn(arn: string): KmsArn | null {
  const [, partition, region, account] = ARN.exec(arn.trim()) ?? []
  return partition && region && account ? { partition, region, account } : null
}

/** Stands in for the key ARN in the guide until the operator has pasted one. */
export const ARN_PLACEHOLDER = 'arn:aws:kms:<region>:<account>:key/<key-id>'

/**
 * The ARN an IAM policy can name for this key, or null. Only a key ARN works:
 * IAM matches Resource against the key's own ARN, so an alias there grants
 * nothing.
 */
export function policyKeyArn(arn: string): string | null {
  return parseKmsArn(arn) && /:key\//.test(arn) ? arn.trim() : null
}

/**
 * The IAM policy the bot needs: Sign to sign, GetPublicKey for the startup
 * check that the key controls operator_address. Scoped to this one key, with
 * the placeholder standing in until there is a key ARN to name.
 */
export function kmsSignPolicy(arn: string): string {
  const resource = policyKeyArn(arn) ?? ARN_PLACEHOLDER
  return JSON.stringify(
    {
      Version: '2012-10-17',
      Statement: [
        {
          Effect: 'Allow',
          Action: ['kms:Sign', 'kms:GetPublicKey'],
          Resource: resource,
        },
      ],
    },
    null,
    2
  )
}

/**
 * The CLI that creates an Ethereum-compatible key and prints its ARN. The
 * region is the pasted key's own, or a placeholder: there is no safe default,
 * since the key has to live where the bot runs.
 */
export function createKeyCommand(region = '<region>'): string {
  return [
    `aws kms create-key --region ${region} \\`,
    '  --key-spec ECC_SECG_P256K1 --key-usage SIGN_VERIFY \\',
    '  --description "Stitch operator key" \\',
    '  --query KeyMetadata.Arn --output text',
  ].join('\n')
}

/** The CLI for a sign-only IAM user and its access key. The policy is read from a file. */
export function createUserCommands(user = 'stitch-operator'): string {
  return [
    `aws iam create-user --user-name ${user}`,
    `aws iam put-user-policy --user-name ${user} \\`,
    '  --policy-name stitch-kms-sign --policy-document file://policy.json',
    `aws iam create-access-key --user-name ${user}`,
  ].join('\n')
}
