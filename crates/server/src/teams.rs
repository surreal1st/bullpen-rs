//! S5c-03: Teams, the same shape as Slack, behind a flag. Port of TS
//! `bullpen-night/src/server/teams.ts` (47 lines, read in full).
//!
//! Decision D4 (TS `PLAN-2026-09-13-wave3.md`): "Slack yes, Teams only if
//! work wants it." Nobody has asked yet, so this stays a stub - enough that
//! the shape exists and the flag is real, not enough to spend real effort on
//! an integration nobody is using. `BULLPEN_TEAMS=on` is the gate; with it
//! unset (the default) `POST /api/teams/events` answers 404 without reading
//! this file's `verify_teams_client_state` at all.
//!
//! TODO before this can do anything real: a Microsoft Entra ID app
//! registration in the WORK tenant (rocketcom, not Josh's personal
//! Microsoft account) with `ChannelMessage.Read.All` and `Chat.Read.All`
//! permissions, then a Graph subscription (`POST /subscriptions` with
//! `changeType: created`, a `notificationUrl` pointing at
//! `/api/teams/events`, and a `clientState` secret) renewed before it
//! expires. None of that can happen without a WORK Entra admin granting
//! consent - the same "Josh does the app registration, Bullpen does the
//! code" split Slack's wizard follows, except this one needs a WORK admin.

/// Whether the Teams stub is turned on at all. Mirrors the TS
/// `TEAMS_ENABLED = process.env["BULLPEN_TEAMS"] === "on"`.
pub fn teams_enabled() -> bool {
    std::env::var("BULLPEN_TEAMS")
        .map(|v| v == "on")
        .unwrap_or(false)
}

/// Graph's own validation handshake: when a Teams subscription is first
/// registered, it POSTs with `?validationToken=<token>` and expects that
/// exact token echoed back as `text/plain`, no signature involved - Graph's
/// equivalent of Slack's `url_verification` challenge. Mirrors the TS
/// `teamsValidationToken(query: URLSearchParams)`; the route's own
/// `axum::extract::Query<HashMap<String, String>>` is this crate's
/// `URLSearchParams` equivalent, so this takes the already-parsed map
/// rather than pulling in a query-string parser of its own.
pub fn teams_validation_token(query: &std::collections::HashMap<String, String>) -> Option<String> {
    query.get("validationToken").cloned()
}

/// Graph's `clientState` is a shared secret chosen at subscription time and
/// echoed back on every notification - the closest thing Graph has to a
/// signature for a change notification. Real verification (a subscription
/// lookup by `clientState`, matched in constant time) needs the subscription
/// this file does not yet create; this refuses everything until it does,
/// which is correct for a stub that must not be mistaken for "connected".
/// Mirrors the TS `verifyTeamsClientState`.
pub fn verify_teams_client_state() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_token_reads_the_query_param() {
        let mut query = std::collections::HashMap::new();
        query.insert("validationToken".to_string(), "abc123".to_string());
        assert_eq!(teams_validation_token(&query), Some("abc123".to_string()));
    }

    #[test]
    fn validation_token_none_when_absent() {
        assert_eq!(
            teams_validation_token(&std::collections::HashMap::new()),
            None
        );
        let mut query = std::collections::HashMap::new();
        query.insert("other".to_string(), "1".to_string());
        assert_eq!(teams_validation_token(&query), None);
    }

    #[test]
    fn client_state_never_verifies() {
        assert!(!verify_teams_client_state());
    }
}
