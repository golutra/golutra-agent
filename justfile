set dotenv-load := false

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

clippy:
    cargo clippy --workspace --all-targets -- -D warnings

test:
    cargo test --workspace

check:
    cargo check --workspace

schema:
    cargo run -p golutra-protocol-fixtures --bin export_sdk_schema -- schemas/sdk-protocol.schema.json
    npm run --prefix sdk/typescript generate
    python3 sdk/python/scripts/generate.py
    cargo test -p golutra-protocol-fixtures schema_smoke -- --nocapture

fixture:
    cargo test -p golutra-protocol-fixtures

smoke:
    cargo test --workspace

replay-smoke:
    cargo test -p golutra-protocol-fixtures

transport-smoke:
    cargo test -p golutra-client

tui-driver-process-smoke:
    cargo test -p golutra-tui --test tui_driver_process -- --test-threads=1

provider-golden:
    cargo test -p golutra-llm --test provider_golden -- --skip live_provider_smoke_is_opt_in_and_never_reads_normal_user_credentials

provider-live-smoke:
    cargo test -p golutra-llm --test provider_golden live_provider_smoke_is_opt_in_and_never_reads_normal_user_credentials -- --nocapture

ts-check:
    npm test --prefix sdk/typescript

py-check:
    python3 sdk/python/scripts/generate.py
    python3 -m compileall -q sdk/python/src sdk/python/tests
    PYTHONPATH=sdk/python/src python3 -m unittest discover -s sdk/python/tests -v

release-package:
    python3 scripts/package_release.py

release-package-smoke:
    python3 -m unittest discover -s scripts/tests -v
