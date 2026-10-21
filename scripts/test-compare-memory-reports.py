#!/usr/bin/env python3
"""Check comparison gates with deterministic synthetic reports, not app claims."""
import copy
import importlib.util
import json
import subprocess
import tempfile
import unittest
from pathlib import Path
spec = importlib.util.spec_from_file_location('memory_compare', Path(__file__).with_name('compare-memory-reports.py'))
comparison = importlib.util.module_from_spec(spec)
spec.loader.exec_module(comparison)


def report(digest, rss, pss):
    artifact = {'binary_sha256': digest * 64, 'source_commit': digest * 40, 'build_features': ['gpu-renderer', 'remote-content'], 'build_profile': 'release', 'profile_settings': 'opt=3,lto=thin,units=1', 'rust_version': '1.98.1', 'slint_version': '1.18.1'}
    runs = []
    for index in range(3):
        samples = []
        for tick in range(1, 12):
            resources = {'rss_kib': rss + index * 100 + (5000 if tick == 3 else 0), 'pss_kib': pss + index * 100 + (4000 if tick == 3 else 0), 'swap_kib': 0, 'swap_pss_kib': 0}
            samples.append({'elapsed_seconds': tick * 0.5, 'resources': resources})
        runs.append({'round': index + 1, 'idle': samples[-1]['resources'], 'resource_samples': samples, 'renderer_selected': {'active': 'cpu', 'wgpu_initialized': False}})
    return {'schema_version': 2, 'identity': {'binary_sha256': digest * 64, 'artifact': artifact, 'platform': 'Linux fixture', 'display': ':98', 'wayland_display': None, 'slint_backend': None, 'winit_backend': None}, 'configuration': {'rounds': 3, 'sample_seconds': 5.5, 'sample_interval': 0.5, 'fixture_fingerprints': {'data': 'empty', 'cache': 'empty'}, 'declared_window': {'width': 1320, 'height': 800, 'scale': 1.0}, 'forced_snapshots': False, 'provider_sync_disabled': True, 'include_mappings': False, 'include_thread_stacks': False, 'profile_data_supplied': False, 'profile_cache_supplied': False}, 'runs': runs}


class ComparisonTests(unittest.TestCase):
    def setUp(self):
        self.before = report('a', 100000, 90000)
        self.after = report('b', 80000, 70000)

    def test_raw_medians_and_intermediate_peaks(self):
        self.before['summary'] = {'median_idle': {'rss_kib': 1}}
        result = comparison.compare(self.before, self.after)
        self.assertEqual(result['outcome'], 'observed_lower_ranges')
        self.assertEqual(result['measurements']['timed_end']['rss']['baseline_median_mib'], 100100 / 1024)
        self.assertEqual(result['measurements']['sampled_peak']['rss']['baseline_median_mib'], 105100 / 1024)
        self.assertEqual(result['measurements']['timed_end']['pss']['median_reduction_mib'], 20000 / 1024)

    def test_same_executable_is_a_control(self):
        self.after['identity']['binary_sha256'] = self.before['identity']['binary_sha256']
        self.after['identity']['artifact']['binary_sha256'] = self.before['identity']['binary_sha256']
        self.assertEqual(comparison.compare(self.before, self.after)['outcome'], 'same_executable_control')

    def test_overlapping_ranges_are_inconclusive(self):
        self.after = report('b', 100050, 90050)
        result = comparison.compare(self.before, self.after)
        self.assertEqual(result['outcome'], 'mixed_or_overlapping_ranges')
        self.assertEqual(result['measurements']['timed_end']['pss']['observed_ranges'], 'overlap')

    def test_incompatible_build_and_measurement_settings(self):
        mutations = [('artifact', 'build_features', ['remote-content']), ('artifact', 'profile_settings', 'opt=s'), ('artifact', 'rust_version', 'different'), ('configuration', 'declared_window', {'width': 900, 'height': 700, 'scale': 1}), ('configuration', 'fixture_fingerprints', {'data': 'c'*64, 'cache': 'empty'}), ('identity', 'display', ':99'), ('identity', 'winit_backend', 'x11')]
        for group, key, value in mutations:
            with self.subTest(group=group, key=key):
                candidate = copy.deepcopy(self.after)
                target = candidate['identity']['artifact'] if group == 'artifact' else candidate[group]
                target[key] = value
                with self.assertRaisesRegex(ValueError, 'incompatible'):
                    comparison.compare(self.before, candidate)

    def test_intermediate_swap_is_rejected_even_when_final_swap_is_zero(self):
        self.after['runs'][0]['resource_samples'][2]['resources']['swap_pss_kib'] = 1
        with self.assertRaisesRegex(ValueError, 'swapped samples'):
            comparison.compare(self.before, self.after)

    def test_missing_provenance_gpu_and_insufficient_rounds(self):
        for mutation in ['artifact', 'renderer', 'rounds', 'nan', 'samples', 'hash']:
            with self.subTest(mutation=mutation):
                candidate = copy.deepcopy(self.after)
                if mutation == 'artifact': candidate['identity']['artifact'] = None
                if mutation == 'renderer': candidate['runs'][0]['renderer_selected']['wgpu_initialized'] = True
                if mutation == 'rounds': candidate['runs'] = candidate['runs'][:2]
                if mutation == 'nan': candidate['runs'][0]['resource_samples'][0]['resources']['pss_kib'] = float('nan')
                if mutation == 'samples': candidate['runs'][0]['resource_samples'].pop(0)
                if mutation == 'hash': candidate['identity']['artifact']['binary_sha256'] = 'c' * 64
                with self.assertRaises(ValueError): comparison.compare(self.before, candidate)

    def test_cli_writes_reviewable_report(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            old, new, output = root / 'old.json', root / 'new.json', root / 'comparison.json'
            old.write_text(json.dumps(self.before))
            new.write_text(json.dumps(self.after))
            result = subprocess.run(['python3', comparison.__file__, str(old), str(new), '--output', str(output)], capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads(output.read_text())['outcome'], 'observed_lower_ranges')
            self.assertIn('RSS:', result.stdout)
            self.after['configuration']['declared_window']['scale'] = 2
            new.write_text(json.dumps(self.after))
            output.unlink()
            rejected = subprocess.run(['python3', comparison.__file__, str(old), str(new), '--output', str(output)], capture_output=True, text=True)
            self.assertNotEqual(rejected.returncode, 0)
            self.assertFalse(output.exists())

if __name__ == '__main__': unittest.main()
