import math
import random
import struct
import unittest
from decimal import Decimal, localcontext

try:
    from minivectordb_testkit import oracle
except ImportError:
    oracle = None


class Float32Tests(unittest.TestCase):
    def test_converts_before_distance_computation(self):
        self.assertIsNotNone(oracle, "vector oracle is not implemented")
        self.assertEqual(oracle.float32(16777217), 16777216.0)
        self.assertEqual(oracle.float32(18014399583223809), 18014400656965632.0)

    def test_decimal_rounds_directly_to_float32_without_double_rounding(self):
        midpoint = Decimal("1.000000059604644775390625")
        above = Decimal("1.00000005960464477539062582718061255302767487140869206996285356581211090087890625")
        self.assertEqual(oracle.float32(midpoint), 1.0)
        self.assertEqual(oracle.float32(above), 1.0000001192092896)

    def test_rejects_non_finite_overflow_and_non_numeric_values(self):
        for value in (float("nan"), float("inf"), -float("inf"), 1e100,
                      Decimal("NaN"), Decimal("1e39"), True, "1"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                oracle.float32(value)

    def test_decimal_float32_midpoints_round_to_nearest_even(self):
        rng = random.Random(19)
        with localcontext() as context:
            context.prec = 200
            for _ in range(100):
                bits = rng.randrange(0x00800000, 0x7F7FFFFE)
                lower = struct.unpack("<f", struct.pack("<I", bits))[0]
                upper = struct.unpack("<f", struct.pack("<I", bits + 1))[0]
                low, high = Decimal.from_float(lower), Decimal.from_float(upper)
                midpoint = (low + high) / 2
                tiny = (high - low) / (2**60)
                self.assertEqual(oracle.float32(midpoint - tiny), lower)
                self.assertEqual(oracle.float32(midpoint + tiny), upper)
                self.assertEqual(oracle.float32(midpoint), lower if bits % 2 == 0 else upper)

    def test_decimal_roundtrip_preserves_finite_float32_bit_patterns(self):
        rng = random.Random(31)
        for _ in range(500):
            bits = rng.getrandbits(32)
            if (bits >> 23) & 0xFF == 0xFF:
                continue
            expected = struct.unpack("<f", struct.pack("<I", bits))[0]
            actual = oracle.float32(Decimal.from_float(expected))
            self.assertEqual(struct.pack("<f", actual), struct.pack("<I", bits))


class ExactSearchTests(unittest.TestCase):
    def setUp(self):
        self.rows = [
            {"id": 8, "category": "keep", "vector": [3, 4]},
            {"id": 3, "category": "keep", "vector": [-3, -4]},
            {"id": 1, "category": "skip", "vector": [0, 0]},
            {"id": 2, "category": "keep", "vector": [0, 6]},
        ]

    def test_filters_before_topk_and_breaks_distance_ties_by_primary_key(self):
        self.assertEqual(
            oracle.exact_topk(self.rows, [0, 0], 2, "keep"),
            [(3, 5.0), (8, 5.0)],
        )

    def test_handles_empty_filter_k_larger_than_count_and_zero(self):
        self.assertEqual(oracle.exact_topk(self.rows, [0, 0], 10, "absent"), [])
        self.assertEqual(len(oracle.exact_topk(self.rows, [0, 0], 10, "keep")), 3)
        self.assertEqual(oracle.exact_topk(self.rows, [0, 0], 0, "keep"), [])

    def test_orders_distinct_distances_even_if_double_precision_displays_a_tie(self):
        rows = [
            {"id": 1, "category": "a", "vector": [1, 2**-30]},
            {"id": 2, "category": "a", "vector": [1, 0]},
        ]
        self.assertEqual([item[0] for item in oracle.exact_topk(rows, [0, 0], 2)], [2, 1])

    def test_exact_checker_accepts_the_precise_order_when_display_distances_tie(self):
        rows = [
            {"id": 1, "category": "a", "vector": [1, 2**-30]},
            {"id": 2, "category": "a", "vector": [1, 0]},
        ]
        queries = [
            {"id": "all", "vector": [0, 0], "k": 2, "category": None},
            {"id": "a", "vector": [0, 0], "k": 2, "category": "a"},
        ]
        answer = [{"id": 2, "distance": 1.0}, {"id": 1, "distance": 1.0}]
        self.assertEqual(oracle.evaluate(rows, queries, {"all": answer, "a": answer}, mode="exact")["status"], "pass")

    def test_rejects_bad_dimension_duplicate_primary_key_and_invalid_k(self):
        for rows, query, k in (
            (self.rows, [0], 1),
            (self.rows + [self.rows[0]], [0, 0], 1),
            (self.rows, [0, 0], -1),
            (self.rows, [0, math.inf], 1),
        ):
            with self.subTest(rows=len(rows), query=query, k=k), self.assertRaises(ValueError):
                oracle.exact_topk(rows, query, k)

    def test_result_is_invariant_under_input_permutation_and_integer_translation(self):
        rng = random.Random(20261110)
        for _ in range(40):
            rows = [
                {"id": i, "category": "a" if i % 2 else "b",
                 "vector": [rng.randint(-20, 20), rng.randint(-20, 20)]}
                for i in range(12)
            ]
            query = [rng.randint(-10, 10), rng.randint(-10, 10)]
            expected = oracle.exact_topk(rows, query, 5, "a")
            rng.shuffle(rows)
            self.assertEqual(oracle.exact_topk(rows, query, 5, "a"), expected)
            moved = [{**row, "vector": [x + 7 for x in row["vector"]]} for row in rows]
            self.assertEqual(oracle.exact_topk(moved, [x + 7 for x in query], 5, "a"), expected)


class RecallTests(unittest.TestCase):
    def setUp(self):
        self.rows = [
            {"id": 1, "category": "a", "vector": [0, 0]},
            {"id": 2, "category": "a", "vector": [1, 0]},
            {"id": 3, "category": "b", "vector": [2, 0]},
            {"id": 4, "category": "b", "vector": [3, 0]},
        ]
        self.queries = [
            {"id": "all", "vector": [0, 0], "k": 2, "category": None},
            {"id": "a", "vector": [0, 0], "k": 10, "category": "a"},
            {"id": "empty", "vector": [0, 0], "k": 10, "category": "none"},
        ]
        self.results = {
            "all": [{"id": 1, "distance": 0}, {"id": 3, "distance": 2}],
            "a": [{"id": 1, "distance": 0}, {"id": 2, "distance": 1}],
            "empty": [],
        }

    def test_recall_uses_min_k_n_and_averages_groups_separately(self):
        report = oracle.evaluate(self.rows, self.queries, self.results, mode="ann")
        self.assertEqual(report["status"], "fail")
        self.assertEqual(report["groups"]["unfiltered"], {"samples": 1, "mean_recall": 0.5})
        self.assertEqual(report["groups"]["filtered"], {"samples": 1, "mean_recall": 1.0})
        self.assertEqual(report["errors"], [])

    def test_rejects_duplicate_invalid_filtered_and_wrong_distance(self):
        bad_results = (
            {"all": [{"id": 1, "distance": 0}, {"id": 1, "distance": 0}]},
            {"all": [{"id": 1, "distance": 0}, {"id": 99, "distance": 1}]},
            {"a": [{"id": 1, "distance": 0}, {"id": 3, "distance": 2}]},
            {"all": [{"id": 1, "distance": 9}, {"id": 2, "distance": 1}]},
            {"all": [{"id": 1, "distance": 10**1000}, {"id": 2, "distance": 1}]},
            {"all": [{"id": 2, "distance": 1}, {"id": 1, "distance": 0}]},
        )
        for changed in bad_results:
            with self.subTest(changed=changed):
                report = oracle.evaluate(self.rows, self.queries, {**self.results, **changed}, mode="ann")
                self.assertEqual(report["status"], "fail")
                self.assertTrue(report["errors"])

    def test_missing_extra_and_short_results_are_failures(self):
        for changed in (
            {"a": self.results["a"], "empty": []},
            {**self.results, "extra": []},
            {**self.results, "all": [{"id": 1, "distance": 0}]},
        ):
            with self.subTest(changed=changed):
                self.assertEqual(oracle.evaluate(self.rows, self.queries, changed)["status"], "fail")

    def test_empty_groups_are_incomplete_not_pass(self):
        report = oracle.evaluate(self.rows, self.queries[2:], {"empty": []})
        self.assertEqual(report["status"], "incomplete")
        self.assertIsNone(report["groups"]["filtered"]["mean_recall"])
        self.assertIsNone(report["groups"]["unfiltered"]["mean_recall"])

    def test_below_threshold_group_fails_even_if_other_group_is_missing(self):
        report = oracle.evaluate(self.rows, self.queries[:1], {"all": self.results["all"]})
        self.assertEqual(report["status"], "fail")
        self.assertIsNone(report["groups"]["filtered"]["mean_recall"])

    def test_exact_mode_rejects_a_valid_but_approximate_answer(self):
        report = oracle.evaluate(self.rows, self.queries, self.results, mode="exact")
        self.assertEqual(report["status"], "fail")
        self.assertTrue(report["errors"])

    def test_recall_exactly_at_threshold_is_not_lost_to_float_rounding(self):
        rows = [{"id": i, "category": "a", "vector": [i]} for i in range(21)]
        queries = [
            {"id": f"{group}{i}", "vector": [0], "k": 20, "category": group}
            for group in (None, "a") for i in range(3)
        ]
        answer = [{"id": i, "distance": i} for i in list(range(19)) + [20]]
        report = oracle.evaluate(rows, queries, {q["id"]: answer for q in queries}, mode="ann")
        self.assertEqual(report["groups"]["filtered"]["mean_recall"], 0.95)
        self.assertEqual(report["groups"]["unfiltered"]["mean_recall"], 0.95)
        self.assertEqual(report["status"], "pass")


if __name__ == "__main__":
    unittest.main()
