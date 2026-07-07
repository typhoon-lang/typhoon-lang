#!/usr/bin/env python3
"""Run the network integration sample and keep it alive."""

from __future__ import annotations

import os
import subprocess
import tempfile
from pathlib import Path

DEFAULT_SAMPLE = (
    Path.cwd() / "sample" / "network_integration_test" / "main.ty"
).resolve()


def main() -> int:
    ty_bin = os.environ.get("TY_BIN", "tyc")
    sample = Path(os.environ.get("TY_SAMPLE", str(DEFAULT_SAMPLE)))
    if not sample.is_absolute():
        sample = Path.cwd() / sample

    suffix = ".exe" if os.name == "nt" else ""
    out = Path(tempfile.mkdtemp(prefix="ty-server-")) / f"server{suffix}"

    compile_proc = subprocess.run(
        [ty_bin, "build", str(sample), str(out)], cwd=str(Path.cwd())
    )
    if compile_proc.returncode != 0:
        return compile_proc.returncode

    run_proc = subprocess.run([str(out)])
    return run_proc.returncode


if __name__ == "__main__":
    raise SystemExit(main())
