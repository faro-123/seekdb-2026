"""Capture a trusted direct command, fingerprints and unmodified raw streams."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time

p = argparse.ArgumentParser()
p.add_argument('--out', required=True)
p.add_argument('--input', action='append', default=[])
p.add_argument('command', nargs=argparse.REMAINDER)
a = p.parse_args()
out = Path(a.out)
out.mkdir(parents=True, exist_ok=False)
cmd = a.command[1:] if a.command[:1] == ['--'] else a.command
def fingerprints():
    return {x: hashlib.sha256(Path(x).read_bytes()).hexdigest() for x in a.input}
record = dict(command=cmd, cwd=os.getcwd(), inputs=fingerprints(), started_utc=time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()))
(out / 'command.json').write_text(json.dumps(record, indent=2), encoding='utf-8')
start = time.monotonic()
with (out / 'stdout.log').open('wb') as stdout, (out / 'stderr.log').open('wb') as stderr:
    result = subprocess.run(cmd, stdout=stdout, stderr=stderr, check=False)
record.update(exit_code=result.returncode, elapsed_seconds=time.monotonic()-start, inputs_after=fingerprints())
(out / 'result.json').write_text(json.dumps(record, indent=2), encoding='utf-8')
print(json.dumps(record, ensure_ascii=False))
raise SystemExit(result.returncode)
