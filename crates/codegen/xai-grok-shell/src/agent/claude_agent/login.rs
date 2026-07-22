//! Login/auth for the harness backend — **delegated entirely to `claude`**.
//!
//! The whole point of using the Agent SDK is that authentication is Anthropic's
//! own flow: `claude login` mints and refreshes the Pro/Max subscription
//! credential, stored where the `claude` runtime expects it. Atlas does not
//! reimplement OAuth here (that was the removed `anthropic-subscription` path);
//! it only *detects* whether the harness is available and authenticated, and
//! surfaces the `claude login` command when it isn't.

use std::path::PathBuf;

/// Overridable name/path of the harness binary (`$CLAUDE_AGENT_BIN`, else
/// `claude`).
pub fn binary_name() -> String {
    std::env::var("CLAUDE_AGENT_BIN")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "claude".to_owned())
}

/// Availability + auth status of the harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HarnessStatus {
    /// `claude` is on PATH and a credential is present — ready to run turns.
    Ready,
    /// `claude` is installed but no subscription/API credential was found.
    /// The user must run [`login_command`].
    NotLoggedIn,
    /// The `claude` binary could not be located on PATH.
    NotInstalled,
}

/// The shell command a user runs to authenticate the harness.
pub fn login_command() -> String {
    format!("{} login", binary_name())
}

/// Human-readable guidance for a given status, shown in `/login` and errors.
pub fn status_hint(status: &HarnessStatus) -> String {
    match status {
        HarnessStatus::Ready => "Claude Agent SDK is installed and authenticated.".to_owned(),
        HarnessStatus::NotLoggedIn => format!(
            "Claude Agent SDK is installed but not authenticated. Run `{}` to sign in with your Claude Pro/Max subscription.",
            login_command()
        ),
        HarnessStatus::NotInstalled => format!(
            "The `{}` binary was not found on PATH. Install the Claude Agent SDK / Claude Code CLI, then run `{}`.",
            binary_name(),
            login_command()
        ),
    }
}

/// Detect harness status against the real filesystem/PATH.
///
/// Auth detection is best-effort: it checks for a `claude` credentials file
/// rather than making a network call, so a stale/expired credential still reads
/// as `Ready` — the definitive check is the first turn (an auth failure there
/// surfaces the login hint). This mirrors how the harness itself defers auth
/// validation to request time.
///
/// On macOS, `claude login` stores credentials in the login Keychain (service
/// `Claude Code-credentials`), not in `~/.claude/.credentials.json` — that file
/// only exists on Linux. So the file probe alone always reads as
/// `NotLoggedIn` on a Mac even when the user is already logged in; the Keychain
/// is checked as a fallback there.
pub fn detect() -> HarnessStatus {
    let status = detect_with(
        |name| which_on_path(name),
        |dir| dir.join(".claude").join(".credentials.json").exists(),
    );
    if matches!(status, HarnessStatus::NotLoggedIn) && has_macos_keychain_credential() {
        HarnessStatus::Ready
    } else {
        status
    }
}

/// Whether the macOS login Keychain has a `Claude Code-credentials` item.
/// Shells out to `security` (no keychain-access crate in the dependency
/// closure); a missing binary or lookup failure is treated as absent.
#[cfg(target_os = "macos")]
fn has_macos_keychain_credential() -> bool {
    std::process::Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", "Claude Code-credentials"])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[cfg(not(target_os = "macos"))]
fn has_macos_keychain_credential() -> bool {
    false
}

/// Testable core of [`detect`] with injected PATH lookup and credential probe.
pub fn detect_with(
    locate: impl Fn(&str) -> Option<PathBuf>,
    has_credentials: impl Fn(&std::path::Path) -> bool,
) -> HarnessStatus {
    if locate(&binary_name()).is_none() {
        return HarnessStatus::NotInstalled;
    }
    // The `claude` runtime keeps credentials under $HOME (or $CLAUDE_CONFIG_DIR).
    let home = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
    match home {
        Some(dir) if has_credentials(&dir) => HarnessStatus::Ready,
        // Binary present but no credential file we can see. Some installs store
        // auth elsewhere (keychain); treat as NotLoggedIn so the user gets the
        // login hint rather than a silent failure at turn time.
        Some(_) => HarnessStatus::NotLoggedIn,
        None => HarnessStatus::NotLoggedIn,
    }
}

/// Locate an executable on `PATH` (minimal `which`, no external crate).
fn which_on_path(name: &str) -> Option<PathBuf> {
    // Absolute/relative path given directly.
    let direct = PathBuf::from(name);
    if direct.is_absolute() && direct.exists() {
        return Some(direct);
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let exe = dir.join(format!("{name}.exe"));
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn not_installed_when_binary_missing() {
        let status = detect_with(|_| None, |_| true);
        assert_eq!(status, HarnessStatus::NotInstalled);
    }

    #[test]
    fn ready_when_binary_and_credentials_present() {
        let status = detect_with(|_| Some(PathBuf::from("/usr/bin/claude")), |_| true);
        assert_eq!(status, HarnessStatus::Ready);
    }

    #[test]
    fn not_logged_in_when_credentials_absent() {
        // HOME is set in the test env; credentials probe returns false.
        let status = detect_with(|_| Some(PathBuf::from("/usr/bin/claude")), |_: &Path| false);
        assert_eq!(status, HarnessStatus::NotLoggedIn);
    }

    #[test]
    fn login_command_uses_binary_name() {
        // Default binary name unless CLAUDE_AGENT_BIN overrides it.
        assert!(login_command().ends_with(" login"));
    }

    #[test]
    fn status_hints_mention_login_where_relevant() {
        assert!(status_hint(&HarnessStatus::NotLoggedIn).contains("login"));
        assert!(status_hint(&HarnessStatus::NotInstalled).contains("PATH"));
    }
}
