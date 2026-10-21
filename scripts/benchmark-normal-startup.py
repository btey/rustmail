#!/usr/bin/env python3
"""Measure an unsnapshotted Linux launch at a fixed time.

This intentionally does not enable RUSTMAIL_STARTUP_METRICS, which forces frame
snapshots in the regular startup benchmark. The sample is timed from process
launch, not from a rendered-frame event.
"""

from __future__ import annotations

import argparse
import importlib.util
import hashlib
import math
import json
import os
import platform
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path


spec = importlib.util.spec_from_file_location(
    "startup_benchmark", Path(__file__).with_name("benchmark-startup.py")
)
startup = importlib.util.module_from_spec(spec)
spec.loader.exec_module(startup)


def profile_fingerprint(source: Path | None) -> str:
    """Hash fixture bytes and relative names without recording private paths."""
    if source is None:
        return "empty"
    digest = hashlib.sha256()
    for path in sorted(source.rglob("*")):
        if path.is_symlink():
            raise ValueError("fixture fingerprints require a tree without symlinks")
        if path.is_file():
            relative = path.relative_to(source).as_posix().encode()
            digest.update(len(relative).to_bytes(8, "big"))
            digest.update(relative)
            digest.update(bytes.fromhex(startup.file_sha256(path)))
    return digest.hexdigest()


def sample_resources(process, duration: float, interval: float | None) -> list[dict]:
    """Timed procfs samples; observed peaks are lower bounds, not frame events."""
    started = time.monotonic()
    samples = []
    index = 1
    while True:
        target = duration if interval is None else min(index * interval, duration)
        time.sleep(max(0.0, started + target - time.monotonic()))
        if process.poll() is not None:
            raise RuntimeError(f"application exited before sample: {process.returncode}")
        resources = startup.read_proc_resources(process.pid)
        samples.append({"elapsed_seconds": time.monotonic() - started, "resources": resources})
        if target >= duration:
            return samples
        index += 1


def load_artifact_manifest(path: Path | None, binary_hash: str) -> dict | None:
    if path is None:
        return None
    data = json.loads(path.read_text())
    if not isinstance(data, dict):
        raise ValueError("artifact manifest must be a JSON object")
    if data.get("binary_sha256") != binary_hash:
        raise ValueError("artifact manifest does not match executable SHA-256")
    commit = data.get("source_commit")
    if not isinstance(commit, str) or len(commit) != 40 or any(c not in "0123456789abcdef" for c in commit):
        raise ValueError("artifact manifest requires a full source_commit hash")
    features = data.get("build_features")
    if not isinstance(features, list) or any(not isinstance(value, str) or not value for value in features):
        raise ValueError("artifact manifest requires a build_features list")
    provenance = {}
    for key in ("build_profile", "rust_version", "slint_version", "profile_settings"):
        value = data.get(key)
        if not isinstance(value, str) or not value:
            raise ValueError(f"artifact manifest requires {key}")
        provenance[key] = value
    return {"binary_sha256": binary_hash, "source_commit": commit, "build_features": sorted(set(features)), **provenance}


def run_once(
    binary: Path,
    sample_seconds: float,
    include_mappings: bool,
    include_thread_stacks: bool,
    profile_data: Path | None,
    profile_cache: Path | None,
    sample_interval: float | None = None,
) -> dict:
    with tempfile.TemporaryDirectory(prefix="rustmail-normal-startup-") as temporary:
        root = Path(temporary)
        startup.copy_profile(profile_data, root / "data" / "rustmail")
        startup.copy_profile(profile_cache, root / "cache" / "rustmail")
        environment = os.environ.copy()
        for name in (
            "RUSTMAIL_STARTUP_METRICS",
            "RUSTMAIL_BENCHMARK_EXIT_AFTER_MS",
            "RUSTMAIL_BENCHMARK_SCREENSHOT_DIR",
            "RUSTMAIL_BENCHMARK_TRAY_INTERVAL_MS",
        ):
            environment.pop(name, None)
        environment.update(
            XDG_DATA_HOME=str(root / "data"),
            XDG_CACHE_HOME=str(root / "cache"),
            RUSTMAIL_RENDERER="cpu",
            RUSTMAIL_BENCHMARK_DISABLE_SYNC="1",
        )
        with tempfile.TemporaryFile(mode="w+t") as log:
            process = subprocess.Popen(
                [str(binary)],
                env=environment,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=log,
                text=True,
            )
            try:
                started = time.monotonic()
                samples = sample_resources(process, sample_seconds, sample_interval)
                resources = samples[-1]["resources"]
                mappings = (
                    startup.read_proc_mappings(
                        process.pid, startup.runtime_executable(process.pid)
                    )
                    if include_mappings
                    else None
                )
                thread_stacks = (
                    read_thread_stacks(process.pid)
                    if include_thread_stacks
                    else None
                )
                sampled_after = time.monotonic() - started
            finally:
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=5)
            log.seek(0)
            renderer = None
            for line in log:
                if line.startswith(startup.RENDERER_PREFIX):
                    payload = json.loads(line[len(startup.RENDERER_PREFIX) :])
                    if payload.get("event") == "selected":
                        renderer = payload
            if not renderer or renderer.get("active") != "cpu" or renderer.get("wgpu_initialized") is not False:
                raise RuntimeError(f"expected CPU renderer without WGPU, got {renderer}")

        result = {
            "idle": resources,
            "resource_samples": samples,
            "sampled_peak": {
                name: max(sample["resources"][name] for sample in samples)
                for name in ("rss_kib", "pss_kib", "swap_kib", "swap_pss_kib")
            },
            "sampled_after_seconds": sampled_after,
            "renderer_selected": renderer,
        }
        if mappings is not None:
            result["mapping_report"] = mappings
        if thread_stacks is not None:
            result["thread_stacks"] = thread_stacks
        return result


