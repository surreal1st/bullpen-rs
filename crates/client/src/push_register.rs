//! S13-02: register this device for APNs after sign-in.
//!
//! Until S13-03 lands the Swift `PushRegistrar` bridge, tests and simulators
//! can inject a token via `BULLPEN_PUSH_TOKEN` (hex). Never log the token.

use crate::api;
use crate::transport;

/// Called once the gate confirms a session (`App` → `Open`).
pub fn after_sign_in() {
    let Some(token) = std::env::var("BULLPEN_PUSH_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty())
    else {
        return;
    };
    let environment = std::env::var("BULLPEN_PUSH_ENV").unwrap_or_else(|_| "production".into());
    transport::spawn_task(async move {
        let _ = api::register_push_device(&token, &environment).await;
    });
}
