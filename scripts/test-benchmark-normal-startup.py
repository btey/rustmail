#!/usr/bin/env python3
"""Behavior checks for timed procfs sampling and benchmark provenance."""
import importlib.util
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch
spec = importlib.util.spec_from_file_location("normal_startup", Path(__file__).with_name("benchmark-normal-startup.py"))
benchmark = importlib.util.module_from_spec(spec)
spec.loader.exec_module(benchmark)

class TimedSampleTests(unittest.TestCase):
    def test_live_fixture_samples_and_cpu_gate(self):
        if not Path('/proc/self/smaps_rollup').exists():
            self.skipTest('requires Linux procfs')
        with tempfile.TemporaryDirectory() as directory:
            stub = Path(directory) / 'fixture'
            stub.write_text('''#!/usr/bin/env python3
import sys, time
sys.stderr.write('RUSTMAIL_RENDERER {"event":"selected","active":"cpu","wgpu_initialized":false}\\n')
sys.stderr.flush()
payload = bytearray(1024 * 1024)
time.sleep(10)
''')
            stub.chmod(0o755)
            run = benchmark.run_once(stub, 0.3, False, False, None, None, 0.1)
            self.assertEqual(len(run['resource_samples']), 3)
            self.assertGreater(run['idle']['rss_kib'], 0)
            self.assertGreater(run['idle']['pss_kib'], 0)
            self.assertEqual(run['idle'], run['resource_samples'][-1]['resources'])
            self.assertEqual(run['sampled_peak']['rss_kib'], max(s['resources']['rss_kib'] for s in run['resource_samples']))
            self.assertFalse(run['renderer_selected']['wgpu_initialized'])

    def test_early_exit_rejects_samples(self):
        class Exited:
            pid = 0
            def poll(self): return 7
            returncode = 7
        with patch.object(benchmark.time, 'sleep'):
            with self.assertRaisesRegex(RuntimeError, 'exited before sample: 7'):
                benchmark.sample_resources(Exited(), 1, 0.5)

    def test_fixture_hash_tracks_content_and_names_without_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / 'fixture.db'
            path.write_bytes(b'one')
            first = benchmark.profile_fingerprint(root)
            path.write_bytes(b'two')
            self.assertNotEqual(first, benchmark.profile_fingerprint(root))
            second = benchmark.profile_fingerprint(root)
            path.rename(root / 'renamed.db')
            self.assertNotEqual(second, benchmark.profile_fingerprint(root))
            self.assertNotIn(directory, benchmark.profile_fingerprint(root))
            (root / 'link').symlink_to(root / 'renamed.db')
            with self.assertRaisesRegex(ValueError, 'symlinks'):
                benchmark.profile_fingerprint(root)

    def test_manifest_checks_executable_and_keeps_only_provenance(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'manifest.json'
            data = {'binary_sha256': 'a' * 64, 'source_commit': 'b' * 40, 'build_features': ['remote-content', 'gpu-renderer'], 'private_path': directory, 'build_profile': 'release', 'profile_settings': 'opt=3,lto=thin,units=1', 'rust_version': '1.98.1', 'slint_version': '1.18.1'}
            path.write_text(json.dumps(data))
            result = benchmark.load_artifact_manifest(path, 'a' * 64)
            self.assertNotIn('private_path', result)
            with self.assertRaisesRegex(ValueError, 'SHA-256'):
                benchmark.load_artifact_manifest(path, 'c' * 64)
            data['source_commit'] = 'HEAD'
            path.write_text(json.dumps(data))
            with self.assertRaisesRegex(ValueError, 'source_commit'):
                benchmark.load_artifact_manifest(path, 'a' * 64)

    def test_invalid_sample_intervals_fail_before_launch(self):
        for value in ['0', '-1', 'nan', 'inf', '100', '0.000001']:
            result = subprocess.run(['python3', str(Path(benchmark.__file__)), '--binary', '/bin/true', '--output', '/dev/null', '--sample-interval', value], capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('sample interval', result.stderr)

if __name__ == '__main__': unittest.main()
