from __future__ import annotations

import sys
import unittest
from pathlib import Path
from typing import get_args


SDK_SRC = Path(__file__).resolve().parents[1] / "src"
sys.path.insert(0, str(SDK_SRC))

from golutra_sdk.generated import (
    DriverEnvelope,
    DriverKey,
    DriverResponseEnvelope,
    TaskReconciliationDecision,
    TaskRecoveryRecord,
    TaskStatus,
    WaitCondition,
)


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

    def test_recovery_contract_is_exported_to_generated_clients(self) -> None:
        self.assertIn("interrupted", get_args(TaskStatus))
        self.assertIn("uncertain", get_args(TaskStatus))
        self.assertEqual(
            set(get_args(TaskReconciliationDecision)),
            {"no_side_effect_observed", "side_effect_observed", "abandon"},
        )
        self.assertIn("reconciliation_required", TaskRecoveryRecord.__annotations__)


if __name__ == "__main__":
    unittest.main()
