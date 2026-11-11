import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from minivectordb_testkit.dataset import generate_dataset, truth


class OfflineCliTests(unittest.TestCase):
    def run_tool(self, *args):
        return subprocess.run(
            [sys.executable, "-m", "minivectordb_testkit", *map(str, args)],
            text=True, capture_output=True, check=False,
        )

    def test_seeded_dataset_and_truth_can_be_checked_without_database(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            first, second = root / "first.json", root / "second.json"
            args = ["--rows", "8", "--dim", "2", "--queries", "4", "--k", "2", "--seed", "17"]
            for path in (first, second):
                result = self.run_tool("generate", "--out", path, *args)
                self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(first.read_bytes(), second.read_bytes())
            dataset = json.loads(first.read_text(encoding="utf8"))
            self.assertEqual((len(dataset["rows"]), len(dataset["queries"])), (8, 4))
            truth, report = root / "truth.json", root / "report.json"
            result = self.run_tool("truth", "--dataset", first, "--out", truth)
            self.assertEqual(result.returncode, 0, result.stderr)
            result = self.run_tool("check", "--dataset", first, "--results", truth,
                                   "--mode", "exact", "--out", report)
            self.assertEqual(result.returncode, 0, result.stderr)
            evaluated = json.loads(report.read_text(encoding="utf8"))
            self.assertEqual(evaluated["status"], "pass")
            self.assertEqual(evaluated["groups"]["filtered"]["samples"], 2)
            self.assertEqual(evaluated["groups"]["unfiltered"]["samples"], 2)

    def test_bad_distance_fails_and_existing_output_is_not_overwritten(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            dataset, truth, report = (root / x for x in ("data.json", "truth.json", "report.json"))
            result = self.run_tool("generate", "--out", dataset, "--rows", "4",
                                   "--dim", "2", "--queries", "2", "--k", "2")
            self.assertEqual(result.returncode, 0, result.stderr)
            result = self.run_tool("truth", "--dataset", dataset, "--out", truth)
            self.assertEqual(result.returncode, 0, result.stderr)
            results = json.loads(truth.read_text(encoding="utf8"))
            results["q0000"][0]["distance"] = 1e9
            truth.write_text(json.dumps(results), encoding="utf8")
            result = self.run_tool("check", "--dataset", dataset, "--results", truth,
                                   "--mode", "ann", "--out", report)
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertEqual(json.loads(report.read_text(encoding="utf8"))["status"], "fail")
            before = report.read_bytes()
            result = self.run_tool("generate", "--out", report)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(report.read_bytes(), before)

    def test_declared_dimension_is_validated_even_when_no_queries_exist(self):
        dataset = generate_dataset(2, 2, 0, 2, 17)
        dataset["dimension"] = 3
        with self.assertRaises(ValueError):
            truth(dataset)

    def test_json_decimal_is_rounded_directly_to_float32(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source, output = root / "data.json", root / "truth.json"
            source.write_text(
                '{"format":"minivectordb-offline-dataset-v1","dimension":1,'
                '"rows":[{"id":1,"category":"a","vector":'
                '[1.00000005960464477539062582718061255302767487140869206996285356581211090087890625]},'
                '{"id":2,"category":"a","vector":[1.0]}],'
                '"queries":[{"id":"q","vector":[1.0],"category":null,"k":1}]}',
                encoding="utf8",
            )
            result = self.run_tool("truth", "--dataset", source, "--out", output)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads(output.read_text(encoding="utf8"))["q"][0]["id"], 2)


if __name__ == "__main__":
    unittest.main()
