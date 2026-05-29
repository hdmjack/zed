use credentials_provider::CredentialsProvider;
use gpui::AsyncApp;
use std::sync::Arc;

const GITHUB_CREDENTIALS_URL: &str = "https://api.github.com";

/// Resolves a GitHub token using a layered fallback chain:
/// 1. `GITHUB_TOKEN` environment variable
/// 2. System keychain via `CredentialsProvider`
/// 3. `gh auth token` CLI command
pub async fn resolve_github_token(
    credential_provider: Arc<dyn CredentialsProvider>,
    cx: &AsyncApp,
) -> Option<String> {
    //First env var
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        if !token.is_empty() {
            return Some(token);
        }
    }

    //system keychain
    if let Ok(Some((_username, token_bytes))) = credential_provider
        .read_credentials(GITHUB_CREDENTIALS_URL, cx)
        .await
    {
        if let Ok(token) = String::from_utf8(token_bytes) {
            return Some(token);
        }
    }

    //gh cli

    if let Ok(output) = smol::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .await
    {
        if output.status.success() {
            let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !token.is_empty() {
                return Some(token);
            }
        }
    }

    None
}
