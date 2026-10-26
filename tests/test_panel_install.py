"""Exercise panel enrollment and upgrades without Docker or network side effects."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
HASH = '$argon2id$v=19$m=19456,t=2,p=1$test-salt$test-hash'
PASSWORD = 'a separate panel password'


class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        self.install = self.root / 'panel'
        self.install.mkdir()
        self.log = self.root / 'docker.jsonl'
        self.write_tool('docker', '''
import json, os, sys
with open(os.environ['DOCKER_TEST_LOG'], 'a') as f:
    f.write(json.dumps(sys.argv[1:]) + '\\n')
if sys.argv[1] == 'run':
    if sys.stdin.read().strip() != os.environ['EXPECTED_PASSWORD']:
        sys.exit('unexpected password on stdin')
    print(os.environ['TEST_HASH'])
elif 'status' in sys.argv:
    print('{"Self":{"DNSName":"stitch-panel.test.ts.net."}}')
''')
        self.write_tool('curl', '''
import os, pathlib, shutil, sys
url = sys.argv[2]
name = pathlib.PurePosixPath(url).name
if name not in ('docker-compose.panel.yml', 'docker-compose.panel.local.yml', 'panel-serve.json', 'panel-tailscale.sh'):
    sys.exit('unexpected download: ' + url)
source = pathlib.Path(os.environ['PANEL_TEST_SOURCE'])
if name in ('panel-serve.json', 'panel-tailscale.sh'):
    source /= 'deploy'
shutil.copyfile(source / name, sys.argv[sys.argv.index('-o') + 1])
''')
        self.env = {
            'PATH': str(self.bin) + os.pathsep + os.environ['PATH'],
            'HOME': str(self.root),
            'PANEL_DIR': str(self.install),
            'PANEL_BOTS_DIR': str(self.root / 'bots'),
            'PANEL_MODE': 'server',
            'TS_AUTHKEY': 'tskey-auth-test',
            'STITCH_REF': 'a' * 40,
            'PANEL_IMAGE': 'example.test/panel:sha-' + 'a' * 40,
            'STITCH_REQUIRE_PINNED': '1',
            'DOCKER_TEST_LOG': str(self.log),
            'PANEL_TEST_SOURCE': str(ROOT),
            'TEST_HASH': HASH,
            'EXPECTED_PASSWORD': PASSWORD,
        }

    def write_tool(self, name, source):
        path = self.bin / name
        path.write_text('#!' + sys.executable + '\n' + source)
        path.chmod(0o755)

    def existing(self, hash_line=None, mode='server'):
        # Legacy deployments may have no explicit PANEL_MODE.
        lines = ['TS_AUTHKEY=tskey-auth-test', 'PANEL_USERS=alice@example.com']
        if mode == 'local':
            lines = ['PANEL_MODE=local']
        if hash_line is not None:
            lines.append('PANEL_PASSWORD_HASH=' + hash_line)
        (self.install / '.env').write_text('\n'.join(lines) + '\n')
        (self.install / '.env').chmod(0o600)

    def run_installer(self, **env):
        result = subprocess.run(
            ['sh', str(ROOT / 'install-panel.sh')],
            env={**self.env, **env},
            capture_output=True, text=True, start_new_session=True, timeout=30,
        )
        calls = [json.loads(line) for line in self.log.read_text().splitlines()]
        return result, calls

    def assert_started(self, result, calls):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue(any('up' in call for call in calls), calls)
        self.assertNotIn(PASSWORD, result.stdout + result.stderr)
        self.assertNotIn(PASSWORD, (self.install / '.env').read_text())
        self.assertEqual((self.install / '.env').stat().st_mode & 0o777, 0o600)

    def test_new_server_requires_password_without_tailnet_user_input(self):
        result, calls = self.run_installer(PANEL_PASSWORD=PASSWORD)
        self.assert_started(result, calls)
        self.assertIn("PANEL_PASSWORD_HASH='" + HASH + "'", (self.install / '.env').read_text())
        self.assertEqual(sum(call[0] == 'run' for call in calls), 1)

    def test_passwordless_server_upgrade_preserves_access_and_offers_ui_setup(self):
        self.existing()
        result, calls = self.run_installer()
        self.assert_started(result, calls)
        self.assertIn('Set panel password', result.stdout)
        self.assertIn('PANEL_LEGACY_IDENTITY_AUTH=1', (self.install / '.env').read_text())
        self.assertFalse(any(call[0] == 'run' for call in calls))
        shim = self.install / 'deploy/panel-tailscale.sh'
        self.assertEqual(shim.read_bytes(), (ROOT / 'deploy/panel-tailscale.sh').read_bytes())
        self.assertEqual(shim.stat().st_mode & 0o777, 0o755)

    def test_empty_hash_on_legacy_upgrade_uses_ui_setup(self):
        self.existing("''")
        result, calls = self.run_installer()
        self.assert_started(result, calls)
        self.assertIn('Set panel password', result.stdout)

    def test_new_server_without_a_password_never_starts(self):
        result, calls = self.run_installer()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('PANEL_PASSWORD is not set', result.stderr)
        self.assertFalse(any('up' in call for call in calls))

    def test_migrated_install_keeps_ui_password_after_legacy_flag_is_disabled(self):
        self.existing()
        fleet = self.root / 'migrated-fleet'
        fleet.mkdir()
        # The binary validates the file; the installer must leave it untouched.
        saved = fleet / '.stitch-panel-password.json'
        saved.write_text('saved by panel')
        with (self.install / '.env').open('a') as f:
            f.write('PANEL_LEGACY_IDENTITY_AUTH=0\nPANEL_BOTS_DIR=' + str(fleet) + '\n')
        result, calls = self.run_installer()
        self.assert_started(result, calls)
        self.assertIn('Keeping the password saved in the panel UI', result.stdout)
        self.assertFalse(any(call[0] == 'run' for call in calls))
        self.assertEqual(saved.read_text(), 'saved by panel')

        result, calls = self.run_installer(PANEL_PASSWORD=PASSWORD)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('PANEL_PASSWORD cannot replace it', result.stderr)
        self.assertEqual(saved.read_text(), 'saved by panel')

    def test_existing_hash_is_preserved_without_rehashing(self):
        self.existing("'" + HASH + "'")
        result, calls = self.run_installer()
        self.assert_started(result, calls)
        self.assertFalse(any(call[0] == 'run' for call in calls), calls)
        self.assertIn(HASH, (self.install / '.env').read_text())

    def test_upgrade_can_replace_a_password(self):
        self.existing("'$argon2id$old'")
        with (self.install / '.env').open('a') as f:
            f.write('PANEL_LEGACY_IDENTITY_AUTH=1\n')
        result, calls = self.run_installer(PANEL_PASSWORD=PASSWORD)
        self.assert_started(result, calls)
        env = (self.install / '.env').read_text()
        self.assertNotIn('$argon2id$old', env)
        self.assertIn('PANEL_LEGACY_IDENTITY_AUTH=0', env)
        self.assertEqual(env.count('PANEL_PASSWORD_HASH='), 1)

    def test_short_password_never_starts_server(self):
        result, calls = self.run_installer(PANEL_PASSWORD='too-short')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('at least 12 characters', result.stderr)
        self.assertFalse(any('up' in call for call in calls), calls)

    def test_local_password_install_still_works(self):
        result, calls = self.run_installer(PANEL_MODE='local', PANEL_PASSWORD=PASSWORD)
        self.assert_started(result, calls)
        self.assertTrue((self.install / 'docker-compose.panel.local.yml').exists())
        self.assertFalse((self.install / 'deploy').exists())


@unittest.skipUnless(shutil.which('docker'), 'Docker Compose is needed to validate the deployment')
class DeploymentTests(unittest.TestCase):
    def config(self, password=HASH, legacy="0"):
        with tempfile.TemporaryDirectory() as tmp:
            shutil.copyfile(ROOT / 'docker-compose.panel.yml', Path(tmp) / 'compose.yml')
            result = subprocess.run(
                ['docker', 'compose', '-f', str(Path(tmp) / 'compose.yml'), 'config', '--format', 'json'],
                cwd=tmp,
                env={
                    'PATH': os.environ['PATH'],
                    'HOME': tmp,
                    'TS_AUTHKEY': 'tskey-auth-test',
                    'TS_EXTRA_ARGS': '--ssh=true',
                    'TS_AUTH_ONCE': 'false',
                    'STITCH_PANEL_TRUST_IDENTITY_HEADER': '1',
                    'STITCH_PANEL_IDENTITY_PROXY_ONLY': '1',
                    'PANEL_PASSWORD_HASH': password,
                    'PANEL_LEGACY_IDENTITY_AUTH': legacy,
                    'PANEL_USERS': 'alice@example.com' if legacy == '1' else '',
                },
                capture_output=True, text=True, timeout=30,
            )
        return result

    def test_explicit_legacy_mode_preserves_allowlisted_identity_for_ui_migration(self):
        result = self.config(password='', legacy='1')
        self.assertEqual(result.returncode, 0, result.stderr)
        panel = json.loads(result.stdout)['services']['panel']['environment']
        self.assertEqual(panel['STITCH_PANEL_TAILNET_USERS'], 'alice@example.com')
        self.assertEqual(panel['STITCH_PANEL_TRUST_IDENTITY_HEADER'], '1')

    def test_identity_bypass_and_ssh_cannot_be_enabled_through_env(self):
        result = self.config()
        self.assertEqual(result.returncode, 0, result.stderr)
        services = json.loads(result.stdout)['services']
        panel, sidecar = services['panel'], services['tailscale']
        # `compose config` escapes dollars for safe reloading of its output.
        self.assertEqual(panel['environment']['STITCH_PANEL_PASSWORD_HASH'].replace('$$', '$'), HASH)
        self.assertEqual(panel['environment']['STITCH_PANEL_TRUST_IDENTITY_HEADER'], '0')
        self.assertEqual(panel['environment']['STITCH_PANEL_IDENTITY_PROXY_ONLY'], '0')
        self.assertEqual(panel['environment']['STITCH_PANEL_TAILNET_USERS'], '')
        self.assertEqual(sidecar['environment']['TS_EXTRA_ARGS'], '--reset --ssh=false')
        # Persisted nodes skip login but still apply policy through the CLI shim.
        self.assertEqual(sidecar['environment']['TS_AUTH_ONCE'], 'true')
        self.assertEqual(sidecar['environment']['PATH'].split(':')[0], '/opt/stitch/bin')
        shim = next(v for v in sidecar['volumes'] if v['target'] == '/opt/stitch/bin/tailscale')
        self.assertTrue(shim['read_only'])
        self.assertTrue(shim['source'].endswith('/deploy/panel-tailscale.sh'))
        self.assertEqual(panel['network_mode'], 'service:tailscale')
        self.assertFalse(panel.get('ports'))
        self.assertFalse(sidecar.get('ports'))


class TailscalePolicyTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        shim_dir, cli_dir = self.root / 'shim', self.root / 'cli'
        shim_dir.mkdir()
        cli_dir.mkdir()
        self.shim = shim_dir / 'tailscale'
        shutil.copyfile(ROOT / 'deploy/panel-tailscale.sh', self.shim)
        self.shim.chmod(0o755)
        self.log = self.root / 'calls.jsonl'
        real_cli = cli_dir / 'tailscale'
        real_cli.write_text('#!' + sys.executable + '\n' + '''
import json, os, sys
with open(os.environ['TAILSCALE_TEST_LOG'], 'a') as f:
    f.write(json.dumps(sys.argv[1:]) + '\\n')
# Fail if startup tries to authenticate an enrolled node with its expired key.
if any(arg == '--authkey=expired' for arg in sys.argv):
    sys.exit(74)
sys.exit(int(os.environ.get('TAILSCALE_TEST_EXIT', '0')))
''')
        real_cli.chmod(0o755)
        self.env = {
            **os.environ,
            'PATH': str(shim_dir) + os.pathsep + str(cli_dir) + os.pathsep + os.environ['PATH'],
            'TAILSCALE_TEST_LOG': str(self.log),
            'TS_AUTHKEY': 'expired',
        }

    def run_cli(self, *args, **env):
        return subprocess.run(
            [str(self.shim), *args], env={**self.env, **env},
            capture_output=True, text=True, timeout=10,
        )

    def calls(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()]

    def test_enrolled_startup_sets_policy_without_reusing_expired_auth_key(self):
        # With TS_AUTH_ONCE=true, containerboot uses this invocation for an
        # enrolled node, then starts Serve only if it succeeds.
        args = ['--socket=/tmp/tailscaled.sock', 'set', '--accept-dns=false']
        for _ in range(2):
            result = self.run_cli(*args)
            self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(self.calls()), 2)
        for call in self.calls():
            self.assertEqual(call[:3], args)
            self.assertEqual(call[3:], [
                '--ssh=false', '--accept-routes=false', '--advertise-routes=',
                '--advertise-exit-node=false', '--advertise-connector=false',
                '--exit-node=', '--exit-node-allow-lan-access=false',
            ])
            self.assertFalse(any('authkey' in arg for arg in call))

    def test_first_enrollment_preserves_key_and_reset_arguments(self):
        args = ['--socket=/tmp/tailscaled.sock', 'up', '--authkey=new-key', '--reset', '--ssh=false']
        result = self.run_cli(*args)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls(), [args])

    def test_policy_failure_is_returned_to_containerboot(self):
        result = self.run_cli('--socket=/tmp/tailscaled.sock', 'set', TAILSCALE_TEST_EXIT='73')
        self.assertEqual(result.returncode, 73)
        self.assertEqual(len(self.calls()), 1)

    def test_set_without_socket_also_enforces_policy(self):
        result = self.run_cli('set', '--ssh=true', '--advertise-routes=10.0.0.0/8')
        self.assertEqual(result.returncode, 0, result.stderr)
        # The final flags take precedence over earlier arguments.
        self.assertIn('--ssh=false', self.calls()[0][3:])
        self.assertIn('--advertise-routes=', self.calls()[0][3:])

    def test_other_commands_preserve_arguments_and_exit_status(self):
        args = ['serve', 'status']
        result = self.run_cli(*args, TAILSCALE_TEST_EXIT='5')
        self.assertEqual(result.returncode, 5)
        self.assertEqual(self.calls(), [args])


if __name__ == '__main__':
    unittest.main()
