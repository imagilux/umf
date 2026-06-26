//! Unit tests for the `auth` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

#[test]
fn override_short_circuits_other_layers() {
    let ov = CredentialOverride {
        username: Some("u".to_string()),
        password: Some("p".to_string()),
    };
    match resolve_auth_for(Some("any.example.com"), &ov) {
        RegistryAuth::Basic(u, p) => {
            assert_eq!(u, "u");
            assert_eq!(p, "p");
        }
        other => panic!("expected Basic, got {other:?}"),
    }
}

#[test]
fn override_with_only_username_falls_through() {
    let ov = CredentialOverride {
        username: Some("u".to_string()),
        password: None,
    };
    assert!(ov.as_basic().is_none());
}

#[test]
fn base64_decodes_simple_creds() {
    // user:pass
    let decoded = base64_decode("dXNlcjpwYXNz").unwrap();
    assert_eq!(decoded, b"user:pass");
}

#[test]
fn base64_decodes_with_padding() {
    // foo:bar
    let decoded = base64_decode("Zm9vOmJhcg==").unwrap();
    assert_eq!(decoded, b"foo:bar");
}

#[test]
fn base64_rejects_invalid_char() {
    assert!(base64_decode("inv*lid==").is_none());
}

#[test]
fn parse_b64_auth_round_trips() {
    let auth = parse_b64_auth("dXNlcjpwYXNz").unwrap();
    match auth {
        RegistryAuth::Basic(u, p) => {
            assert_eq!(u, "user");
            assert_eq!(p, "pass");
        }
        other => panic!("expected Basic, got {other:?}"),
    }
}

#[test]
fn parse_b64_auth_rejects_empty_user() {
    assert!(parse_b64_auth("OnBhc3M=").is_none()); // ":pass"
}

#[test]
fn docker_hub_aliases_match() {
    assert!(is_docker_hub("docker.io"));
    assert!(is_docker_hub("registry-1.docker.io"));
    assert!(is_docker_hub("index.docker.io"));
    assert!(!is_docker_hub("ghcr.io"));
}

#[test]
fn config_file_auth_reads_known_host() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.json");
    let body = serde_json::json!({
        "auths": {
            "registry.example.com": {
                "auth": "dXNlcjpwYXNz"  // user:pass
            }
        }
    });
    fs::write(&path, body.to_string()).unwrap();
    match config_file_auth_at(Some(path), Some("registry.example.com")) {
        Some(RegistryAuth::Basic(u, p)) => {
            assert_eq!(u, "user");
            assert_eq!(p, "pass");
        }
        other => panic!("expected Basic, got {other:?}"),
    }
}

#[test]
fn config_file_auth_missing_host_returns_none() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.json");
    fs::write(&path, r#"{"auths": {}}"#).unwrap();
    assert!(config_file_auth_at(Some(path), Some("other.example.com")).is_none());
}

#[test]
fn config_file_auth_reads_username_password_fields() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.json");
    let body = serde_json::json!({
        "auths": {
            "registry.example.com": {
                "username": "alice",
                "password": "secret"
            }
        }
    });
    fs::write(&path, body.to_string()).unwrap();
    match config_file_auth_at(Some(path), Some("registry.example.com")) {
        Some(RegistryAuth::Basic(u, p)) => {
            assert_eq!(u, "alice");
            assert_eq!(p, "secret");
        }
        other => panic!("expected Basic, got {other:?}"),
    }
}

#[test]
fn config_file_auth_resolves_via_https_alias() {
    // Docker historically writes `https://registry/v1/` instead of the
    // bare host.
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.json");
    let body = serde_json::json!({
        "auths": {
            "https://registry.example.com/v1/": {
                "auth": "dXNlcjpwYXNz"
            }
        }
    });
    fs::write(&path, body.to_string()).unwrap();
    assert!(config_file_auth_at(Some(path), Some("registry.example.com")).is_some());
}

#[test]
fn config_file_auth_degrades_when_the_store_helper_is_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.json");
    let body = serde_json::json!({
        "credsStore": "umf-test-definitely-not-installed",
        "auths": {}
    });
    fs::write(&path, body.to_string()).unwrap();
    // A configured store whose helper binary is missing degrades to
    // anonymous (None at this layer) instead of erroring the resolve.
    assert!(config_file_auth_at(Some(path), Some("registry.example.com")).is_none());
}

