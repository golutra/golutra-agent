fn main() -> std::io::Result<()> {
    let output = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "schemas/sdk-protocol.schema.json".to_owned());
    golutra_protocol_fixtures::export_sdk_schema(output)
}
