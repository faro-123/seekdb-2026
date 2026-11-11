"""Independent float32 and vector-search reference calculations."""

import math
import struct
from decimal import Decimal
from fractions import Fraction


def float32(value):
    """Round a finite JSON number to IEEE 754 binary32, rejecting overflow."""
    if isinstance(value, bool) or not isinstance(value, (int, float, Decimal)):
        raise ValueError("vector elements must be numbers")
    if isinstance(value, float):
        try:
            result = struct.unpack("<f", struct.pack("<f", value))[0]
        except (OverflowError, struct.error) as exc:
            raise ValueError("vector element is outside float32 range") from exc
        if not math.isfinite(result):
            raise ValueError("vector elements must be finite float32 values")
        return result
    if isinstance(value, Decimal) and not value.is_finite():
        raise ValueError("vector elements must be finite float32 values")
    negative = value.is_signed() if isinstance(value, Decimal) else value < 0
    if value == 0:
        return -0.0 if negative else 0.0
    if isinstance(value, Decimal):
        if value.adjusted() >= 39:
            raise ValueError("vector element is outside float32 range")
        if value.adjusted() < -46:
            return -0.0 if negative else 0.0
        numerator, denominator = value.as_integer_ratio()
    else:
        numerator, denominator = value, 1
    numerator = abs(numerator)
    exponent = numerator.bit_length() - denominator.bit_length()
    if exponent >= 0:
        if numerator < denominator << exponent:
            exponent -= 1
    elif numerator << -exponent < denominator:
        exponent -= 1
    if exponent > 127:
        raise ValueError("vector element is outside float32 range")
    if exponent < -150:
        return -0.0 if negative else 0.0

    def rounded_ratio(shift):
        n, d = numerator, denominator
        if shift >= 0:
            n <<= shift
        else:
            d <<= -shift
        quotient, remainder = divmod(n, d)
        if 2 * remainder > d or (2 * remainder == d and quotient & 1):
            quotient += 1
        return quotient

    if exponent < -126:
        bits = rounded_ratio(149)
    else:
        significand = rounded_ratio(23 - exponent)
        if significand == 1 << 24:
            significand >>= 1
            exponent += 1
        if exponent > 127:
            raise ValueError("vector element is outside float32 range")
        bits = ((exponent + 127) << 23) | (significand - (1 << 23))
    if negative:
        bits |= 1 << 31
    return struct.unpack("<f", struct.pack("<I", bits))[0]


def _vector(values):
    if not isinstance(values, (list, tuple)) or not values:
        raise ValueError("vectors must be nonempty arrays")
    return tuple(float32(value) for value in values)


def _prepare(rows, query, k, category):
    if isinstance(k, bool) or not isinstance(k, int) or k < 0:
        raise ValueError("k must be a nonnegative integer")
    if category is not None and not isinstance(category, str):
        raise ValueError("category filter must be a string or null")
    query = _vector(query)
    exact_query = tuple(Fraction.from_float(value) for value in query)
    prepared = []
    ids = set()
    for row in rows:
        if not isinstance(row, dict):
            raise ValueError("each row must be an object")
        primary_key = row.get("id")
        if (isinstance(primary_key, bool) or not isinstance(primary_key, int)
                or not -(1 << 63) <= primary_key < (1 << 63)):
            raise ValueError("row id must be a signed BIGINT")
        if primary_key in ids:
            raise ValueError("duplicate row id")
        ids.add(primary_key)
        row_category = row.get("category")
        if not isinstance(row_category, str):
            raise ValueError("row category must be a string")
        vector = _vector(row.get("vector"))
        if len(vector) != len(query):
            raise ValueError("vector dimension mismatch")
        if category is None or row_category == category:
            distance = math.hypot(*(a - b for a, b in zip(vector, query)))
            squared = sum(
                ((Fraction.from_float(a) - b) ** 2 for a, b in zip(vector, exact_query)),
                Fraction(0),
            )
            prepared.append((primary_key, distance, squared))
    prepared.sort(key=lambda item: (item[2], item[0]))
    return [(primary_key, distance) for primary_key, distance, _ in prepared]