#[test]
fn config_file_auth_helper_outranks_inline_and_degrades_on_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.json");
    // Per-host helper configured (binary absent) *and* an inline entry:
    // the helper is authoritative, so its failure must NOT fall back to
    // the stale inline secret.
    let body = serde_json::json!({
        "credHelpers": { "registry.example.com": "umf-test-definitely-not-installed" },
        "auths": { "registry.example.com": { "auth": "dXNlcjpwYXNz" } }
    });
    fs::write(&path, body.to_string()).unwrap();
    assert!(config_file_auth_at(Some(path), Some("registry.example.com")).is_none());
}

#[test]
fn pick_auth_key_prefers_https_form() {
    let config: AuthConfigFile =
        serde_json::from_str(r#"{"auths":{"https://registry.example.com":{"auth":"x"}}}"#).unwrap();
    // The host bare form is tried first; when it doesn't match, the
    // `https://` form is tried second.
    assert_eq!(
        pick_auth_key(&config, Some("registry.example.com")),
        Some("https://registry.example.com".to_string())
    );
}

#[test]
fn credential_override_debug_redacts_password() {
    let creds = CredentialOverride {
        username: Some("alice".to_string()),
        password: Some("super-secret-token".to_string()),
    };
    let rendered = format!("{creds:?}");
    assert!(
        !rendered.contains("super-secret-token"),
        "password value must never appear in Debug output: {rendered}"
    );
    assert!(
        rendered.contains("redacted"),
        "should mark the password redacted"
    );
    assert!(
        rendered.contains("alice"),
        "username is not secret and should remain for diagnostics"
    );
}

// ── Credential-helper protocol ──────────────────────────────────────────────
//
// The reply evaluation is a pure function (`helper_reply_auth`), so every
// acceptance/rejection branch is tested in-process — no fixture scripts, no
// shell. Process plumbing is covered by one `cat` pipe smoke test (a real
// exec, still no shell) plus the absent-binary degrade tests above.

#[test]
fn helper_names_are_validated() {
    for good in [
        "desktop",
        "ecr-login",
        "osxkeychain",
        "wincred.v2",
        "pass_1",
    ] {
        assert!(helper_name_is_valid(good), "{good:?} must be accepted");
    }
    for bad in ["", "../evil", "a/b", "a b", "ab\u{0}c", "café"] {
        assert!(!helper_name_is_valid(bad), "{bad:?} must be rejected");
    }
}

#[test]
fn helper_reply_maps_to_basic_auth() {
    let reply =
        br#"{"ServerURL":"registry.example.com","Username":"helper-user","Secret":"helper-pass"}"#;
    match helper_reply_auth(true, "registry.example.com", reply) {
        Some(RegistryAuth::Basic(u, p)) => {
            assert_eq!(u, "helper-user");
            assert_eq!(p, "helper-pass");
        }
        other => panic!("expected Basic, got {other:?}"),
    }
}

#[test]
fn helper_token_username_maps_to_bearer() {
    let reply = br#"{"Username":"<token>","Secret":"id-token"}"#;
    match helper_reply_auth(true, "registry.example.com", reply) {
        Some(RegistryAuth::Bearer(t)) => assert_eq!(t, "id-token"),
        other => panic!("expected Bearer, got {other:?}"),
    }
}

#[test]
fn helper_miss_and_garbage_degrade_to_none() {
    // Exit != 0 — the standard "credentials not found" miss; the reply
    // text is irrelevant.
    assert!(helper_reply_auth(false, "h", b"credentials not found in native keychain").is_none());

    // Exit 0 but a non-JSON reply.
    assert!(helper_reply_auth(true, "h", b"not-json").is_none());

    // Exit 0 but empty username/secret.
    assert!(helper_reply_auth(true, "h", br#"{"Username":"","Secret":""}"#).is_none());
    assert!(helper_reply_auth(true, "h", br#"{"Username":"u","Secret":""}"#).is_none());
}

#[test]
fn helper_oversized_reply_is_rejected() {
    // One byte past the cap — even valid JSON must be refused.
    let mut reply = br#"{"Username":"u","Secret":"s","Pad":""#.to_vec();
    reply.resize(MAX_HELPER_REPLY_BYTES as usize, b'a');
    reply.extend_from_slice(br#""}"#);
    assert!(reply.len() as u64 > MAX_HELPER_REPLY_BYTES);
    assert!(helper_reply_auth(true, "h", &reply).is_none());
}

/// One real exec to cover the pipe lifecycle (spawn → stdin write + close →
/// bounded stdout read → wait): `cat` echoes the host back, which is not
/// JSON, so the outcome is a deterministic `None`. No shell involved.
#[cfg(unix)]
#[test]
fn helper_pipe_plumbing_round_trips_through_cat() {
    assert!(helper_auth_via(Command::new("cat"), "registry.example.com").is_none());
}
