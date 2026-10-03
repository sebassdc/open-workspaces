#!/usr/bin/env python3
"""Repeat isolated correctness runs and summarize observed warm-cache timings."""
import argparse
from datetime import datetime, timezone
import json
import math
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[2]

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--samples', type=int, default=10)
args = parser.parse_args()
if not 1 <= args.samples <= 30:
    parser.error('samples must be between 1 and 30')
destination = Path(tempfile.mkdtemp(prefix='measure-', dir=ROOT / 'data/runtime-spike/results'))
reports = []
failures = []
for index in range(args.samples):
    result = subprocess.run([sys.executable, str(ROOT / 'experiments/runtime-spike/prototype-e2e.py')],
                            capture_output=True, text=True, timeout=600)
    (destination / f'sample-{index + 1}.log').write_text(result.stdout + result.stderr)
    first = result.stdout.splitlines()[0] if result.stdout else ''
    if first.startswith('Local test artifacts: '):
        report = json.loads((Path(first.removeprefix('Local test artifacts: ')) / 'e2e-result.json').read_text())
        if result.returncode == 0 and report['passed']:
            reports.append(report)
        else:
            failures.append({'sample': index + 1, 'error': report.get('error', 'test failure')})
    else:
        failures.append({'sample': index + 1, 'error': 'no test report'})
    print(f'Sample {index + 1}/{args.samples}: {"passed" if result.returncode == 0 else "FAILED"}', flush=True)

metrics = {}
if reports:
    for name in reports[0]['timings_ms']:
        values = sorted(report['timings_ms'][name] for report in reports)
        def percentile(fraction):
            return round(values[max(0, math.ceil(len(values) * fraction) - 1)], 2)
        metrics[name] = {'samples': len(values), 'p50_ms': percentile(0.5), 'p95_ms': percentile(0.95)}
summary = {'timestamp_utc': datetime.now(timezone.utc).isoformat(), 'attempts': args.samples,
           'successes': len(reports), 'failures': failures, 'percentile_method': 'nearest rank',
           'cache_condition': 'warm host caches; fresh VMs and worker for every sample',
           'workload': 'Alpine 3.24.2, Linux 6.1.186, 1 vCPU/256 MiB; tiny Rust HTTP counter',
           'metrics': metrics}
(destination / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
print(json.dumps(summary, indent=2))
print(f'Local results: {destination}')
raise SystemExit(0 if not failures else 1)
