#!/usr/bin/env python3
"""Compare matched normal-startup reports using raw RSS/PSS and swap samples.

Requires schema-2 reports, at least three fresh process rounds, matching build
features/profile/tool versions, fixtures, display, declared geometry and timing.
Artifact manifests must have been created for each independently verified build.
This reports observed ranges, not statistical significance or whole-app savings.
"""
from __future__ import annotations
import argparse
import json
import math
import re
import statistics
from pathlib import Path

COUNTERS = ('rss_kib', 'pss_kib', 'swap_kib', 'swap_pss_kib')


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError(message)


def number(value) -> bool:
    return isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value)


def validate(report: dict, label: str) -> dict:
    require(isinstance(report, dict) and report.get('schema_version') == 2, f'{label}: requires schema-2 normal-startup report')
    identity = report.get('identity', {})
    config = report.get('configuration', {})
    require(isinstance(identity, dict) and isinstance(config, dict), f'{label}: invalid identity or configuration')
    artifact = identity.get('artifact')
    require(isinstance(artifact, dict), f'{label}: artifact provenance is missing')
    digest = identity.get('binary_sha256')
    require(isinstance(digest, str) and re.fullmatch(r'[0-9a-f]{64}', digest) is not None, f'{label}: invalid executable SHA-256')
    require(artifact.get('binary_sha256') == digest, f'{label}: manifest/executable SHA-256 mismatch')
    commit = artifact.get('source_commit')
    require(isinstance(commit, str) and re.fullmatch(r'[0-9a-f]{40}', commit) is not None, f'{label}: invalid artifact source commit')
    for key in ('build_profile', 'profile_settings', 'rust_version', 'slint_version'):
        require(isinstance(artifact.get(key), str) and bool(artifact[key]), f'{label}: missing {key}')
    features = artifact.get('build_features')
    require(isinstance(features, list) and all(isinstance(f, str) and f for f in features), f'{label}: missing build features')
    require(config.get('forced_snapshots') is False and config.get('provider_sync_disabled') is True, f'{label}: requires unsnapshotted, sync-disabled runs')
    for key in ('include_mappings', 'include_thread_stacks', 'profile_data_supplied', 'profile_cache_supplied'):
        require(isinstance(config.get(key), bool), f'{label}: missing {key}')
    fixtures = config.get('fixture_fingerprints')
    require(isinstance(fixtures, dict) and all(isinstance(fixtures.get(k), str) and (fixtures[k] == 'empty' or re.fullmatch(r'[0-9a-f]{64}', fixtures[k])) for k in ('data', 'cache')), f'{label}: missing fixture fingerprints')
    window = config.get('declared_window')
    require(isinstance(window, dict) and all(number(window.get(k)) and window[k] > 0 for k in ('width', 'height', 'scale')), f'{label}: record observed window width, height and scale')
    duration = config.get('sample_seconds')
    interval = config.get('sample_interval')
    require(number(duration) and duration > 0, f'{label}: invalid sample duration')
    require(interval is None or (number(interval) and 0 < interval <= duration), f'{label}: invalid sample interval')
    require(interval is None or duration / interval <= 10_000, f'{label}: sample interval produces more than 10000 samples')
    require(isinstance(identity.get('platform'), str) and bool(identity['platform']), f'{label}: missing platform')
    require(bool(identity.get('display') or identity.get('wayland_display')), f'{label}: missing display context')
    runs = report.get('runs')
    require(isinstance(runs, list) and len(runs) >= 3, f'{label}: requires at least three raw process rounds')
    require(config.get('rounds') == len(runs), f'{label}: round count does not match raw runs')
    require(all(isinstance(run, dict) for run in runs), f'{label}: invalid raw run')
    require([run.get('round') for run in runs] == list(range(1, len(runs) + 1)), f'{label}: rounds must be unique and ordered')
    for run in runs:
        renderer = run.get('renderer_selected', {})
        require(isinstance(renderer, dict) and renderer.get('active') == 'cpu' and renderer.get('wgpu_initialized') is False, f'{label}: runtime CPU/WGPU gate failed')
        samples = run.get('resource_samples')
        require(isinstance(samples, list) and bool(samples), f'{label}: raw procfs samples missing')
        expected_samples = 1 if interval is None else math.ceil(duration / interval)
        require(len(samples) == expected_samples, f'{label}: sample count disagrees with interval')
        previous = -1.0
        for sample in samples:
            require(isinstance(sample, dict), f'{label}: invalid procfs sample')
            elapsed = sample.get('elapsed_seconds')
            require(number(elapsed) and elapsed >= 0 and elapsed >= previous, f'{label}: nonmonotonic sample times')
            previous = elapsed
            resources = sample.get('resources', {})
            require(isinstance(resources, dict) and all(number(resources.get(key)) and resources[key] >= 0 for key in COUNTERS), f'{label}: missing or invalid RAM/swap counters')
            require(resources['swap_kib'] == 0 and resources['swap_pss_kib'] == 0, f'{label}: swapped samples cannot support RAM-saving claims')
        require(previous >= duration, f'{label}: final sample is earlier than declared duration')
        require(run.get('idle') == samples[-1]['resources'], f'{label}: final counters disagree with raw samples')
    return {'identity': identity, 'configuration': config, 'artifact': artifact, 'runs': runs}


