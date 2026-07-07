#!/usr/bin/env python3
"""Run Typhoon sample files one by one."""

from __future__ import annotations

import os
import subprocess
import sys
import tempfile
from pathlib import Path


def sample_root() -> Path:
    return (Path.cwd() / "sample").resolve()


def discover_samples(root: Path) -> list[Path]:
    server_sample = root / "network_integration_test" / "main.ty"
    samples = sorted(p for p in root.rglob("main.ty") if p != server_sample)
    return samples


def should_run(sample: Path) -> bool:
    if sample.parent.name == "network_integration_test":
        return False
    return True


def filter_samples(samples: list[Path]) -> list[Path]:
    return [sample for sample in samples if should_run(sample)]


def sample_output_path(root: Path, sample: Path) -> Path:
    rel = sample.resolve().relative_to(root)
    slug = "__".join(rel.with_suffix("").parts)
    suffix = ".exe" if os.name == "nt" else ""
    base = Path(tempfile.mkdtemp(prefix="ty-sample-"))
    return base / f"{slug}{suffix}"


def run_sample(ty_bin: str, root: Path, sample: Path) -> int:
    out = sample_output_path(root, sample)
    print(f"==> running {sample}")
    proc = subprocess.run([ty_bin, "build", str(sample), str(out)], cwd=str(root))
    if proc.returncode != 0:
        return proc.returncode
    proc = subprocess.run([str(out)])
    return proc.returncode


def ty_binary(root: Path) -> str:
    env = os.environ.get("TY_BIN")
    if env:
        return env
    return str((root.parent / "target" / "debug" / ("tyc.exe" if os.name == "nt" else "tyc")))


def main() -> int:
    root = sample_root()
    ty_bin = ty_binary(root)
    args = [Path(arg) for arg in sys.argv[1:]]
    samples = filter_samples(args or discover_samples(root))

    if not samples:
        print("no sample files found", file=sys.stderr)
        return 1

    for sample in samples:
        path = sample if sample.is_absolute() else (Path.cwd() / sample)
        code = run_sample(ty_bin, root, path)
        if code != 0:
            return code

    return 0


if __name__ == "__main__":
    raise SystemExit(main())

