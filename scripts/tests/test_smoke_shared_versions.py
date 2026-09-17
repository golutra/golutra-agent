from pathlib import Path
import sys
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import smoke_shared_versions as smoke


LEGACY_ERROR = "sqlite operation failed: unknown query parameter `\\C:\\fixture` while parsing connection URL"


class LegacyBaselineTest(unittest.TestCase):
    def exercise(self, *, corrupt=False, candidate_error="data_unsupported"):
        baseline, candidate = Path("legacy.exe"), Path("candidate.exe")

        def run(binary, args, workspace, env, *, success=True):
            database = Path(env["GOLUTRA_AGENT_HOME"]) / "state/runtime.sqlite"
            if not database.exists():
                database.parent.mkdir(parents=True)
                with smoke.database_connection(database) as connection:
                    connection.executescript("CREATE TABLE schema_migrations(version INTEGER);"
                                             "INSERT INTO schema_migrations VALUES (7);"
                                             "CREATE TABLE preserved(value TEXT);"
                                             "INSERT INTO preserved VALUES ('original');")
                return "fixture complete"
            with smoke.database_connection(database) as connection:
                if binary == baseline:
                    if corrupt:
                        connection.execute("UPDATE preserved SET value = 'corrupt'")
                    return LEGACY_ERROR
                version = connection.execute("SELECT version FROM schema_migrations").fetchone()[0]
                return "current history" if version == 7 else candidate_error

        with patch.object(smoke.subprocess, "run", return_value=SimpleNamespace(returncode=1, stderr=LEGACY_ERROR)), \
                patch.object(smoke, "run", side_effect=run):
            return smoke.check_unavailable_windows_baseline(baseline, candidate)

    def test_known_legacy_failure_still_requires_unchanged_data_and_candidate_rejection(self):
        report = self.exercise()
        self.assertEqual(report["old_schema_fixture"], {"version": 5, "synthetic": True})
        self.assertTrue(report["legacy_reader_blocked_without_data_changes"])
        self.assertTrue(report["candidate_rejected_old_schema_without_data_changes"])
        with self.assertRaisesRegex(smoke.PackageError, "modified current data"):
            self.exercise(corrupt=True)
        with self.assertRaisesRegex(smoke.PackageError, "did not reject"):
            self.exercise(candidate_error="unrelated failure")

    def test_other_startup_errors_cannot_be_reported_as_known_legacy_limitations(self):
        with patch.object(smoke.subprocess, "run", return_value=SimpleNamespace(returncode=1, stderr="access denied")):
            with self.assertRaisesRegex(smoke.PackageError, "unexpected legacy startup failure"):
                smoke.check_unavailable_windows_baseline(Path("legacy"), Path("candidate"))

    def test_working_baseline_requires_normal_bidirectional_acceptance(self):
        with patch.object(smoke.subprocess, "run", return_value=SimpleNamespace(returncode=0, stderr="")):
            self.assertIsNone(smoke.check_unavailable_windows_baseline(Path("legacy"), Path("candidate")))


if __name__ == "__main__":
    unittest.main()
