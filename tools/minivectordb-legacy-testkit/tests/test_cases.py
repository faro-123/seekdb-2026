import json
import unittest
from pathlib import Path

from minivectordb_testkit.oracle import exact_topk


ROOT = Path(__file__).resolve().parents[1]


class AdaptedCaseTests(unittest.TestCase):
    def test_manifest_sql_and_result_assertions_align(self):
        manifest = json.loads((ROOT / "manifest.json").read_text(encoding="utf8"))
        seen = set()
        for case in manifest["cases"]:
            with self.subTest(case=case["id"]):
                self.assertNotIn(case["id"], seen)
                seen.add(case["id"])
                lines = (ROOT / case["test"]).read_text(encoding="utf8").splitlines()
                sql = []
                for line in (line.strip() for line in lines if line.strip()):
                    if line.startswith("--"):
                        self.assertIn(line, ("--restart", "--mode ann"))
                    else:
                        self.assertTrue(line.endswith(";"))
                        sql.append(line)
                assertions = json.loads((ROOT / case["result"]).read_text(encoding="utf8"))
                self.assertEqual(len(sql), case["statements"])
                self.assertEqual(len(assertions), len(sql))
                for assertion in assertions:
                    self.assertIsInstance(assertion, dict)
                    if "rows" in assertion:
                        self.assertTrue(all(len(row) == len(assertion["columns"]) for row in assertion["rows"]))

    def test_vector_expected_rows_match_independent_reference_state(self):
        scenarios = (
            ("legacy_filtered_topk", [
                ([{"id": 8, "category": "keep", "vector": [3, 4]},
                  {"id": 3, "category": "keep", "vector": [-3, -4]},
                  {"id": 1, "category": "skip", "vector": [0, 0]},
                  {"id": 2, "category": "keep", "vector": [0, 6]},
                  {"id": 5, "category": "skip", "vector": [0, 1]}], "keep", 2),
                ([{"id": 8, "category": "keep", "vector": [3, 4]},
                  {"id": 3, "category": "keep", "vector": [-3, -4]},
                  {"id": 2, "category": "keep", "vector": [0, 6]}], "keep", 9),
                ([], "none", 9),
            ]),
            ("legacy_index_rollback_restart", [
                ([{"id": 1, "category": "keep", "vector": [3, 4]},
                  {"id": 4, "category": "keep", "vector": [0, 0]}], "keep", 10),
                ([{"id": 1, "category": "keep", "vector": [0, 0]},
                  {"id": 2, "category": "keep", "vector": [1, 0]}], "keep", 10),
                ([{"id": 1, "category": "keep", "vector": [3, 4]},
                  {"id": 4, "category": "keep", "vector": [0, 0]}], "keep", 10),
                ([{"id": 1, "category": "keep", "vector": [3, 4]},
                  {"id": 4, "category": "keep", "vector": [0, 0]}], "keep", 10),
            ]),
            ("legacy_failed_index_tx", [
                ([{"id": 1, "category": "keep", "vector": [0, 5]},
                  {"id": 2, "category": "keep", "vector": [3, 4]}], "keep", 10),
            ] * 3),
        )
        for name, states in scenarios:
            assertions = json.loads((ROOT / "cases" / (name + ".result")).read_text(encoding="utf8"))
            actual_queries = [entry["rows"] for entry in assertions if "rows" in entry]
            expected_queries = [
                [list(pair) for pair in exact_topk(rows, [0, 0], k, category)]
                for rows, category, k in states
            ]
            with self.subTest(case=name):
                self.assertEqual(actual_queries, expected_queries)


if __name__ == "__main__":
    unittest.main()
