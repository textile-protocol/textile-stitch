import { CopyBlock, GuideStep } from './GuideSheet'

/**
 * Writes the API user's RSA key pair: the CSR Fireblocks asks for, and the
 * private key the form takes. `-nodes` leaves the key unencrypted, which the
 * bot needs: it reads the key unattended, with nobody there to type a
 * passphrase.
 */
const KEY_PAIR_COMMAND = [
  'openssl req -new -newkey rsa:4096 -nodes \\',
  '  -keyout fireblocks_secret.key -out fireblocks.csr \\',
  "  -subj '/O=stitch'",
].join('\n')

/**
 * Setting up a Fireblocks operator, from a workspace to the two values the
 * form asks for: the API key and its private key. The vault account and the
 * operator address aren't typed: the form lists the vaults and Verify proves
 * the address. The step people miss is the policy rule, so it gets its own
 * step, and the failures it causes are listed at the end.
 *
 * Mirrors docs/signer-fireblocks.md, which stays the reference for running the
 * bot by hand.
 */
export default function FireblocksGuide() {
  return (
    <div className="space-y-6">
      <p className="text-muted text-sm">
        About fifteen minutes in the Fireblocks console, plus approvals on the
        Fireblocks mobile app. The operator key stays in your vault as MPC key
        shares. The bot gets an API user that signs typed messages for one vault
        account.
      </p>

      <details className="border-line-soft rounded-lg border p-3 text-sm">
        <summary className="cursor-pointer font-bold">
          Trying it on a Developer Sandbox?
        </summary>
        <div className="text-muted mt-2 space-y-2">
          <p>
            A Sandbox pairs every API user with Fireblocks&apos; communal test
            co-signer, approves everything, and doesn&apos;t let you edit
            policies, so skip step 4. It has no Signer role: use{' '}
            <b>Non-Signing Admin</b>, which can sign there. If the form offers
            Automatic or Custom CSR, pick <b>Custom CSR</b> and upload yours.
          </p>
          <p>
            Set <b>Workspace region</b> to Sandbox. A pass there says nothing
            about your production policy, and its signing time is a best case.
          </p>
        </div>
      </details>

      <GuideStep n={1} title="Make the API key pair">
        <p>On your own machine, run:</p>
        <CopyBlock label="Generate the key pair" text={KEY_PAIR_COMMAND} />
        <p>
          It writes two files. <code>fireblocks.csr</code> is the public half
          you upload in step 2. <code>fireblocks_secret.key</code> is the
          secret: Fireblocks never sees it, and it goes into the{' '}
          <b>API private key</b> field.
        </p>
      </GuideStep>

      <GuideStep n={2} title="Create the API user">
        <p>
          In <b>Developer Center → API users</b>, press <b>Add API user</b>.
          Name it <code>stitch</code>, set the role to <b>Signer</b>, upload{' '}
          <code>fireblocks.csr</code> as the <b>CSR file</b>, pick your
          co-signer under <b>Co-signer setup</b>, and press <b>Add user</b>.
        </p>
        <p className="text-muted">
          Not Editor or Viewer: they list your vaults, then fail on the first
          signature. Not Embedded Wallet Signer either: it signs for end-user
          wallets, not vault accounts. No co-signer to pick? Mainnet needs one
          you run yourself. A testnet workspace can pair with Fireblocks&apos;
          Communal Test Co-signer.
        </p>
        <p>
          The Owner and your admin quorum approve the new user on the mobile
          app. Then copy its ID from the <b>API User (ID)</b> column into the{' '}
          <b>API key</b> field.
        </p>
      </GuideStep>

      <GuideStep n={3} title="Pick the vault account">
        <p>
          Any vault account under <b>Accounts → Vault</b> works. Its address is
          the same on every EVM chain, so one covers all your corridors. It
          needs an EVM asset wallet for the panel to read that address: if it
          has none, open it, press <b>Create wallet</b> and add <b>Ethereum</b>,
          or <b>ETH_TEST5</b> on a testnet workspace. It doesn&apos;t need a
          balance.
        </p>
      </GuideStep>

      <GuideStep n={4} title="Allow signing in the policy">
        <p>
          Open <b>Policies</b>, pick the <b>Typed message</b> policy and go to{' '}
          <b>Edit policy → Add rule</b>. Set <b>Initiator</b> to your API user,{' '}
          <b>Source</b> to the vault account, and <b>Action</b> to <b>Allow</b>.
          Then press <b>Publish policy</b> and have your approval group approve
          it.
        </p>
        <p className="text-muted">
          Allow, not Request approval: the venue stops listening for a quote
          about 5.5 s after it asks, which is no time for a person. And one
          vault, not all of them, so this key can&apos;t sign for anything else.
        </p>
        <p>
          The wizard sends the Permit2 approvals through Fireblocks as contract
          calls, so add a rule to the <b>Contract Call</b> policy for the same
          user and vault, set to <b>Request approval</b>, with{' '}
          <b>Transaction initiator can approve</b> off. Approve each call as the
          wizard sends it. It waits up to 3 minutes.
        </p>
        <p className="text-muted">
          Not Allow here: whoever holds the bot&apos;s key could then send any
          contract call from the vault.
        </p>
      </GuideStep>

      <GuideStep n={5} title="Load the vault and verify">
        <p>
          Close this guide. Set <b>Workspace region</b> to the region Fireblocks
          hosts your workspace in: it only answers on that host. Most are on
          Global, and the console doesn&apos;t show it, so ask your Fireblocks
          contact if you&apos;re not sure. Paste the API key, drop{' '}
          <code>fireblocks_secret.key</code> on the private key field, press{' '}
          <b>Load vault accounts</b>, pick the vault from step 3, and press{' '}
          <b>Verify</b>. The panel signs a throwaway message (it authorises
          nothing) and fills in the operator address it came from.
        </p>
        <p className="text-muted">
          It also shows how long one signature took. Over 400 ms is too slow to
          quote well, and the usual cause is a policy rule that waits for a
          person.
        </p>
        <p>
          Then press <b>Create</b>. The next step shows the operator address to
          send a little gas to for the approvals.
        </p>
      </GuideStep>

      <details className="border-line-soft rounded-lg border p-3 text-sm">
        <summary className="cursor-pointer font-bold">
          If Load vault accounts or Verify fails
        </summary>
        <dl className="mt-2 space-y-2">
          <div>
            <dt className="font-bold">returned 401</dt>
            <dd className="text-muted">
              The API key and private key don&apos;t belong together, the user
              isn&apos;t approved yet, or the workspace region is wrong.
            </dd>
          </div>
          <div>
            <dt className="font-bold">not a usable RSA PEM</dt>
            <dd className="text-muted">
              Load the whole <code>fireblocks_secret.key</code>, not{' '}
              <code>fireblocks.csr</code>.
            </dd>
          </div>
          <div>
            <dt className="font-bold">has no EVM wallet</dt>
            <dd className="text-muted">
              Add an EVM asset to the vault, as in step 3.
            </dd>
          </div>
          <div>
            <dt className="font-bold">
              refused the typed-message request: BLOCKED
            </dt>
            <dd className="text-muted">
              No Typed message rule, it names a different user or vault, or the
              policy isn&apos;t published yet. Step 4.
            </dd>
          </div>
        </dl>
      </details>
    </div>
  )
}
