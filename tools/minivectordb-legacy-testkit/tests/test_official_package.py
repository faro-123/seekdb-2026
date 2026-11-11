"""Exercise official case dispatch with a synthetic session, not a database."""

import importlib.util
import json
import sys
import unittest
from pathlib import Path


PACKAGE = Path(__file__).resolve().parents[1]
REPOSITORY = PACKAGE.parents[1]
PROTOCOL = REPOSITORY / "protocol_check"
sys.path.insert(0, str(PROTOCOL))
spec = importlib.util.spec_from_file_location("official_protocol_driver", PROTOCOL / "run.py")
driver = importlib.util.module_from_spec(spec)
spec.loader.exec_module(driver)


class SyntheticSession:
    def __init__(self, responses):
        self.responses = iter(responses)
        self.modes = []
        self.events = []

    def request(self, sql, mode):
        self.modes.append(mode)
        self.events.append(("sql", sql))
        return next(self.responses)

    def shutdown(self):
        self.events.append(("shutdown",))

    def start(self):
        self.events.append(("start",))


class OfficialPackageTests(unittest.TestCase):
    def test_official_driver_dispatches_restart_and_ann_for_adapted_case(self):
        responses = [
            {"ok": True}, {"ok": True, "affected_rows": 3}, {"ok": True},
            {"ok": True}, {"ok": True, "affected_rows": 1},
            {"ok": True, "affected_rows": 1}, {"ok": True, "affected_rows": 1},
            {"ok": True, "columns": ["id", "distance"], "rows": [[4, 0.0], [1, 5.0]]},
            {"ok": True},
            {"ok": True, "columns": ["id", "distance"], "rows": [[1, 0.0], [2, 1.0]]},
            {"ok": True}, {"ok": True, "affected_rows": 1},
            {"ok": True, "affected_rows": 1}, {"ok": True, "affected_rows": 1},
            {"ok": True},
            {"ok": True, "columns": ["id", "distance"], "rows": [[4, 0.0], [1, 5.0]]},
            {"ok": True, "columns": ["id", "distance"], "rows": [[4, 0.0], [1, 5.0]]},
        ]
        case = next(x for x in json.loads((PACKAGE / "manifest.json").read_text(encoding="utf8"))["cases"]
                    if x["id"] == "legacy_index_rollback_restart")
        session = SyntheticSession(responses)
        driver.check_case(session, case, PACKAGE, {"abs_tol": 1e-5, "rel_tol": 1e-5, "error_codes": {}})
        self.assertEqual(session.modes, ["exact"] * 16 + ["ann"])
        self.assertEqual([event[0] for event in session.events].count("shutdown"), 1)
        self.assertEqual([event[0] for event in session.events].count("start"), 1)


if __name__ == "__main__":
    unittest.main()
