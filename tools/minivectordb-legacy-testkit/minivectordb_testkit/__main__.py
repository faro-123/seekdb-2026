"""Offline fixture generation and result checking; not the contest wire protocol."""

import argparse
from decimal import Decimal
import hashlib
import json
import sys
from pathlib import Path

from .dataset import generate_dataset, truth, validate_dataset
from .oracle import evaluate


def _object_without_duplicates(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def _invalid_constant(value):
    raise ValueError(f"non-finite JSON number: {value}")


def _read_json(path):
    data = Path(path).read_bytes()
    value = json.loads(data.decode("utf-8"), object_pairs_hook=_object_without_duplicates,
                       parse_constant=_invalid_constant, parse_float=Decimal)
    return value, hashlib.sha256(data).hexdigest()


def _write_json(path, value):
    # Exclusive creation ensures the tool never replaces a previous experiment.
    with open(path, "x", encoding="utf-8", newline="\n") as output:
        json.dump(value, output, ensure_ascii=False, allow_nan=False, indent=2)
        output.write("\n")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    generate = commands.add_parser("generate", help="create a small seeded dataset")
    generate.add_argument("--out", required=True)
    generate.add_argument("--rows", type=int, default=100)
    generate.add_argument("--dim", type=int, default=4)
    generate.add_argument("--queries", type=int, default=10)
    generate.add_argument("--k", type=int, default=10)
    generate.add_argument("--seed", type=int, default=20261110)
    reference = commands.add_parser("truth", help="compute local exact answers")
    reference.add_argument("--dataset", required=True)
    reference.add_argument("--out", required=True)
    check = commands.add_parser("check", help="validate candidate answers")
    check.add_argument("--dataset", required=True)
    check.add_argument("--results", required=True)
    check.add_argument("--mode", choices=("exact", "ann"), default="ann")
    check.add_argument("--out", required=True)
    args = parser.parse_args(argv)

    try:
        if args.command == "generate":
            value = generate_dataset(args.rows, args.dim, args.queries, args.k, args.seed)
            _write_json(args.out, value)
            return 0
        dataset, dataset_hash = _read_json(args.dataset)
        if args.command == "truth":
            _write_json(args.out, truth(dataset))
            return 0
        rows, queries = validate_dataset(dataset)
        results, result_hash = _read_json(args.results)
        report = evaluate(rows, queries, results, mode=args.mode)
        report["dataset_sha256"] = dataset_hash
        report["results_sha256"] = result_hash
        _write_json(args.out, report)
        print(f"local check: {report['status']}")
        return {"pass": 0, "fail": 1, "incomplete": 2}[report["status"]]
    except (ValueError, OSError, UnicodeError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
