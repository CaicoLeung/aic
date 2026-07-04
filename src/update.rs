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
const ZIPSIGN_PUBLIC_KEY: &[u8; 32] = include_bytes!("../keys/zipsign.pub");

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
    // resolved path: honour an explicit HOMEBREW_PREFIX.
    if let Ok(prefix) = std::env::var("HOMEBREW_PREFIX") {
        if let Ok(prefix) = std::path::PathBuf::from(prefix).canonicalize() {
            if exe.starts_with(prefix) {
                return true;
            }
        }
    }
    false
}

pub fn run_update() -> Result<()> {
    if is_homebrew_install() {
        println!(
            "aic was installed via Homebrew. Update it with `brew upgrade aic` (or `brew upgrade CaicoLeung/aic/aic`) instead."
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
        self_update::Status::UpToDate(_) => {
            println!("Already up to date (v{})", cargo_crate_version!())
        }
        self_update::Status::Updated(v) => println!("Updated to version {v}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{exe_is_in_cellar, ZIPSIGN_PUBLIC_KEY};
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
}