def compare(baseline: dict, candidate: dict) -> dict:
    left, right = validate(baseline, 'baseline'), validate(candidate, 'candidate')
    for key in ('platform', 'display', 'wayland_display', 'slint_backend', 'winit_backend'):
        require(left['identity'].get(key) == right['identity'].get(key), f'incompatible display/platform context: {key}')
    for key in ('build_profile', 'profile_settings', 'rust_version', 'slint_version'):
        require(left['artifact'][key] == right['artifact'][key], f'incompatible artifact settings: {key}')
    require(sorted(set(left['artifact']['build_features'])) == sorted(set(right['artifact']['build_features'])), 'incompatible artifact settings: build_features')
    for key in ('sample_seconds', 'sample_interval', 'fixture_fingerprints', 'declared_window', 'forced_snapshots', 'provider_sync_disabled', 'include_mappings', 'include_thread_stacks', 'profile_data_supplied', 'profile_cache_supplied'):
        require(left['configuration'][key] == right['configuration'][key], f'incompatible measurement settings: {key}')
    rows = {}
    for phase in ('timed_end', 'sampled_peak'):
        rows[phase] = {}
        for counter in ('rss_kib', 'pss_kib'):
            def values(side):
                return [run['idle'][counter] if phase == 'timed_end' else max(sample['resources'][counter] for sample in run['resource_samples']) for run in side['runs']]
            old, new = values(left), values(right)
            old_median, new_median = statistics.median(old), statistics.median(new)
            rows[phase][counter.removesuffix('_kib')] = {
                'baseline_median_mib': old_median / 1024,
                'candidate_median_mib': new_median / 1024,
                'median_reduction_mib': (old_median - new_median) / 1024,
                'median_reduction_percent': (old_median - new_median) / old_median * 100 if old_median else None,
                'baseline_range_mib': [min(old) / 1024, max(old) / 1024],
                'candidate_range_mib': [min(new) / 1024, max(new) / 1024],
                'observed_ranges': 'lower' if max(new) < min(old) else 'higher' if min(new) > max(old) else 'overlap',
            }
    same_executable = left['identity']['binary_sha256'] == right['identity']['binary_sha256']
    outcome = 'same_executable_control' if same_executable else 'observed_lower_ranges' if all(row['observed_ranges'] == 'lower' for row in rows['timed_end'].values()) else 'mixed_or_overlapping_ranges'
    return {'schema_version': 1, 'scope': 'matched_timed_startup', 'outcome': outcome,
            'baseline_artifact': left['artifact'], 'candidate_artifact': right['artifact'],
            'rounds': {'baseline': len(left['runs']), 'candidate': len(right['runs'])},
            'measurements': rows,
            'limitations': ['Timed procfs samples are not frame or settled-state events.', 'Sampled peaks are lower bounds.', 'Geometry and build provenance are declared; verify them independently.', 'Observed ranges do not establish statistical significance or memory savings in other app scenarios.']}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('baseline', type=Path)
    parser.add_argument('candidate', type=Path)
    parser.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    try:
        report = compare(json.loads(args.baseline.read_text()), json.loads(args.candidate.read_text()))
    except (ValueError, OSError) as error:
        parser.error(str(error))
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + '\n')
    for name, row in report['measurements']['timed_end'].items():
        print(f"{name.upper()}: {row['baseline_median_mib']:.2f} -> {row['candidate_median_mib']:.2f} MiB; ranges={row['observed_ranges']}")
    print(f"outcome={report['outcome']}; report={args.output}")

if __name__ == '__main__':
    main()
