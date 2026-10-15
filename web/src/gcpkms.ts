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