def exact_topk(rows, query, k, category=None):
    """Return exact (id, Euclidean distance) pairs after filtering."""
    return _prepare(rows, query, k, category)[:k]


def _finite_number(value):
    if isinstance(value, bool) or not isinstance(value, (int, float, Decimal)):
        return False
    try:
        return math.isfinite(value)
    except OverflowError:
        return False


def evaluate(rows, queries, results, mode="ann", threshold=0.95):
    """Validate result sets and score filtered/unfiltered mean recall separately."""
    if mode not in ("exact", "ann"):
        raise ValueError("mode must be exact or ann")
    if not _finite_number(threshold) or not 0 <= threshold <= 1:
        raise ValueError("threshold must be finite and between zero and one")
    if not isinstance(queries, list) or not isinstance(results, dict):
        raise ValueError("queries must be an array and results an object")
    if not isinstance(rows, list):
        raise ValueError("rows must be an array")

    errors = []
    observed = {"filtered": [], "unfiltered": []}
    details = []
    seen_queries = set()
    for query in queries:
        if not isinstance(query, dict) or not isinstance(query.get("id"), str):
            raise ValueError("each query needs a string id")
        query_id = query["id"]
        if query_id in seen_queries:
            raise ValueError("duplicate query id")
        seen_queries.add(query_id)
        k = query.get("k")
        category = query.get("category")
        ranked = _prepare(rows, query.get("vector"), k, category)
        expected = ranked[:k]
        eligible = dict(ranked)
        positions = {primary_key: index for index, (primary_key, _) in enumerate(ranked)}
        group = "unfiltered" if category is None else "filtered"
        answer = results.get(query_id)
        local_errors = []
        if query_id not in results:
            local_errors.append("missing query result")
        elif not isinstance(answer, list):
            local_errors.append("result must be an array")
        else:
            if len(answer) != len(expected):
                local_errors.append("wrong result count")
            ids = []
            order_keys = []
            for item in answer:
                if not isinstance(item, dict):
                    local_errors.append("result row must be an object")
                    continue
                primary_key = item.get("id")
                if isinstance(primary_key, bool) or not isinstance(primary_key, int):
                    local_errors.append("result id must be an integer")
                    continue
                ids.append(primary_key)
                if primary_key not in eligible:
                    local_errors.append("result id is invalid or outside the filter")
                    continue
                expected_distance = eligible[primary_key]
                order_keys.append(positions[primary_key])
                distance = item.get("distance")
                if (not _finite_number(distance)
                        or not math.isclose(distance, expected_distance, rel_tol=1e-5, abs_tol=1e-5)):
                    local_errors.append("distance does not match the stored vector")
            if len(ids) != len(set(ids)):
                local_errors.append("duplicate result id")
            if order_keys != sorted(order_keys):
                local_errors.append("results are not ordered by distance and primary key")
            expected_ids = [primary_key for primary_key, _ in expected]
            if mode == "exact" and ids != expected_ids:
                local_errors.append("exact result differs from reference")

        recall = None
        if not local_errors and expected:
            returned_ids = {item["id"] for item in answer}
            exact_recall = Fraction(
                len(returned_ids.intersection(primary_key for primary_key, _ in expected)), len(expected)
            )
            observed[group].append(exact_recall)
            recall = float(exact_recall)
        errors.extend(f"{query_id}: {message}" for message in local_errors)
        details.append({"id": query_id, "group": group, "eligible": len(ranked),
                        "expected_count": len(expected), "recall": recall})

    for unexpected in sorted(set(results) - seen_queries, key=str):
        errors.append(f"{unexpected}: unexpected query result")
    groups = {}
    means = {}
    for name, recalls in observed.items():
        means[name] = sum(recalls, Fraction(0)) / len(recalls) if recalls else None
        groups[name] = {"samples": len(recalls),
                        "mean_recall": float(means[name]) if recalls else None}
    if errors:
        status = "fail"
    elif any(mean is not None and mean < Fraction(str(threshold)) for mean in means.values()):
        status = "fail"
    elif any(group["mean_recall"] is None for group in groups.values()):
        status = "incomplete"
    else:
        status = "pass"
    return {"status": status, "mode": mode, "threshold": threshold,
            "groups": groups, "queries": details, "errors": errors}
