"""Canonical event model."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Mapping


@dataclass(frozen=True)
class JobEvent:
    event_id: str
    job_id: str
    state: str
    sequence: int
    metadata: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_mapping(cls, value: Mapping[str, Any]) -> "JobEvent":
        raise NotImplementedError

    def to_mapping(self) -> dict[str, Any]:
        raise NotImplementedError
