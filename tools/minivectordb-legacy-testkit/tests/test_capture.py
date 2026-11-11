import hashlib
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


class CaptureTests(unittest.TestCase):
    def capture(self, *args):
        return subprocess.run(
            [sys.executable, "-m", "minivectordb_testkit.capture", *map(str, args)],
            text=True, capture_output=True, check=False,
        )

    def test_records_real_process_output_exit_code_and_input_hash(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source = root / "source.dat"
            source.write_bytes(b"fixed input\n")
            run = root / "run"
            script = ("import os,sys; print(os.path.basename(os.getcwd())); "
                      "print(os.path.isdir(os.environ['MINIVECTORDB_TEST_DATA_DIR'])); "
                      "print('stderr example',file=sys.stderr); sys.exit(7)")
            proc = self.capture("--out", run, "--input", source, "--revision", "example",
                                "--", sys.executable, "-c", script)
            self.assertEqual(proc.returncode, 7, proc.stderr)
            manifest = json.loads((run / "manifest.json").read_text(encoding="utf8"))
            self.assertEqual(manifest["status"], "exited")
            self.assertEqual(manifest["process_scope"], "direct-process-only")
            self.assertEqual(manifest["exit_code"], 7)
            self.assertEqual(manifest["inputs"][0]["sha256_before"], hashlib.sha256(b"fixed input\n").hexdigest())
            self.assertEqual(manifest["inputs"][0]["sha256_before"], manifest["inputs"][0]["sha256_after"])
            self.assertEqual((run / "stdout.txt").read_text(encoding="utf8").splitlines(), ["work", "True"])
            self.assertIn("stderr example", (run / "stderr.txt").read_text(encoding="utf8"))

    def test_does_not_replace_existing_run_directory(self):
        with tempfile.TemporaryDirectory() as temp:
            run = Path(temp) / "run"
            run.mkdir()
            sentinel = run / "manifest.json"
            sentinel.write_text("original", encoding="utf8")
            proc = self.capture("--out", run, "--", sys.executable, "-c", "raise SystemExit(0)")
            self.assertNotEqual(proc.returncode, 0)
            self.assertEqual(sentinel.read_text(encoding="utf8"), "original")
            self.assertEqual(len(list(run.iterdir())), 1)

    def test_input_changed_during_run_is_not_reported_as_comparable(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source, run = root / "input.txt", root / "run"
            source.write_text("before", encoding="utf8")
            script = "import pathlib,sys; pathlib.Path(sys.argv[1]).write_text('after')"
            proc = self.capture("--out", run, "--input", source,
                                "--", sys.executable, "-c", script, source)
            self.assertEqual(proc.returncode, 2, proc.stderr)
            manifest = json.loads((run / "manifest.json").read_text(encoding="utf8"))
            self.assertEqual(manifest["status"], "input_changed")
            self.assertNotEqual(manifest["inputs"][0]["sha256_before"], manifest["inputs"][0]["sha256_after"])

    @unittest.skipUnless(os.name == "nt", "Windows batch execution semantics")
    def test_rejects_windows_batch_file_and_path_resolved_batch_command(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            batch = root / "testkit_probe.cmd"
            marker = root / "executed.txt"
            batch.write_text('@echo called > "%~dp0executed.txt"\r\n', encoding="ascii")
            env = dict(os.environ, PATH=str(root) + os.pathsep + os.environ.get("PATH", ""))
            for index, executable in enumerate((str(batch), "testkit_probe")):
                result = subprocess.run(
                    [sys.executable, "-m", "minivectordb_testkit.capture",
                     "--out", str(root / f"run{index}"), "--", executable],
                    text=True, capture_output=True, check=False, env=env,
                )
                self.assertNotEqual(result.returncode, 0, result.stderr)
                self.assertFalse(marker.exists())


if __name__ == "__main__":
    unittest.main()
