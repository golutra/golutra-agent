from __future__ import annotations

import sys
import unittest
from pathlib import Path
from typing import get_args


SDK_SRC = Path(__file__).resolve().parents[1] / "src"
sys.path.insert(0, str(SDK_SRC))

from golutra_sdk.generated import DriverEnvelope, DriverKey, DriverResponseEnvelope, WaitCondition


class GeneratedContractTest(unittest.TestCase):
    def test_tui_driver_discriminated_unions_keep_their_variants(self) -> None:
        request_variants = {variant.__name__: variant for variant in get_args(DriverEnvelope)}
        self.assertIn("DriverEnvelopeInputPrompt", request_variants)
        self.assertIn("DriverEnvelopeSnapshot", request_variants)
        self.assertEqual(
            set(request_variants["DriverEnvelopeInputPrompt"].__annotations__),
            {"request_id", "text", "type"},
        )

        response_variants = {
            variant.__name__: variant for variant in get_args(DriverResponseEnvelope)
        }
        self.assertIn("DriverResponseEnvelopeReady", response_variants)
        self.assertIn("DriverResponseEnvelopeError", response_variants)
        self.assertIn("request_id", response_variants["DriverResponseEnvelopeError"].__annotations__)

        wait_variants = {variant.__name__ for variant in get_args(WaitCondition)}
        self.assertIn("WaitConditionTaskTerminal", wait_variants)
        self.assertIn("WaitConditionEvent", wait_variants)

        key_variants = get_args(DriverKey)
        self.assertTrue(any(getattr(variant, "__name__", "") == "DriverKeyChar" for variant in key_variants))


if __name__ == "__main__":
    unittest.main()