def read_thread_stacks(pid: int) -> dict:
    """Map sampled thread stack pointers to VMAs without recording addresses."""
    mappings: list[dict] = []
    current: dict | None = None
    for line in Path(f"/proc/{pid}/smaps").read_text(encoding="utf-8").splitlines():
        if startup.SMAPS_HEADER.match(line):
            fields = line.split(maxsplit=5)
            start, end = (int(value, 16) for value in fields[0].split("-"))
            current = {
                "start": start,
                "end": end,
                "kind": "main_stack" if len(fields) == 6 and fields[5] == "[stack]" else "other",
                "size_kib": (end - start) // 1024,
                "rss_kib": 0,
                "pss_kib": 0,
            }
            mappings.append(current)
        elif current is not None:
            for key in ("Rss", "Pss"):
                if line.startswith(f"{key}:"):
                    current[f"{key.lower()}_kib"] = int(line.split()[1])
                    break

    threads = []
    for task in sorted(Path(f"/proc/{pid}/task").iterdir(), key=lambda path: int(path.name)):
        try:
            name = (task / "comm").read_text(encoding="utf-8").strip()
        except OSError:
            continue  # A short-lived task exited during the sample.
        pointer = None
        for _ in range(10):
            try:
                fields = (task / "syscall").read_text(encoding="utf-8").split()
            except OSError:
                break
            if len(fields) >= 9:
                pointer = int(fields[-2], 16)
                break
            time.sleep(0.005)
        matching = next(
            (
                index
                for index, entry in enumerate(mappings)
                if pointer is not None and entry["start"] <= pointer < entry["end"]
            ),
            None,
        )
        threads.append({"name": name, "mapping_index": matching})

    used = {
        thread["mapping_index"]
        for thread in threads
        if thread["mapping_index"] is not None
    }
    unique = [mappings[index] for index in sorted(used)]
    return {
        "thread_count": len(threads),
        "mapped_thread_count": sum(thread["mapping_index"] is not None for thread in threads),
        "unique_stack_mappings": len(unique),
        "reserved_kib": sum(entry["size_kib"] for entry in unique),
        "rss_kib": sum(entry["rss_kib"] for entry in unique),
        "pss_kib": sum(entry["pss_kib"] for entry in unique),
        "threads": [
            {
                "name": thread["name"],
                "mapping_kind": (
                    mappings[thread["mapping_index"]]["kind"]
                    if thread["mapping_index"] is not None else "unmapped"
                ),
                "size_kib": (
                    mappings[thread["mapping_index"]]["size_kib"]
                    if thread["mapping_index"] is not None else None
                ),
                "rss_kib": (
                    mappings[thread["mapping_index"]]["rss_kib"]
                    if thread["mapping_index"] is not None else None
                ),
            }
            for thread in threads
        ],
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--sample-seconds", type=float, default=5.5)
    parser.add_argument("--sample-interval", type=float, help="Optional procfs sampling interval; sampled peaks are lower bounds.")
    parser.add_argument("--artifact-manifest", type=Path, help="Build record: SHA-256, source commit, features, profile/settings, Rust and Slint versions.")
    parser.add_argument("--window-width", type=int, help="Observed logical window width, recorded as declared context.")
    parser.add_argument("--window-height", type=int, help="Observed logical window height, recorded as declared context.")
    parser.add_argument("--scale", type=float, help="Observed display scale, recorded as declared context.")
    parser.add_argument("--profile-data-dir", type=Path)
    parser.add_argument("--profile-cache-dir", type=Path)
    parser.add_argument("--include-mappings", action="store_true")
    parser.add_argument("--include-thread-stacks", action="store_true")
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    if sys.platform != "linux":
        parser.error("this benchmark requires Linux /proc")
    if args.rounds < 1 or not math.isfinite(args.sample_seconds) or args.sample_seconds <= 0:
        parser.error("rounds and sample seconds must be positive and finite")
    if args.sample_interval is not None and (not math.isfinite(args.sample_interval) or args.sample_interval <= 0 or args.sample_interval > args.sample_seconds or args.sample_seconds / args.sample_interval > 10_000):
        parser.error("sample interval must be positive, finite, no longer than duration, and produce at most 10000 samples")
    geometry = (args.window_width, args.window_height, args.scale)
    if any(value is not None for value in geometry) and (any(value is None for value in geometry) or args.window_width <= 0 or args.window_height <= 0 or not math.isfinite(args.scale) or args.scale <= 0):
        parser.error("supply positive width, height and finite scale together")
    if not os.environ.get("DISPLAY") and not os.environ.get("WAYLAND_DISPLAY"):
        parser.error("a display is required")
    binary = args.binary.resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        parser.error(f"binary is not executable: {binary}")
    for name, path in (
        ("profile data", args.profile_data_dir),
        ("profile cache", args.profile_cache_dir),
    ):
        if path is not None and not path.is_dir():
            parser.error(f"{name} directory does not exist: {path}")

    binary_hash = startup.file_sha256(binary)
    try:
        artifact = load_artifact_manifest(args.artifact_manifest, binary_hash)
        fixture = {"data": profile_fingerprint(args.profile_data_dir), "cache": profile_fingerprint(args.profile_cache_dir)}
    except (ValueError, OSError) as error:
        parser.error(str(error))
    runs = []
    for index in range(args.rounds):
        run = run_once(
            binary,
            args.sample_seconds,
            args.include_mappings,
            args.include_thread_stacks,
            args.profile_data_dir,
            args.profile_cache_dir,
            args.sample_interval,
        )
        run["round"] = index + 1
        runs.append(run)
        print(
            f"round={index + 1} rss_kib={run['idle']['rss_kib']} "
            f"pss_kib={run['idle']['pss_kib']}",
            flush=True,
        )
    if startup.file_sha256(binary) != binary_hash:
        raise RuntimeError("executable changed during benchmark; results rejected")
    if fixture != {"data": profile_fingerprint(args.profile_data_dir), "cache": profile_fingerprint(args.profile_cache_dir)}:
        raise RuntimeError("source fixture changed during benchmark; results rejected")
    names = sorted({name for run in runs for name in run["idle"]})
    report = {
        "schema_version": 2,
        "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "identity": {
            "binary": str(binary),
            "binary_sha256": binary_hash,
            "artifact": artifact,
            "git_commit_scope": "workspace_at_measurement",
            "git_commit": startup.command_output("git", "rev-parse", "HEAD"),
            "platform": platform.platform(),
            "display": os.environ.get("DISPLAY"),
            "wayland_display": os.environ.get("WAYLAND_DISPLAY"),
            "slint_backend": os.environ.get("SLINT_BACKEND"),
            "winit_backend": os.environ.get("WINIT_UNIX_BACKEND"),
        },
        "configuration": {
            "rounds": args.rounds,
            "sample_seconds": args.sample_seconds,
            "sample_interval": args.sample_interval,
            "fixture_fingerprints": fixture,
            "declared_window": {"width": args.window_width, "height": args.window_height, "scale": args.scale},
            "include_mappings": args.include_mappings,
            "include_thread_stacks": args.include_thread_stacks,
            "forced_snapshots": False,
            "provider_sync_disabled": True,
            "profile_data_supplied": args.profile_data_dir is not None,
            "profile_cache_supplied": args.profile_cache_dir is not None,
        },
        "summary": {
            "sampled_peaks_are_lower_bounds": True,
            "median_sampled_peak": {
                name: statistics.median(run["sampled_peak"][name] for run in runs)
                for name in ("rss_kib", "pss_kib", "swap_kib", "swap_pss_kib")
            },
            "median_idle": {
                name: statistics.median(run["idle"][name] for run in runs)
                for name in names
            }
        },
        "runs": runs,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    print(f"report={args.output.resolve()}")


if __name__ == "__main__":
    main()
