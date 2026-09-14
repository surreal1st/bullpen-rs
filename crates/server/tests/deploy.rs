/// Test that the systemd unit file is correctly configured for deployment.
///
/// This test verifies critical deployment parameters:
/// - The service listens on port 4380
/// - The Docker sandbox is enabled (BULLPEN_SANDBOX=on)
/// - The service does not expose network to containers (no --network outside of strict Docker)
///
/// These assertions guard against configuration drift that could break production.
#[test]
fn test_deploy_unit_file_config() {
    use std::fs;
    use std::path::PathBuf;

    // Read the unit file from the deploy directory
    let unit_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("manifest dir has parent")
        .parent()
        .expect("parent has parent")
        .join("deploy")
        .join("bullpen-rs.service");

    let unit_content = fs::read_to_string(&unit_path).expect("failed to read unit file");

    // Assert port 4380 is configured
    assert!(
        unit_content.contains("BULLPEN_PORT=4380"),
        "unit file must configure BULLPEN_PORT=4380"
    );

    // Assert sandbox is enabled
    assert!(
        unit_content.contains("BULLPEN_SANDBOX=on"),
        "unit file must have BULLPEN_SANDBOX=on to enable the Docker sandbox"
    );

    // Assert that there is no --network directive in environment (Docker networking
    // is controlled via the sandbox module, not the unit file).
    // This catches accidental exposure of the Docker network to containers.
    let environment_section = unit_content
        .split("[Install]")
        .next()
        .expect("should have [Install] section");

    assert!(
        !environment_section.contains("--network"),
        "unit file must not contain --network in [Service] environment; network isolation is enforced by the sandbox"
    );

    // Verify it's a valid systemd service unit (basic structure check)
    assert!(
        unit_content.contains("[Unit]"),
        "unit file must have [Unit] section"
    );
    assert!(
        unit_content.contains("[Service]"),
        "unit file must have [Service] section"
    );
    assert!(
        unit_content.contains("[Install]"),
        "unit file must have [Install] section"
    );
}
