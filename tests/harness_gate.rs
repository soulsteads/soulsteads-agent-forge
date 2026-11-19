//! The test-only `stdio` adapter must not exist in a default build.
use agent_forge::harness::RunnerProfile;
use serde_json::json;

#[test]
fn stdio_adapter_is_unreachable_unless_the_test_feature_is_enabled() {
    let profile: RunnerProfile =
        serde_json::from_value(json!({"adapter":"stdio","executable":"/bin/sh"})).unwrap();
    assert_eq!(
        profile.validate("fixture").is_ok(),
        cfg!(feature = "test-adapters")
    );
    assert_eq!(
        agent_forge::harness::matrix().any(|a| a.name == "stdio"),
        cfg!(feature = "test-adapters")
    );
    for adapter in ["codex", "claude", "opencode", "soulsteads"] {
        assert!(agent_forge::harness::matrix().any(|a| a.name == adapter));
    }
}
