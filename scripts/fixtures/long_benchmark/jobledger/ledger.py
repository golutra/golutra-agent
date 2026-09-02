"""In-memory ledger with durable NDJSON persistence."""

from __future__ import annotations

from pathlib import Path
from typing import Iterable

from .model import JobEvent


class JobLedger:
    def __init__(self, events: Iterable[JobEvent] = ()) -> None:
        raise NotImplementedError

    def append(self, event: JobEvent) -> bool:
        raise NotImplementedError

    def events(self, job_id: str | None = None) -> list[JobEvent]:
        raise NotImplementedError

    def latest(self, job_id: str) -> JobEvent | None:
        raise NotImplementedError

    def state_counts(self) -> dict[str, int]:
        raise NotImplementedError

    def merge(self, other: "JobLedger") -> int:
        raise NotImplementedError

    def save(self, path: str | Path) -> None:
        raise NotImplementedError

    @classmethod
    def load(cls, path: str | Path) -> "JobLedger":
        raise NotImplementedError

    @classmethod
    def load_recovering(cls, path: str | Path) -> "JobLedger":
        raise NotImplementedError
