"""Canonical NDJSON codec."""

from __future__ import annotations

from .model import JobEvent


def encode_event(event: JobEvent) -> str:
    raise NotImplementedError


def decode_event(line: str) -> JobEvent:
    raise NotImplementedError
