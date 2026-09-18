//! The credential this bridge sends upstream, and how to read it.
//!
//! Everything here is pure, so it runs on the host target: the `get-secret`
//! call itself lives in `lib.rs`, where the generated bindings are.
//!
//! # Two field names, and why the choice is by name
//!
//! An upstream MCP server is authenticated with a bearer token, and that token
//! reaches this component in one of two forms — a string somebody pasted, or
//! an access token an OAuth flow acquired and stored with its expiry. Both end
//! up in the same `Authorization: Bearer …` header, but they are not the same
//! credential: one has an expiry that must be honoured, the other has none.
//!
//! `ACT-CONSTANTS.md` §8.1 makes the *type* a property of the **field**, and
//! §8.2 forbids inferring a credential's meaning from its shape. So the two
//! forms are two field names, and which one is stored decides which path runs:
//!
//! | Field | Type | Stored with |
//! |-------|------|-------------|
//! | [`FIELD_TOKEN`] | `std:string` | `act secret set … --field mcp:token --fields-stdin` |
//! | [`FIELD_OAUTH`] | `std:oauth2` | `act secret set … --field mcp:oauth=std:oauth2 --fields-stdin`, or `act login` |
//!
//! Nothing here looks at whether a value happens to be a map. `mcp:oauth`
//! holding a bare string is a provisioning mistake and is reported as one,
//! rather than quietly re-read as a plain token — the two differ in exactly
//! the thing that would then be lost, which is the expiry.
//!
//! Both names are in this component's own namespace. No field name is
//! well-known (§8.2 registers types, not names) and a `std:`-prefixed one is
//! refused at pack time, so the party that reads a field is the party that
//! prints the command its operator copies. These two are printed in every
//! error below.

use std::fmt;

use act_sdk::credentials::Secret;

use crate::mcp_client::McpError;

/// A plain bearer token, stored as a `std:string` field.
pub const FIELD_TOKEN: &str = "mcp:token";
/// An OAuth 2.0 credential, stored as a `std:oauth2` field — a map whose
/// members are registered in `ACT-CONSTANTS.md` §8.3.
pub const FIELD_OAUTH: &str = "mcp:oauth";

/// The longest a credential key may be. A lookup name, not a sentence: the
/// host copies it verbatim into the line a **human** reads when deciding to
/// release a credential, so it must not be able to become a paragraph.
pub const MAX_CREDENTIAL_KEY: usize = 64;

/// The `Authorization` header value, kept apart from every other string in
/// the component so that it can be given a `Debug` that does not print it.
#[derive(Clone, PartialEq, Eq)]
pub struct Bearer(String);

impl Bearer {
    /// The full header value, `Bearer <token>`.
    pub fn header_value(&self) -> String {
        format!("Bearer {}", self.0)
    }
}

/// Never prints the token. `Debug` is where credential material escapes by
/// accident — a `dbg!` while debugging, or an error type that derives `Debug`
/// around a field of this type.
impl fmt::Debug for Bearer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Bearer(<redacted>)")
    }
}

