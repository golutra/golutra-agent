set dotenv-load := false

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

clippy:
    cargo clippy --workspace --all-targets --locked -- -D warnings

test:
    cargo test --workspace --locked

check:
    cargo check --workspace --locked

schema:
    cargo run --locked -p golutra-protocol-fixtures --bin export_sdk_schema -- schemas/sdk-protocol.schema.json
    npm run --prefix sdk/typescript generate
    python3 sdk/python/scripts/generate.py
    cargo test --locked -p golutra-protocol-fixtures schema_smoke -- --nocapture

fixture:
    cargo test --locked -p golutra-protocol-fixtures

smoke:
    cargo test --workspace --locked

replay-smoke:
    cargo test --locked -p golutra-protocol-fixtures

transport-smoke:
    cargo test --locked -p golutra-client

tui-driver-process-smoke:
    cargo test --locked -p golutra-tui --test tui_driver_process -- --test-threads=1

tui-driver-live-smoke:
    cargo test --locked -p golutra-tui --test tui_driver_process live_provider_driver_smoke_is_isolated_and_opt_in -- --ignored --nocapture --test-threads=1

provider-golden:
    cargo test --locked -p golutra-llm --test provider_golden -- --skip live_provider_smoke_is_opt_in_and_never_reads_normal_user_credentials

provider-live-smoke:
    cargo test --locked -p golutra-llm --test provider_golden live_provider_smoke_is_opt_in_and_never_reads_normal_user_credentials -- --nocapture

ts-check:
    npm test --prefix sdk/typescript
    npm audit --audit-level=high --prefix sdk/typescript

py-check:
    python3 sdk/python/scripts/generate.py
    python3 -m compileall -q sdk/python/src sdk/python/tests
    PYTHONPATH=sdk/python/src python3 -m unittest discover -s sdk/python/tests -v

open-source-check:
    python3 scripts/check_open_source.py

release-package:
    python3 scripts/package_release.py

release-package-smoke:
    python3 -m unittest discover -s scripts/tests -v

npm-package-root:
    python3 scripts/package_npm.py --package root

npm-package-platform target binary_dir:
    python3 scripts/package_npm.py --package platform --target "{{target}}" --binary-dir "{{binary_dir}}"
