//! Host-only validation of Codex workspace personal access tokens.
//!
//! Mirrors Codex's PAT metadata lookup. No OAuth cache, refresh token, or
//! agent-identity signing key is copied into the guest or handled here.

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::cmd::Cmd;

const WHOAMI_URL: &str = "https://auth.openai.com/api/accounts/v1/user-auth-credential/whoami";

/// Only the routing metadata needed by inference, deliberately excluding
/// the user's email, user ID, and plan name returned by the service.
#[derive(Debug, Deserialize)]
pub(crate) struct CodexTokenMetadata {
    pub(crate) chatgpt_account_id: String,
    pub(crate) chatgpt_account_is_fedramp: bool,
}

pub(crate) fn validate_token(token: &str) -> Result<()> {
    if !token.starts_with("at-")
        || token.len() <= 3
        || !token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b))
    {
        bail!(
            "Expected a Codex personal access token (at-...) from ChatGPT Admin > Access tokens; \
             API keys, browser tokens, and agent-identity JWTs are not supported by this proxy mode"
        );
    }
    Ok(())
}

/// Validate on every proxy start so revoked/expired credentials fail closed.
/// curl receives the credential through stdin, never argv. Its user config,
/// redirects, diagnostics, and response bodies on HTTP errors are disabled.
pub(crate) fn load_metadata(token: &str) -> Result<CodexTokenMetadata> {
    load_metadata_with_command(token, metadata_command())
}

fn load_metadata_with_command(token: &str, command: Cmd) -> Result<CodexTokenMetadata> {
    validate_token(token)?;
    let body = command
        .stdin_input(format!("Authorization: Bearer {token}\n"))
        .capture()
        .context(
            "Failed to validate the Codex access token; check expiry, revocation, \
             workspace permissions, and HTTPS access to auth.openai.com",
        )?;
    parse_metadata(&body)
}

fn metadata_command() -> Cmd {
    Cmd::new("curl").args([
        "--disable",
        "--silent",
        "--fail",
        "--proto",
        "=https",
        "--connect-timeout",
        "10",
        "--max-time",
        "20",
        "--max-filesize",
        "65536",
        "--header",
        "@-",
        "--url",
        WHOAMI_URL,
    ])
}

fn parse_metadata(body: &str) -> Result<CodexTokenMetadata> {
    // Avoid serde's input-dependent diagnostics: the service's raw response
    // must never appear in errors or logs.
    let metadata: CodexTokenMetadata = serde_json::from_str(body)
        .map_err(|_| anyhow::anyhow!("Invalid Codex access-token metadata response"))?;
    if metadata.chatgpt_account_id.is_empty()
        || metadata.chatgpt_account_id.len() > 256
        || !metadata
            .chatgpt_account_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    {
        bail!("Invalid workspace ID in Codex access-token metadata");
    }
    Ok(metadata)
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    #[test]
    fn metadata_lookup_sends_bearer_only_through_stdin_and_fails_closed() {
        use sha2::Digest as _;

        let expected = hex::encode(sha2::Sha256::digest(b"Authorization: Bearer at-private\n"));
        let script = format!(
            "import hashlib,json,sys; assert hashlib.sha256(sys.stdin.buffer.read()).hexdigest() == '{expected}'; print(json.dumps({{'chatgpt_account_id':'workspace-123','chatgpt_account_is_fedramp':False}}))"
        );
        let command = Cmd::new("python3").args(["-c", &script]);
        let metadata = load_metadata_with_command("at-private", command).unwrap();
        assert_eq!(metadata.chatgpt_account_id, "workspace-123");
        assert!(!metadata.chatgpt_account_is_fedramp);
        let failed =
            Cmd::new("python3").args(["-c", "import sys; print(sys.stdin.read()); sys.exit(22)"]);
        let error = load_metadata_with_command("at-private", failed).unwrap_err();
        assert!(error.to_string().contains("Failed to validate"));
        assert!(!format!("{error:#}").contains("at-private"));
        let malformed = Cmd::new("python3").args(["-c", "import sys; print(sys.stdin.read())"]);
        assert!(
            !format!(
                "{:#}",
                load_metadata_with_command("at-private", malformed).unwrap_err()
            )
            .contains("at-private")
        );
    }

    #[test]
    fn malformed_tokens_are_rejected_before_the_metadata_command_runs() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("launched");
        let command = Cmd::new("python3")
            .args([
                "-c",
                "import pathlib,sys; pathlib.Path(sys.argv[1]).touch()",
            ])
            .arg(&marker);
        assert!(load_metadata_with_command("sk-wrong-type", command).is_err());
        assert!(!marker.exists());
    }

    #[test]
    fn only_personal_access_tokens_are_accepted_without_header_injection() {
        assert!(validate_token("at-example_123-abc").is_ok());
        for invalid in [
            "",
            "at-",
            "sk-example",
            "header.payload.signature",
            "at-x\nX: y",
            "at- x",
            "at-\0",
            "at-é",
        ] {
            assert!(validate_token(invalid).is_err());
        }
        assert!(
            !validate_token("at-secret\nX: y")
                .unwrap_err()
                .to_string()
                .contains("secret")
        );
    }

    #[test]
    fn metadata_retains_only_workspace_routing_fields() {
        let m = parse_metadata(r#"{"chatgpt_account_id":"account-123","chatgpt_account_is_fedramp":true,"email":"private@example.com","chatgpt_user_id":"private-user","chatgpt_plan_type":"business"}"#).unwrap();
        assert_eq!(m.chatgpt_account_id, "account-123");
        assert!(m.chatgpt_account_is_fedramp);
        let debug = format!("{m:?}");
        assert!(!debug.contains("private"));
    }

    #[test]
    fn invalid_metadata_never_echoes_the_response() {
        for body in [
            r#"{"chatgpt_account_id":"","chatgpt_account_is_fedramp":false}"#,
            r#"{"chatgpt_account_id":"account\r\nX-Injected: secret","chatgpt_account_is_fedramp":false}"#,
            r#"{"chatgpt_account_id":"account"}"#,
            r#"{"chatgpt_account_id":"account","chatgpt_account_is_fedramp":"secret"}"#,
            "secret invalid JSON",
        ] {
            assert!(
                !parse_metadata(body)
                    .unwrap_err()
                    .to_string()
                    .contains("secret")
            );
        }
    }

    #[test]
    fn workspace_id_length_boundary() {
        for (len, accepted) in [(256, true), (257, false)] {
            let body = format!(
                r#"{{"chatgpt_account_id":"{}","chatgpt_account_is_fedramp":false}}"#,
                "a".repeat(len)
            );
            assert_eq!(parse_metadata(&body).is_ok(), accepted, "length {len}");
        }
    }

    #[test]
    fn metadata_request_is_fixed_bounded_and_does_not_follow_redirects() {
        let command = metadata_command().build();
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect();
        assert_eq!(args[0], "--disable");
        assert!(args.contains(&"--fail"));
        assert!(args.windows(2).any(|p| p == ["--url", WHOAMI_URL]));
        assert!(args.windows(2).any(|p| p == ["--header", "@-"]));
        assert!(args.windows(2).any(|p| p == ["--proto", "=https"]));
        assert!(args.windows(2).any(|p| p == ["--max-time", "20"]));
        assert!(args.windows(2).any(|p| p == ["--max-filesize", "65536"]));
        assert!(
            !args
                .iter()
                .any(|a| a.contains("location") || a.contains("Bearer"))
        );
    }
}
