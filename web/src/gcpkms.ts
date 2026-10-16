/**
 * The regional Cloud KMS endpoint for a key version's location, or null for a
 * key in `global` or a name with no location. A regional endpoint only serves
 * keys in its own location, so this is the one to suggest when Verify measures
 * the global endpoint as slow.
 */
export function regionalKmsEndpoint(keyVersion: string): string | null {
  const location = /\/locations\/([a-z0-9-]+)\//.exec(keyVersion.trim())?.[1]
  return location && location !== 'global'
    ? `https://${location}-cloudkms.googleapis.com`
    : null
}

/** The ids inside a Cloud KMS key version resource name. */
export interface KeyVersionParts {
  project: string
  location: string
  keyRing: string
  key: string
}

const KEY_VERSION =
  /^projects\/([^/\s]+)\/locations\/([^/\s]+)\/keyRings\/([^/\s]+)\/cryptoKeys\/([^/\s]+)\/cryptoKeyVersions\/[^/\s]+$/

/** The project, location, key ring and key a full key version name points at, or null. */
export function parseKeyVersion(name: string): KeyVersionParts | null {
  const [, project, location, keyRing, key] =
    KEY_VERSION.exec(name.trim()) ?? []
  return project && location && keyRing && key
    ? { project, location, keyRing, key }
    : null
}

/** What the guide's commands use until the operator has pasted a key version. */
export const GUIDE_DEFAULTS: KeyVersionParts = {
  project: 'PROJECT_ID',
  location: 'us-east1',
  keyRing: 'stitch',
  key: 'operator',
}

/** The service account the guide creates, named for its one job. */
export const SIGNER_ACCOUNT = 'stitch-signer'

/** The service account's email in a project. */
export function signerEmail(project: string): string {
  return `${SIGNER_ACCOUNT}@${project}.iam.gserviceaccount.com`
}

const keyFlags = ({ keyRing, location }: KeyVersionParts) =>
  `--keyring ${keyRing} --location ${location}`

/** gcloud for an HSM secp256k1 signing key, and the line that prints its version's full name. */
export function createKeyCommands(p: KeyVersionParts): string {
  return [
    `gcloud config set project ${p.project}`,
    'gcloud services enable cloudkms.googleapis.com iam.googleapis.com',
    `gcloud kms keyrings create ${p.keyRing} --location ${p.location}`,
    `gcloud kms keys create ${p.key} ${keyFlags(p)} \\`,
    '  --purpose asymmetric-signing \\',
    '  --default-algorithm ec-sign-secp256k1-sha256 \\',
    '  --protection-level hsm',
    `gcloud kms keys versions list --key ${p.key} ${keyFlags(p)} \\`,
    '  --format="value(name)"',
  ].join('\n')
}

/** gcloud for a service account that can sign with this one key, and its JSON key. */
export function createSignerCommands(p: KeyVersionParts): string {
  const email = signerEmail(p.project)
  return [
    `gcloud iam service-accounts create ${SIGNER_ACCOUNT}`,
    `gcloud kms keys add-iam-policy-binding ${p.key} ${keyFlags(p)} \\`,
    `  --member serviceAccount:${email} \\`,
    '  --role roles/cloudkms.signerVerifier',
    `gcloud iam service-accounts keys create ${SIGNER_ACCOUNT}.json \\`,
    `  --iam-account ${email}`,
  ].join('\n')
}
