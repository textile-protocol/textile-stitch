import {
  GUIDE_DEFAULTS,
  SIGNER_ACCOUNT,
  createKeyCommands,
  createSignerCommands,
  parseKeyVersion,
  signerEmail,
} from '../gcpkms'
import { CopyBlock, GuideStep } from './GuideSheet'

const CONSOLE = 'https://console.cloud.google.com'

function ConsoleLink({ path, children }: { path: string; children: string }) {
  return (
    <a
      href={`${CONSOLE}${path}`}
      target="_blank"
      rel="noreferrer"
      className="font-bold text-accent underline-offset-2 hover:underline"
    >
      {children}
    </a>
  )
}

/**
 * Setting up a Cloud KMS operator key, from a Google Cloud project to the two
 * values the form asks for: the key version name and a service-account JSON
 * key. Each step gives the console path and the gcloud equivalent. Once a key
 * version is pasted, the commands use its project, location, key ring and key.
 *
 * Mirrors docs/signer-gcpkms.md, which stays the reference for running the bot
 * by hand.
 */
export default function GcpKmsGuide({ keyVersion }: { keyVersion: string }) {
  const pasted = parseKeyVersion(keyVersion)
  const parts = pasted ?? GUIDE_DEFAULTS
  return (
    <div className="space-y-6">
      <p className="text-muted text-sm">
        About fifteen minutes in the Google Cloud console. The operator key is
        created inside Cloud HSM and never leaves it. The bot gets a service
        account that can sign with that one key and do nothing else.
      </p>

      <GuideStep n={1} title="Before you start">
        <p>
          You need a project with billing enabled, and to be its Owner or a
          Cloud KMS Admin on it. An HSM key costs about $2.50 a month plus a
          small fee per signature.
        </p>
        <p>
          Enable the{' '}
          <ConsoleLink path="/apis/enableflow?apiid=cloudkms.googleapis.com">
            Cloud KMS API
          </ConsoleLink>{' '}
          and the{' '}
          <ConsoleLink path="/apis/enableflow?apiid=iam.googleapis.com">
            IAM API
          </ConsoleLink>{' '}
          in that project.
        </p>
      </GuideStep>

      <GuideStep n={2} title="Create the key">
        <p>
          Open <ConsoleLink path="/security/kms">Key Management</ConsoleLink>{' '}
          and press <b>Create key ring</b>. Name it <code>stitch</code> and set{' '}
          <b>Key ring location</b> to the region the bot will run in, like{' '}
          <code>us-east1</code>. Every quote waits on a round trip there, and
          the location can&apos;t be changed later.
        </p>
        <p>
          Open the new key ring and press <b>Create key</b>:
        </p>
        <ul className="list-disc space-y-1 pl-5">
          <li>
            Key name <code>operator</code>, protection level <b>HSM</b>, key
            material <b>Generated key</b>, purpose <b>Asymmetric sign</b>.
          </li>
          <li>
            Algorithm: the <b>secp256k1</b> one (EC_SIGN_SECP256K1_SHA256).
            Nothing else gives an Ethereum address, and the algorithm can&apos;t
            be changed later. It only appears with protection level HSM.
          </li>
          <li>
            Press <b>Create</b>.
          </li>
        </ul>
        <details className="text-muted">
          <summary className="cursor-pointer">Or with gcloud</summary>
          <div className="mt-2 space-y-2">
            <CopyBlock label="Create the key" text={createKeyCommands(parts)} />
            <p>The last line prints the key version name for step 3.</p>
          </div>
        </details>
      </GuideStep>

      <GuideStep n={3} title="Copy the key version name">
        <p>
          Open the key. Its first version starts as <b>Pending generation</b>;
          wait for it to show <b>Enabled</b>. Then open the <b>More</b> menu
          (three dots) on that version&apos;s row and pick{' '}
          <b>Copy resource name</b>. That&apos;s the <b>Key version</b> field.
        </p>
        <p className="text-muted">
          Use the version row&apos;s menu, not the key&apos;s. The name has to
          end in <code>/cryptoKeyVersions/</code> and a number.
        </p>
      </GuideStep>

      <GuideStep n={4} title="Create a service account that can only sign">
        <p>
          Open{' '}
          <ConsoleLink path="/iam-admin/serviceaccounts">
            Service accounts
          </ConsoleLink>
          , press <b>Create service account</b>, name it{' '}
          <code>{SIGNER_ACCOUNT}</code>, and press <b>Done</b>. Don&apos;t give
          it any project roles: it gets access to this one key only.
        </p>
        <p>
          Back in Key Management, open the <code>{parts.keyRing}</code> key ring
          and tick the checkbox next to <code>{parts.key}</code>. In the info
          panel on the right (press <b>Show info panel</b> if it&apos;s hidden),
          press <b>Add principal</b>. Enter the service account&apos;s email:
        </p>
        <CopyBlock
          label="Service account email"
          text={signerEmail(parts.project)}
        />
        <p>
          {pasted
            ? 'This is filled in with the project from the form. '
            : 'Swap PROJECT_ID for your project id, or paste the key version into the form first and this fills itself in. '}
          Pick the role <b>Cloud KMS CryptoKey Signer/Verifier</b> and press{' '}
          <b>Save</b>.
        </p>
        <p className="text-muted">
          Not Cloud KMS CryptoKey Signer: that one can&apos;t read the public
          key, which the bot checks on every start.
        </p>
      </GuideStep>

      <GuideStep n={5} title="Create the JSON key">
        <p>
          In Service accounts, click <code>{SIGNER_ACCOUNT}</code>, open the{' '}
          <b>Keys</b> tab, and go to <b>Add key → Create new key</b>. Pick{' '}
          <b>JSON</b> and press <b>Create</b>. The file downloads once and
          can&apos;t be downloaded again. Load it into the{' '}
          <b>Service account key (JSON)</b> field.
        </p>
        <p className="text-muted">
          Whoever holds this file can sign as your operator until you delete the
          key. Treat it like a hot-wallet private key.
        </p>
        <p className="text-muted">
          If Google says <b>Service account key creation is disabled</b>, your
          organization enforces the{' '}
          <code>iam.managed.disableServiceAccountKeyCreation</code> policy
          (older organizations:{' '}
          <code>iam.disableServiceAccountKeyCreation</code>). It&apos;s on by
          default for organizations created since May 2024. An Organization
          Policy Administrator has to allow keys for this project.
        </p>
        <details className="text-muted">
          <summary className="cursor-pointer">
            Or with gcloud (steps 4 and 5)
          </summary>
          <div className="mt-2 space-y-2">
            <CopyBlock
              label="Create the service account"
              text={createSignerCommands(parts)}
            />
            <p>
              It writes <code>{SIGNER_ACCOUNT}.json</code> to the current
              folder.
            </p>
          </div>
        </details>
      </GuideStep>

      <GuideStep n={6} title="Verify">
        <p>
          Close this guide and press <b>Verify</b>. The panel signs a throwaway
          message with the key (it authorises nothing) and fills in the operator
          address it controls. The protection level should say HSM.
        </p>
        <p className="text-muted">
          It also shows how long one signature took. Over 400 ms is too slow to
          quote well: run the bot in the key&apos;s region, or set the regional
          endpoint the form suggests.
        </p>
        <p>
          Then press <b>Create</b>. The next step shows the operator address to
          send a little gas to for the approvals.
        </p>
      </GuideStep>

      <details className="border-line-soft rounded-lg border p-3 text-sm">
        <summary className="cursor-pointer font-bold">If Verify fails</summary>
        <dl className="mt-2 space-y-2">
          <div>
            <dt className="font-bold">
              … but an Ethereum key has to be EC_SIGN_SECP256K1_SHA256
            </dt>
            <dd className="text-muted">
              The key has the wrong algorithm. Create a new one as in step 2.
            </dd>
          </div>
          <div>
            <dt className="font-bold">names a key, not a key version</dt>
            <dd className="text-muted">
              Copy the name from the version row&apos;s menu, as in step 3.
            </dd>
          </div>
          <div>
            <dt className="font-bold">Cloud KMS returned 403</dt>
            <dd className="text-muted">
              The role is missing on the key or went to a different account, or
              the Cloud KMS API isn&apos;t enabled. A service account, role or
              JSON key made in the last minute or two may not work yet, so try
              again once before changing anything.
            </dd>
          </div>
          <div>
            <dt className="font-bold">Cloud KMS returned 404</dt>
            <dd className="text-muted">
              The key version name is mistyped, or the regional endpoint
              doesn&apos;t match the key&apos;s location. Leave the endpoint
              empty to use the global one.
            </dd>
          </div>
          <div>
            <dt className="font-bold">
              Workload Identity Federation credentials are not supported
            </dt>
            <dd className="text-muted">
              The panel needs a service-account JSON key from step 5.
            </dd>
          </div>
        </dl>
      </details>
    </div>
  )
}
