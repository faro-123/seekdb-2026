"""Capture a trusted local command in a new isolated work directory."""

import argparse
from datetime import datetime, timezone
import hashlib
import json
import math
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time


def _hash_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", required=True, help="new experiment directory")
    parser.add_argument("--input", action="append", default=[], help="input file to fingerprint; repeatable")
    parser.add_argument("--revision", default="", help="source revision or label")
    parser.add_argument("--timeout", type=float, default=600, help="direct process timeout in seconds")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("provide a trusted command after --")
    if not math.isfinite(args.timeout) or args.timeout <= 0:
        parser.error("timeout must be a positive finite number")

    try:
        if os.name == "nt":
            resolved = shutil.which(command[0])
            if (Path(command[0]).suffix.lower() in (".bat", ".cmd")
                    or (resolved and Path(resolved).suffix.lower() in (".bat", ".cmd"))):
                raise ValueError("Windows batch files require a shell and are not supported")
        inputs = []
        for filename in args.input:
            path = Path(filename).resolve(strict=True)
            if not path.is_file():
                raise ValueError(f"input is not a file: {path}")
            inputs.append({"path": str(path), "sha256_before": _hash_file(path)})
        output = Path(args.out).absolute()
        output.mkdir(exist_ok=False)
        work = output / "work"
        data = output / "data"
        work.mkdir()
        data.mkdir()
    except (OSError, ValueError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2

    environment = dict(os.environ, MINIVECTORDB_TEST_DATA_DIR=str(data))
    started_at = datetime.now(timezone.utc).isoformat()
    start = time.perf_counter()
    exit_code = None
    failure = ""
    status = "exited"
    with open(output / "stdout.txt", "xb") as stdout, open(output / "stderr.txt", "xb") as stderr:
        try:
            proc = subprocess.run(command, cwd=work, env=environment, stdout=stdout, stderr=stderr,
                                  timeout=args.timeout, check=False)
            exit_code = proc.returncode
        except subprocess.TimeoutExpired:
            status = "timeout"
        except OSError as exc:
            status = "launch_error"
            failure = str(exc)
    elapsed = time.perf_counter() - start
    input_changed = False
    for record in inputs:
        try:
            record["sha256_after"] = _hash_file(record["path"])
        except OSError:
            record["sha256_after"] = None
        input_changed |= record["sha256_before"] != record["sha256_after"]
    if input_changed and status == "exited":
        status = "input_changed"
    manifest = {
        "scope": "local-command-capture-not-official-benchmark",
        "process_scope": "direct-process-only",
        "status": status, "exit_code": exit_code, "error": failure,
        "started_at_utc": started_at, "wall_seconds": elapsed,
        "revision": args.revision, "command": command, "inputs": inputs,
    }
    with open(output / "manifest.json", "x", encoding="utf-8", newline="\n") as stream:
        json.dump(manifest, stream, ensure_ascii=False, allow_nan=False, indent=2)
        stream.write("\n")
    print(f"local capture: {status}; output: {output}")
    if status == "timeout":
        return 124
    if status == "launch_error":
        return 127
    if status == "input_changed":
        return 2
    return exit_code if 0 <= exit_code <= 255 else 1


if __name__ == "__main__":
    raise SystemExit(main())
