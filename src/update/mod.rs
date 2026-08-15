use anyhow::Result;
use self_update::cargo_crate_version;

/// zipsign ed25519ph verifying (public) key for `aic update`.
///
/// Release archives are signed in CI with the matching private key (the
/// `ZIPSIGN_PRIVATE_KEY` Actions secret); this embedded key lets `self_update`
/// reject a download whose signature does not verify, so a compromised or
/// corrupted release asset cannot overwrite a running binary. Homebrew
/// installs never reach this path — `is_homebrew_install()` redirects them to
/// `brew upgrade` first. See `docs/adr/0002-signed-self-update.md`.
///
/// Raw 32-byte ed25519 public key — zipsign `gen-key` writes the bare key with
/// no armor. The `&[u8; 32]` annotation makes a malformed key file a compile
/// error.
const ZIPSIGN_PUBLIC_KEY: &[u8; 32] = include_bytes!("../../keys/zipsign.pub");

/// True if a resolved binary path lives inside a Homebrew Cellar.
///
/// Homebrew installs the binary in a versioned Cellar directory and symlinks
/// it into the bin dir, so `current_exe()` for a brew install contains a
/// `Cellar` path component. Matching on the exact component (not a substring)
/// avoids false positives on paths like `/opt/MyCellar/...`.
fn exe_is_in_cellar(exe: &std::path::Path) -> bool {
    exe.iter().any(|c| c == std::ffi::OsStr::new("Cellar"))
}

/// Detect whether `aic` is being run from a Homebrew install.
///
/// We must not run the in-place self-updater against such an install: it would
/// replace the file behind Homebrew's back, desyncing `brew`'s recorded version
/// and fighting `brew upgrade`. See `docs/adr/0001-self-update-homebrew-guard.md`.
fn is_homebrew_install() -> bool {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return false,
    };
    if exe_is_in_cellar(&exe) {
        return true;
    }
    // Fallback for layouts where the Cellar component isn't visible in the
    // resolved path: honour an explicit HOMEBREW_PREFIX. Iterator chain avoids
    // clippy::collapsible_if on nested if-lets.
    std::env::var("HOMEBREW_PREFIX")
        .ok()
        .and_then(|p| std::path::PathBuf::from(p).canonicalize().ok())
        .is_some_and(|prefix| exe.starts_with(&prefix))
}

/// Build the `brew upgrade aic` command for Homebrew-managed installs.
///
/// Extracted as a helper so tests can assert the exact invocation contract
/// without shelling out to a real brew.
fn brew_upgrade_command() -> std::process::Command {
    let mut cmd = std::process::Command::new("brew");
    cmd.args(["upgrade", "aic"]);
    cmd
}

pub fn run_update() -> Result<()> {
    if is_homebrew_install() {
        let status = brew_upgrade_command().status()?;
        anyhow::ensure!(
            status.success(),
            "brew upgrade failed (exit {})",
            status.code().map_or("signal".to_owned(), |c| c.to_string())
        );
        return Ok(());
    }

    let status = self_update::backends::github::Update::configure()
        .repo_owner("CaicoLeung")
        .repo_name("aic")
        .bin_name("aic")
        .show_download_progress(true)
        .current_version(cargo_crate_version!())
        .verifying_keys([*ZIPSIGN_PUBLIC_KEY])
        .build()?
        .update()?;
    match status {
        self_update::VersionStatus::UpToDate(_) => {
            println!("Already up to date (v{})", cargo_crate_version!())
        }
        self_update::VersionStatus::Updated(v) => println!("Updated to version {v}"),
        // `VersionStatus` is `#[non_exhaustive]`; any future variant is reported generically.
        _ => println!("Update completed (v{})", cargo_crate_version!()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ZIPSIGN_PUBLIC_KEY, brew_upgrade_command, exe_is_in_cellar};
    use std::path::Path;

    #[test]
    fn detects_apple_silicon_cellar_path() {
        assert!(exe_is_in_cellar(Path::new(
            "/opt/homebrew/Cellar/aic/0.1.6/bin/aic"
        )));
    }

    #[test]
    fn detects_intel_cellar_path() {
        assert!(exe_is_in_cellar(Path::new(
            "/usr/local/Cellar/aic/0.1.6/bin/aic"
        )));
    }

    #[test]
    fn detects_linuxbrew_cellar_path() {
        assert!(exe_is_in_cellar(Path::new(
            "/home/linuxbrew/.linuxbrew/Cellar/aic/0.1.6/bin/aic"
        )));
    }

    #[test]
    fn rejects_cargo_bin_path() {
        assert!(!exe_is_in_cellar(Path::new("/home/user/.cargo/bin/aic")));
    }

    #[test]
    fn rejects_similarly_named_directory() {
        // "MyCellar" must not match — we compare whole path components, not substrings.
        assert!(!exe_is_in_cellar(Path::new("/opt/MyCellar/aic")));
    }

    #[test]
    fn zipsign_public_key_is_present() {
        // Catches an accidentally zeroed or placeholder key file. The `&[u8; 32]`
        // type annotation already guarantees the length at compile time.
        assert!(
            ZIPSIGN_PUBLIC_KEY.iter().any(|&b| b != 0),
            "embedded zipsign public key must not be all-zero"
        );
    }

    #[test]
    fn brew_upgrade_command_invokes_correct_program() {
        let cmd = brew_upgrade_command();
        assert_eq!(cmd.get_program(), "brew");
    }

    #[test]
    fn brew_upgrade_command_passes_correct_args() {
        let cmd = brew_upgrade_command();
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        assert_eq!(args, vec!["upgrade", "aic"]);
    }
}
