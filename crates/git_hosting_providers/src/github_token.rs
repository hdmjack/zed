use credentials_provider::CredentialsProvider;
use gpui::AsyncApp;
use smol::io::{AsyncReadExt as _, AsyncWriteExt as _};
use std::process::Stdio;
use std::sync::Arc;

const GITHUB_CREDENTIALS_URL: &str = "https://api.github.com";

/// Resolves a GitHub token using a layered fallback chain:
/// 1. `GITHUB_TOKEN` environment variable (explicit override).
/// 2. The app's own credential store (OS keychain via `CredentialsProvider`).
/// 3. Git's credential helper (`git credential fill`) for `github.com` — the
///    standard, tool-agnostic mechanism that reads the same OS keychain / helper
///    the user already uses for `git push`. This avoids depending on the `gh`
///    CLI being on the app's PATH (which it isn't when Zed is launched from the
///    Finder/Dock).
/// 4. The `gh` CLI (`gh auth token`) — a best-effort convenience for users who
///    authenticated with GitHub over SSH (so step 3 finds no HTTPS credential)
///    but have `gh` on PATH (e.g. Zed launched from a terminal).
pub async fn resolve_github_token(
    credential_provider: Arc<dyn CredentialsProvider>,
    cx: &AsyncApp,
) -> Option<String> {
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        if !token.is_empty() {
            return Some(token);
        }
    }

    if let Ok(Some((_username, token_bytes))) = credential_provider
        .read_credentials(GITHUB_CREDENTIALS_URL, cx)
        .await
    {
        if let Ok(token) = String::from_utf8(token_bytes) {
            if !token.is_empty() {
                return Some(token);
            }
        }
    }

    if let Some(token) = git_credential_token().await {
        return Some(token);
    }

    if let Some(token) = gh_cli_token().await {
        return Some(token);
    }

    None
}

/// Best-effort `gh auth token`. Only succeeds when the `gh` binary is on the
/// process PATH, so it's a convenience layer rather than a dependency.
async fn gh_cli_token() -> Option<String> {
    let output = smol::process::Command::new("gh")
        .args(["auth", "token"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!token.is_empty()).then_some(token)
}

/// Ask Git's credential subsystem for a stored `github.com` credential. Git
/// reads from whatever helper the user configured (e.g. `osxkeychain`, or `gh`
/// when set up as a helper), so this works without the `gh` binary on PATH.
///
/// `git credential fill` reads a key=value request on stdin terminated by a
/// blank line and prints the resolved fields (including `password=`) on stdout.
async fn git_credential_token() -> Option<String> {
    let mut child = smol::process::Command::new("git")
        .args(["credential", "fill"])
        // Never fall back to an interactive prompt: with no stored credential we
        // want a clean failure, not a hang.
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    {
        let mut stdin = child.stdin.take()?;
        stdin
            .write_all(b"protocol=https\nhost=github.com\n\n")
            .await
            .ok()?;
        stdin.flush().await.ok()?;
        // `stdin` drops here, closing the pipe so `git` stops reading and runs.
    }

    let mut stdout = child.stdout.take()?;
    let mut output = String::new();
    stdout.read_to_string(&mut output).await.ok()?;
    let status = child.status().await.ok()?;
    if !status.success() {
        return None;
    }

    output
        .lines()
        .find_map(|line| line.strip_prefix("password=").map(str::trim))
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}