/// The leading credential-key-shaped run of `key`: letters, digits, `-`, `_`
/// and `.`, up to [`MAX_CREDENTIAL_KEY`] characters.
///
/// Returns the *prefix* so that a refusal can name what was asked for without
/// repeating whatever followed it.
fn key_prefix(key: &str) -> &str {
    let end = key
        .as_bytes()
        .iter()
        .take(MAX_CREDENTIAL_KEY)
        .position(|b| !(b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')))
        .unwrap_or_else(|| key.len().min(MAX_CREDENTIAL_KEY));
    &key[..end]
}

/// Check `credential_key` before it leaves this component.
///
/// The key is agent-authored text, and the host pastes it **raw** into the
/// question a human answers when releasing a credential (act-cli's
/// `consent_summary` sanitises `hint` and nothing else, on the reasoning that
/// everything else is host-derived — for a session argument it is not). The
/// hazard is not newline forgery, which the prompter escapes; it is plainer:
/// `prod (approved by your administrator)` renders in that question exactly as
/// written and the human cannot tell which half a machine wrote. A key is a
/// lookup name, so a bounded token costs nothing and ends the question.
pub fn validate_credential_key(key: &str) -> Result<(), McpError> {
    let head = key_prefix(key);
    if head.is_empty() || head.len() != key.len() {
        return Err(McpError::invalid_args(format!(
            "credential_key must be a name: 1–{MAX_CREDENTIAL_KEY} characters of letters, \
             digits, '-', '_' or '.', and '{head}…' is not one. It names an entry in this \
             component's credential profile, not a sentence."
        )));
    }
    Ok(())
}

/// Turn a fetched credential into the header value the bridge sends.
///
/// `now_unix` is Unix seconds, or `0` when the host could not be asked for a
/// clock — `0` disables the expiry check rather than declaring every token
/// expired, because a component that cannot read the time knows nothing about
/// expiry either way.
pub fn bearer_from_secret(secret: &Secret, key: &str, now_unix: u64) -> Result<Bearer, McpError> {
    let has_oauth = secret.field(FIELD_OAUTH).is_some();
    let has_token = secret.field(FIELD_TOKEN).is_some();

    match (has_oauth, has_token) {
        // Two credentials in one record. Refused rather than resolved by
        // precedence: picking one silently would authenticate with material
        // the operator may have meant to replace, and the expiry rules differ
        // between them, so "which one is live" is not a detail.
        (true, true) => Err(credential_required(format!(
            "The credential under key '{key}' carries both {FIELD_OAUTH} and {FIELD_TOKEN}. \
             Store exactly one: {FIELD_OAUTH} for an OAuth access token (expiry honoured), \
             {FIELD_TOKEN} for a bearer token pasted by hand."
        ))),
        (true, false) => oauth_bearer(secret, key, now_unix),
        (false, true) => plain_bearer(secret, key),
        (false, false) => Err(credential_required(format!(
            "The credential under key '{key}' carries neither {FIELD_OAUTH} nor {FIELD_TOKEN}, \
             and those are the only two fields this bridge reads.\n{}",
            provisioning_commands(key)
        ))),
    }
}

/// The `std:string` path: the field's value *is* the token, read by name.
fn plain_bearer(secret: &Secret, key: &str) -> Result<Bearer, McpError> {
    match secret.field_str(FIELD_TOKEN) {
        Some(token) if !token.trim().is_empty() => Ok(Bearer(token.to_string())),
        // Present but blank, or present and not a CBOR string at all. Both
        // are provisioning mistakes that would otherwise surface as a 401
        // from the upstream, which points at the wrong thing.
        _ => Err(credential_required(format!(
            "The {FIELD_TOKEN} field under key '{key}' is empty or is not a string. \
             Store it with:\n  act secret set <component-ref> --key {key} \
             --field {FIELD_TOKEN} --fields-stdin\n  {{\"{FIELD_TOKEN}\": \"<token>\"}}"
        ))),
    }
}

/// The `std:oauth2` path: the field's value is a map, and its `std:access-token`
/// member is the bearer.
///
/// `Secret::as_oauth2` is given the **field name**, not asked to go looking for
/// a map that resembles one — the same rule that makes this component decide
/// between its two credentials by name.
fn oauth_bearer(secret: &Secret, key: &str, now_unix: u64) -> Result<Bearer, McpError> {
    let Some(oauth) = secret.as_oauth2(FIELD_OAUTH) else {
        return Err(credential_required(format!(
            "The {FIELD_OAUTH} field under key '{key}' is not a readable OAuth credential: \
             it must be a std:oauth2 map with a std:access-token string \
             (ACT-CONSTANTS.md §8.3). A token pasted as a plain string belongs in \
             {FIELD_TOKEN} instead.\n{}",
            provisioning_commands(key)
        )));
    };

    // Honoured here rather than left to the upstream, because a 401 from the
    // upstream reads as "wrong token" and sends the operator to re-check the
    // value rather than to re-acquire it.
    if let Some(expires_at) = oauth.expires_at
        && now_unix != 0
        && now_unix >= expires_at
    {
        return Err(credential_required(format!(
            "The OAuth access token in {FIELD_OAUTH} under key '{key}' expired at {expires_at} \
             (Unix seconds; it is now {now_unix}). ACT does not refresh a stored token — \
             silent refresh is out of scope for the host (ACT-AUTH §1.1) — so re-acquire it \
             with:\n  act login <component-ref> --key {key} --field {FIELD_OAUTH} --force\n\
             then open a new session."
        )));
    }

    if oauth.access_token.trim().is_empty() {
        return Err(credential_required(format!(
            "The OAuth access token in {FIELD_OAUTH} under key '{key}' is empty."
        )));
    }
    Ok(Bearer(oauth.access_token))
}

/// The two commands that provision either field, printed together because a
/// missing credential does not say which of the two the operator meant.
fn provisioning_commands(key: &str) -> String {
    format!(
        "Store a bearer token with:\n  act secret set <component-ref> --key {key} \
         --field {FIELD_TOKEN} --fields-stdin\n  {{\"{FIELD_TOKEN}\": \"<token>\"}}\n\
         or an OAuth credential with:\n  act secret set <component-ref> --key {key} \
         --field {FIELD_OAUTH}=std:oauth2 --fields-stdin\n  \
         {{\"{FIELD_OAUTH}\": {{\"std:access-token\": \"<token>\"}}}}"
    )
}

/// `std:credential-required` (`ACT-CONSTANTS.md` §9): the call was fine, the
/// credential was not, and the fix is a command the operator runs.
pub fn credential_required(message: String) -> McpError {
    McpError::credential_required(message)
}

/// What a missing or refused credential looks like to the agent.
///
/// `not-found` and `denied` collapse into this one message deliberately: the
/// host decides `denied` **before** it consults the store (ACT-AUTH §1.1.7),
/// so telling them apart here would invent a difference the host refuses to
/// disclose — and would turn the pair into a way to probe a profile for keys.
pub fn credential_missing(key: &str, known: &[String]) -> McpError {
    let mut message = format!(
        "No usable credential under key '{key}'. Either it is not set, or policy denies \
         act:credentials for this component (grant it with `--allow act:credentials`).\n{}",
        provisioning_commands(key)
    );
    if !known.is_empty() {
        message.push_str(&format!(
            "\nThis component's profile has: {}",
            known.join(", ")
        ));
    }
    credential_required(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ciborium::Value;

    fn secret(pairs: Vec<(&str, Value)>) -> Secret {
        Secret {
            kind: "std:fields".into(),
            fields: pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        }
    }

    fn oauth_map(members: Vec<(&str, Value)>) -> Value {
        Value::Map(
            members
                .into_iter()
                .map(|(k, v)| (Value::Text(k.into()), v))
                .collect(),
        )
    }

    fn text(s: &str) -> Value {
        Value::Text(s.into())
    }

    // ── field-name dispatch ────────────────────────────────────────────

    #[test]
    fn a_plain_token_field_becomes_a_bearer_header() {
        let b = bearer_from_secret(&secret(vec![(FIELD_TOKEN, text("sk-1"))]), "default", 0)
            .expect("readable");
        assert_eq!(b.header_value(), "Bearer sk-1");
    }

    #[test]
    fn an_oauth_field_contributes_its_access_token() {
        let s = secret(vec![(
            FIELD_OAUTH,
            oauth_map(vec![("std:access-token", text("at"))]),
        )]);
        let b = bearer_from_secret(&s, "default", 1_760_000_000).expect("readable");
        assert_eq!(b.header_value(), "Bearer at");
    }

    /// The rule that makes this component conform to `ACT-CONSTANTS.md` §8.2:
    /// the decision is the field **name**, never the value's shape. A map
    /// stored under the plain-token name is not quietly unwrapped, and a
    /// string stored under the OAuth name is not quietly accepted — the two
    /// differ in the expiry, which is the thing that would be lost.
    #[test]
    fn the_choice_is_the_field_name_not_the_value_shape() {
        let map_under_the_string_name = secret(vec![(
            FIELD_TOKEN,
            oauth_map(vec![("std:access-token", text("at"))]),
        )]);
        assert!(bearer_from_secret(&map_under_the_string_name, "default", 0).is_err());

        let string_under_the_oauth_name = secret(vec![(FIELD_OAUTH, text("sk-1"))]);
        let e = bearer_from_secret(&string_under_the_oauth_name, "default", 0)
            .expect_err("a bare string is not a std:oauth2 value");
        assert!(e.message.contains(FIELD_TOKEN), "{}", e.message);
    }

    #[test]
    fn carrying_both_fields_is_refused_rather_than_resolved_by_precedence() {
        let s = secret(vec![
            (FIELD_TOKEN, text("sk-1")),
            (
                FIELD_OAUTH,
                oauth_map(vec![("std:access-token", text("at"))]),
            ),
        ]);
        let e = bearer_from_secret(&s, "default", 0).expect_err("ambiguous");
        assert_eq!(e.kind, "std:credential-required");
        assert!(e.message.contains(FIELD_OAUTH) && e.message.contains(FIELD_TOKEN));
    }

    #[test]
    fn a_credential_with_neither_field_names_both_commands() {
        let s = secret(vec![("acme:tenant", text("t-42"))]);
        let e = bearer_from_secret(&s, "prod", 0).expect_err("nothing to read");
        assert_eq!(e.kind, "std:credential-required");
        assert!(e.message.contains("act secret set"), "{}", e.message);
        assert!(e.message.contains("--key prod"), "{}", e.message);
        assert!(e.message.contains("std:oauth2"), "{}", e.message);
    }

    #[test]
    fn a_blank_token_is_refused_before_the_upstream_sees_it() {
        let e = bearer_from_secret(&secret(vec![(FIELD_TOKEN, text("   "))]), "default", 0)
            .expect_err("blank");
        assert_eq!(e.kind, "std:credential-required");
    }

    // ── OAuth expiry ───────────────────────────────────────────────────

    #[test]
    fn an_expired_oauth_token_is_refused_and_says_how_to_re_acquire_it() {
        let s = secret(vec![(
            FIELD_OAUTH,
            oauth_map(vec![
                ("std:access-token", text("at")),
                ("std:expires-at", Value::Integer(1_000u64.into())),
            ]),
        )]);
        let e = bearer_from_secret(&s, "prod", 1_001).expect_err("expired");
        assert_eq!(e.kind, "std:credential-required");
        assert!(e.message.contains("act login"), "{}", e.message);
        // The host does not refresh; promising otherwise sends the operator
        // to wait for something that never happens.
        assert!(
            !e.message.to_lowercase().contains("refreshing")
                && !e.message.to_lowercase().contains("will be refreshed"),
            "{}",
            e.message
        );
    }

    #[test]
    fn a_token_expiring_exactly_now_is_already_expired() {
        let s = secret(vec![(
            FIELD_OAUTH,
            oauth_map(vec![
                ("std:access-token", text("at")),
                ("std:expires-at", Value::Integer(1_000u64.into())),
            ]),
        )]);
        assert!(bearer_from_secret(&s, "prod", 1_000).is_err());
        assert!(bearer_from_secret(&s, "prod", 999).is_ok());
    }

    /// `0` means "the host could not be asked what time it is", and a
    /// component that cannot read a clock knows nothing about expiry. It must
    /// not therefore declare every token dead.
    #[test]
    fn an_unknown_clock_does_not_expire_anything() {
        let s = secret(vec![(
            FIELD_OAUTH,
            oauth_map(vec![
                ("std:access-token", text("at")),
                ("std:expires-at", Value::Integer(1_000u64.into())),
            ]),
        )]);
        assert!(bearer_from_secret(&s, "prod", 0).is_ok());
    }

    /// A `std:oauth2` map with no `std:expires-at` reads as "no known
    /// expiry" (`ACT-CONSTANTS.md` §8.3), not as "expired".
    #[test]
    fn an_oauth_token_without_an_expiry_is_usable() {
        let s = secret(vec![(
            FIELD_OAUTH,
            oauth_map(vec![("std:access-token", text("at"))]),
        )]);
        assert!(bearer_from_secret(&s, "prod", 9_999_999_999).is_ok());
    }

    // ── nothing leaks ──────────────────────────────────────────────────

    #[test]
    fn no_error_ever_quotes_the_material() {
        let sentinel = "hunter2-sentinel";
        let cases = vec![
            secret(vec![(FIELD_OAUTH, text(sentinel))]),
            secret(vec![
                (FIELD_TOKEN, text(sentinel)),
                (
                    FIELD_OAUTH,
                    oauth_map(vec![("std:access-token", text(sentinel))]),
                ),
            ]),
            secret(vec![(
                FIELD_OAUTH,
                oauth_map(vec![
                    ("std:access-token", text(sentinel)),
                    ("std:expires-at", Value::Integer(1u64.into())),
                ]),
            )]),
        ];
        for s in cases {
            let e = bearer_from_secret(&s, "prod", 1_000).expect_err("refused");
            assert!(!e.message.contains(sentinel), "leaked: {}", e.message);
        }
    }

    #[test]
    fn the_bearer_debug_prints_no_token() {
        let b = bearer_from_secret(
            &secret(vec![(FIELD_TOKEN, text("hunter2-sentinel"))]),
            "d",
            0,
        )
        .expect("readable");
        assert_eq!(format!("{b:?}"), "Bearer(<redacted>)");
    }

    // ── credential_key ─────────────────────────────────────────────────

    #[test]
    fn a_credential_key_is_a_bounded_token() {
        for key in [
            "default",
            "prod",
            "a.b-c_1",
            &"k".repeat(MAX_CREDENTIAL_KEY),
        ] {
            assert!(validate_credential_key(key).is_ok(), "{key}");
        }
    }

    /// The refusal exists because the host pastes this string into a line a
    /// human reads while deciding to release a credential.
    #[test]
    fn a_credential_key_that_is_a_sentence_is_refused_and_only_its_prefix_echoed() {
        let e =
            validate_credential_key("prod (approved by your administrator)").expect_err("refused");
        assert_eq!(e.kind, "std:invalid-args");
        assert!(e.message.contains("'prod…'"), "{}", e.message);
        assert!(!e.message.contains("administrator"), "{}", e.message);
    }

    #[test]
    fn an_empty_or_overlong_credential_key_is_refused() {
        assert!(validate_credential_key("").is_err());
        assert!(validate_credential_key(&"k".repeat(MAX_CREDENTIAL_KEY + 1)).is_err());
        assert!(validate_credential_key("bad key").is_err());
        assert!(validate_credential_key("\nBearer: x").is_err());
    }

    #[test]
    fn the_missing_credential_message_lists_the_profiles_keys_when_it_has_them() {
        let e = credential_missing("prod", &["staging".into(), "dev".into()]);
        assert_eq!(e.kind, "std:credential-required");
        assert!(e.message.contains("staging, dev"), "{}", e.message);
        // Absent and denied read identically — the host will not say which.
        assert!(e.message.contains("not set"), "{}", e.message);
        assert!(
            e.message.contains("--allow act:credentials"),
            "{}",
            e.message
        );
    }
}
