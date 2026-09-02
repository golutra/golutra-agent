from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from jobledger.codec import decode_event, encode_event
from jobledger.ledger import JobLedger
from jobledger.model import JobEvent


class JobLedgerStageOneTest(unittest.TestCase):
    def event(self, event_id: str, job_id: str, state: str, sequence: int) -> JobEvent:
        return JobEvent.from_mapping(
            {
                "event_id": event_id,
                "job_id": job_id,
                "state": state,
                "sequence": sequence,
                "metadata": {"source": "visible-test"},
            }
        )

    def test_codec_is_canonical_and_round_trips(self) -> None:
        event = self.event("event-2", "job-1", "running", 2)
        encoded = encode_event(event)
        self.assertEqual(encoded, json.dumps(event.to_mapping(), sort_keys=True, separators=(",", ":")))
        self.assertEqual(decode_event(encoded), event)

    def test_events_are_sorted_and_duplicates_are_idempotent(self) -> None:
        later = self.event("event-2", "job-1", "running", 2)
        earlier = self.event("event-1", "job-1", "queued", 1)
        ledger = JobLedger([later])
        self.assertTrue(ledger.append(earlier))
        self.assertFalse(ledger.append(earlier))
        self.assertEqual([event.event_id for event in ledger.events()], ["event-1", "event-2"])
        self.assertEqual(ledger.latest("job-1"), later)
        self.assertEqual(ledger.state_counts(), {"queued": 1, "running": 1})

    def test_save_and_load(self) -> None:
        ledger = JobLedger([self.event("event-1", "job-1", "succeeded", 1)])
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "ledger.ndjson"
            ledger.save(path)
            self.assertTrue(path.read_text(encoding="utf-8").endswith("\n"))
            self.assertEqual(JobLedger.load(path).events(), ledger.events())


if __name__ == "__main__":
    unittest.main()
