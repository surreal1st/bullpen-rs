/// Test that the systemd unit file is correctly configured for deployment.
///
/// This test verifies critical deployment parameters:
/// - The service listens on port 4380
/// - The Docker sandbox is enabled (BULLPEN_SANDBOX=on)
/// - The service runs as the bullpen user (not root)
/// - Security hardening is in place: NoNewPrivileges, ProtectSystem, memory limits, rootless Docker
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

    // Assert the service runs as bullpen user, not root (F11)
    assert!(
        unit_content.contains("User=bullpen"),
        "unit file must have User=bullpen; running as root is a security violation"
    );

    // Assert no-new-privileges hardening is enabled (F11)
    assert!(
        unit_content.contains("NoNewPrivileges=true"),
        "unit file must have NoNewPrivileges=true to prevent privilege escalation"
    );

    // Assert filesystem protection is strict (F11)
    assert!(
        unit_content.contains("ProtectSystem=strict"),
        "unit file must have ProtectSystem=strict to restrict filesystem access"
    );

    // Assert memory limits are in place (F11)
    assert!(
        unit_content.contains("MemoryMax="),
        "unit file must have MemoryMax= to prevent OOM issues"
    );

    // Assert rootless Docker is configured (F11)
    assert!(
        unit_content.contains("DOCKER_HOST=unix:///run/user/1004/docker.sock"),
        "unit file must configure rootless Docker daemon for the bullpen user (uid 1004)"
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
