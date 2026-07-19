use super::*;

#[test]
fn viewport_and_input_limits_are_enforced() {
    assert!(validate_dimensions(40, 8).is_ok());
    assert!(validate_dimensions(320, 200).is_ok());
    assert!(validate_dimensions(39, 8).is_err());
    assert!(validate_dimensions(320, 201).is_err());

    assert!(validate_input(&"a".repeat(MAX_DRIVER_INPUT_BYTES)).is_ok());
    assert!(validate_input(&"a".repeat(MAX_DRIVER_INPUT_BYTES + 1)).is_err());
    assert!(validate_input("contains\0nul").is_err());
}
