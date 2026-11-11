"""Small deterministic datasets for offline vector-correctness experiments."""

import random

from .oracle import exact_topk, float32


def generate_dataset(row_count, dimension, query_count, k, seed):
    values = (row_count, dimension, query_count, k, seed)
    if any(isinstance(value, bool) or not isinstance(value, int) for value in values):
        raise ValueError("dataset parameters must be integers")
    if row_count < 0 or dimension <= 0 or query_count < 0 or k < 0:
        raise ValueError("rows, queries, and k must be nonnegative; dimension must be positive")
    rng = random.Random(seed)

    def vector():
        return [float32(rng.uniform(-1, 1)) for _ in range(dimension)]

    rows = [{"id": i + 1, "category": "a" if i % 2 == 0 else "b", "vector": vector()}
            for i in range(row_count)]
    queries = [{"id": f"q{i:04d}", "vector": vector(), "k": k,
                "category": None if i % 2 == 0 else "a"}
               for i in range(query_count)]
    return {"format": "minivectordb-offline-dataset-v1", "seed": seed,
            "dimension": dimension, "rows": rows, "queries": queries}


def validate_dataset(dataset):
    if not isinstance(dataset, dict) or dataset.get("format") != "minivectordb-offline-dataset-v1":
        raise ValueError("unsupported dataset format")
    rows, queries = dataset.get("rows"), dataset.get("queries")
    if not isinstance(rows, list) or not isinstance(queries, list):
        raise ValueError("dataset needs rows and queries arrays")
    dimension = dataset.get("dimension")
    if isinstance(dimension, bool) or not isinstance(dimension, int) or dimension <= 0:
        raise ValueError("dimension must be a positive integer")
    for row in rows:
        if not isinstance(row, dict) or not isinstance(row.get("vector"), list) or len(row["vector"]) != dimension:
            raise ValueError("row does not match declared dimension")
    if rows:
        exact_topk(rows, rows[0]["vector"], 0)
    ids = set()
    for query in queries:
        if not isinstance(query, dict) or not isinstance(query.get("id"), str):
            raise ValueError("each query needs a string id")
        query_id = query["id"]
        if query_id in ids:
            raise ValueError("duplicate query id")
        ids.add(query_id)
        if not isinstance(query.get("vector"), list) or len(query["vector"]) != dimension:
            raise ValueError("query does not match declared dimension")
        exact_topk([], query["vector"], query.get("k"), query.get("category"))
    return rows, queries


def truth(dataset):
    rows, queries = validate_dataset(dataset)
    answers = {}
    for query in queries:
        query_id = query["id"]
        answers[query_id] = [
            {"id": primary_key, "distance": distance}
            for primary_key, distance in exact_topk(
                rows, query.get("vector"), query.get("k"), query.get("category")
            )
        ]
    return answers
