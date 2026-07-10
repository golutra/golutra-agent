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
    cargo test -p golutra-protocol-fixtures schema_smoke -- --nocapture

fixture:
    cargo test -p golutra-protocol-fixtures

smoke:
    cargo test --workspace

replay-smoke:
    cargo test -p golutra-protocol-fixtures

transport-smoke:
    cargo test -p golutra-client

ts-check:
    npm run --prefix sdk/typescript typecheck
