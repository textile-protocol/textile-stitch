import {
  createKeyCommand,
  createUserCommands,
  kmsSignPolicy,
  parseKmsArn,
  policyKeyArn,
} from '../awskms'
import { CopyBlock, GuideStep } from './GuideSheet'

/**
 * Setting up an AWS KMS operator key, from an empty AWS account to a verified
 * signer. Each AWS step gives the console path and the CLI equivalent. The
 * policy is filled in with the key ARN already typed into the form, if any, so
 * it can be pasted as is.
 *
 * Mirrors docs/signer-awskms.md, which stays the reference for running the bot
 * by hand.
 */
export default function AwsKmsGuide({ keyId }: { keyId: string }) {
  const region = parseKmsArn(keyId)?.region
  return (
    <div className="space-y-6">
      <p className="text-muted text-sm">
        About ten minutes in the AWS console. The operator key is created inside
        AWS KMS and never leaves it. The bot gets an access key that can sign
        with that one key and do nothing else.
      </p>

      <GuideStep n={1} title="Create the key">
        <p>
          In the KMS console, first pick the region the bot will run in with the
          region selector at the top right. Keys live in one region, and every
          quote waits on a round trip to it. Then open{' '}
          <b>Customer managed keys → Create key</b>.
        </p>
        <ul className="list-disc space-y-1 pl-5">
          <li>
            Key type <b>Asymmetric</b>, key usage <b>Sign and verify</b>, key
            spec <b>ECC_SECG_P256K1</b>. Nothing else gives an Ethereum address,
            and the spec can&apos;t be changed later. Leave the advanced options
            as they are.
          </li>
          <li>
            Give it the alias <code>stitch-operator</code>. Key administrators
            are up to you. Leave key usage permissions empty: step 2 grants
            them. Click <b>Next</b> through to the review and press{' '}
            <b>Finish</b>.
          </li>
          <li>
            Open the key and copy the <b>ARN</b> under General configuration.
            That&apos;s the <b>Key ARN</b> field.
          </li>
        </ul>
        <details className="text-muted">
          <summary className="cursor-pointer">Or with the AWS CLI</summary>
          <div className="mt-2 space-y-2">
            <CopyBlock label="Create the key" text={createKeyCommand(region)} />
            <p>It prints the key ARN.</p>
          </div>
        </details>
      </GuideStep>

      <GuideStep n={2} title="Create a user that can only sign">
        <p>
          Open <b>IAM → Users → Create user</b> and name it{' '}
          <code>stitch-operator</code>. Leave console access unticked, skip the
          permissions page, and create the user. On the new user&apos;s{' '}
          <b>Permissions</b> tab, go to{' '}
          <b>Add permissions → Create inline policy</b>, switch the editor to{' '}
          <b>JSON</b>, and replace what&apos;s there with:
        </p>
        <CopyBlock label="IAM policy" text={kmsSignPolicy(keyId)} />
        <p className="text-muted">
          {policyKeyArn(keyId)
            ? 'This is filled in with the key ARN from the form.'
            : 'Paste the key ARN into the form first and this fills itself in. Use the key ARN, not an alias.'}{' '}
          Press <b>Next</b>, name the policy <code>stitch-kms-sign</code>, and
          press <b>Create policy</b>.
        </p>
        <details className="text-muted">
          <summary className="cursor-pointer">Or with the AWS CLI</summary>
          <div className="mt-2 space-y-2">
            <p>Save the policy above as policy.json, then:</p>
            <CopyBlock label="Create the user" text={createUserCommands()} />
          </div>
        </details>
      </GuideStep>

      <GuideStep n={3} title="Create an access key">
        <p>
          On the user&apos;s <b>Security credentials</b> tab, under Access keys,
          press <b>Create access key</b>. For the use case pick <b>Other</b>,
          press <b>Next</b>, then <b>Create access key</b>. The access key and
          secret access key it shows go into <b>Access key ID</b> and{' '}
          <b>Secret access key</b>. AWS shows the secret once, so copy it now or
          press <b>Download .csv file</b>.
        </p>
        <p className="text-muted">
          Whoever holds this key can sign as your operator until you deactivate
          it. Treat it like a hot-wallet private key.
        </p>
        <p className="text-muted">
          If AWS refuses to create the user or the access key, your organization
          may block IAM users. Ask your AWS admin.
        </p>
      </GuideStep>

      <GuideStep n={4} title="Paste and verify">
        <p>
          Close this guide, paste the key ARN, access key ID and secret into the
          form, and press <b>Verify</b>. The panel signs a throwaway message
          with the key (it authorises nothing) and fills in the operator address
          it controls.
        </p>
        <p className="text-muted">
          It also shows how long one signature took. Over 400 ms is too slow to
          quote well: run the bot in the key&apos;s region, or create the key
          closer to the bot.
        </p>
      </GuideStep>

      <GuideStep n={5} title="Create the bot">
        <p>
          Press <b>Create</b>. The next step shows the operator address to send
          a little gas to for the approvals.
        </p>
      </GuideStep>

      <details className="border-line-soft rounded-lg border p-3 text-sm">
        <summary className="cursor-pointer font-bold">If Verify fails</summary>
        <dl className="mt-2 space-y-2">
          <div>
            <dt className="font-bold">
              … but an Ethereum key has to be ECC_SECG_P256K1
            </dt>
            <dd className="text-muted">
              The key has the wrong spec. Create a new one as in step 1.
            </dd>
          </div>
          <div>
            <dt className="font-bold">AccessDeniedException</dt>
            <dd className="text-muted">
              The policy is missing, names a different key, or the key policy
              was replaced and doesn&apos;t let IAM grant access. A policy or
              access key created seconds ago can take a moment to work, so try
              again once before changing anything.
            </dd>
          </div>
          <div>
            <dt className="font-bold">
              UnrecognizedClientException or a signature error
            </dt>
            <dd className="text-muted">
              The access key is wrong, inactive, or doesn&apos;t match the
              secret.
            </dd>
          </div>
          <div>
            <dt className="font-bold">NotFoundException</dt>
            <dd className="text-muted">The ARN is mistyped.</dd>
          </div>
        </dl>
      </details>
    </div>
  )
}
