"""The live driver must never report a successful process exit for a failed case."""
import contextlib
import importlib.util
import io
import os
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location('qualify_acp', Path(__file__).resolve().parents[1] / 'scripts/qualify_acp.py')
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class QualificationExitTests(unittest.TestCase):
    def verdict(self, passed, *, crash=False, empty=False, cleanup_error=False):
        with tempfile.TemporaryDirectory(dir=os.environ['TMPDIR']) as directory:
            driver = MODULE.Driver.__new__(MODULE.Driver)
            driver.a = SimpleNamespace(cases=[] if empty else ['chain:fixture'])
            driver.ev = Path(directory)
            driver.results = {}
            driver.prepare = driver.start = driver.stop = driver.stop_postgres = lambda: None
            def case(profile):
                if crash:
                    raise RuntimeError('controlled qualification failure')
                driver.results['chain-' + profile] = {'passed': passed}
            driver.case_chain = case
            if cleanup_error:
                driver.stop_postgres = lambda: driver.results.update(pg_stop_error='controlled shutdown failure')
            with patch.object(MODULE.signal, 'signal'), contextlib.redirect_stdout(io.StringIO()):
                driver.main()

    def test_failed_case_exits_nonzero(self):
        with self.assertRaises(SystemExit) as result:
            self.verdict(False)
        self.assertEqual(result.exception.code, 1)

    def test_case_exception_exits_nonzero(self):
        with self.assertRaises(SystemExit) as result:
            self.verdict(False, crash=True)
        self.assertEqual(result.exception.code, 1)

    def test_empty_qualification_is_not_success(self):
        with self.assertRaises(SystemExit) as result:
            self.verdict(False, empty=True)
        self.assertEqual(result.exception.code, 1)

    def test_cleanup_diagnostic_exits_nonzero(self):
        with self.assertRaises(SystemExit) as result:
            self.verdict(True, cleanup_error=True)
        self.assertEqual(result.exception.code, 1)

    def test_all_passed_cases_finish_successfully(self):
        self.verdict(True)


if __name__ == '__main__':
    unittest.main()
