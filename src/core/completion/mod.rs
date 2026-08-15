//! Self-contained shell-completion installer for `aic completion`.
//!
//! A deep module: the public surface is just the [`Shell`] enum and three
//! entry points — [`detect_shell`], [`prompt_shell`], and
//! [`install_completion`]. Everything else (the per-shell script generator,
//! install-path table, Homebrew detection, follow-up copy) is private
//! implementation detail. The two side-effect-free seams the tests target —
//! [`write_completion`] (writes a script to any `&mut dyn io::Write`) and
//! [`install_completion_impl`] (writes to a given `home`, no env/TTY) — are
//! `pub(crate)` so they can be exercised in isolation against a buffer or a
//! temp dir.
//!
//! This used to live in `main.rs`; it was extracted (AIC-20) because it is a
//! single, well-bounded concern with no dependencies on the commit/resolve
//! workflows.

use crate::core::cli;
use crate::workflow::confirm::inquire_opt;
use clap::CommandFactory;
use std::path::{Path, PathBuf};

/// Where a shell's completion script is installed, and whether the shell
/// picks it up on reload with no further action.
pub(crate) struct CompletionTarget {
    path: PathBuf,
    autoloaded: bool,
}

/// Shells `aic completion` can install for — the single source of truth for
/// everything per-shell: the menu label, the `$SHELL` basenames that detect it,
/// the script generator, the install path, and the follow-up when that path
/// isn't autoloaded. Adding a shell means adding one variant and filling in each
/// method; the exhaustive matches turn a forgotten step into a compile error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    Nushell,
}

impl Shell {
    /// Installable shells, in menu order.
    const ALL: [Self; 4] = [Self::Bash, Self::Zsh, Self::Fish, Self::Nushell];

    /// Maps a shell basename (e.g. the tail of `$SHELL`) to a `Shell`.
    fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|&shell| shell.detect_names().contains(&name))
    }

    /// Lowercase display name — the menu label and the word used in messages.
    fn name(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Zsh => "zsh",
            Self::Fish => "fish",
            Self::Nushell => "nushell",
        }
    }

    /// `$SHELL`-basenames that identify this shell (e.g. the tail of `/usr/bin/zsh`).
    fn detect_names(self) -> &'static [&'static str] {
        match self {
            Self::Bash => &["bash"],
            Self::Zsh => &["zsh"],
            Self::Fish => &["fish"],
            Self::Nushell => &["nu", "nushell"],
        }
    }

    /// Writes this shell's completion script to `out`.
    fn generate(self, cmd: &mut clap::Command, bin_name: &str, out: &mut dyn std::io::Write) {
        use clap_complete::{Shell as ClapShell, generate};
        use clap_complete_nushell::Nushell;
        match self {
            Self::Bash => generate(ClapShell::Bash, cmd, bin_name, out),
            Self::Zsh => generate(ClapShell::Zsh, cmd, bin_name, out),
            Self::Fish => generate(ClapShell::Fish, cmd, bin_name, out),
            Self::Nushell => generate(Nushell, cmd, bin_name, out),
        }
    }

    /// Conventional install location for this shell, plus whether the shell
    /// autoloads it. `bash` and `fish` are always autoloaded; `zsh` never is —
    /// its `site-functions` dir only loads when it's on `$fpath`, which depends
    /// on the user's zsh (Homebrew's own zsh adds the brew dir; macOS system
    /// zsh does not), so a follow-up is always shown. The Homebrew dir is still
    /// the better *location* when `aic` is brewed. `nushell` lands in its config
    /// dir but must be `source`d from `config.nu`.
    fn install_target(self, home: &Path, brew_prefix: Option<&Path>) -> CompletionTarget {
        match self {
            Self::Fish => CompletionTarget {
                path: home.join(".config/fish/completions/aic.fish"),
                autoloaded: true,
            },
            Self::Bash => CompletionTarget {
                path: home.join(".local/share/bash-completion/completions/aic"),
                autoloaded: true,
            },
            // Never autoloaded: the site-functions dir loads only if the user's
            // zsh has it on $fpath (Homebrew's own zsh yes; system zsh no).
            Self::Zsh => {
                let dir = brew_prefix
                    .map(|p| p.join("share/zsh/site-functions"))
                    .unwrap_or_else(|| home.join(".local/share/zsh/site-functions"));
                CompletionTarget {
                    path: dir.join("_aic"),
                    autoloaded: false,
                }
            }
            Self::Nushell => CompletionTarget {
                path: home.join(".config/nushell/aic.nu"),
                autoloaded: false,
            },
        }
    }

    /// Follow-up the user must perform when the install isn't autoloaded, or
    /// `None` when a reload is all that's needed.
    fn follow_up(self, path: &Path) -> Option<String> {
        let dir = path.parent().unwrap_or(path);
        match self {
            Self::Zsh => Some(format!(
                "Add this directory to $fpath for it to take effect:\n  \
                 fpath+=({0})  # then: autoload -Uz compinit && compinit",
                dir.display()
            )),
            Self::Nushell => Some(format!(
                "Source it from your nushell config to take effect — add to `config.nu`:\n  \
                 source {0}",
                path.display()
            )),
            // bash/fish are autoloaded, so this is never reached for them —
            // but the arm keeps the match exhaustive when a shell is added.
            Self::Bash | Self::Fish => None,
        }
    }
}

/// Best-effort detection of the current shell from `$SHELL`. `$SHELL` is the
/// login shell, not necessarily the one actually running, so it's only a hint —
/// it defaults the interactive menu and is the non-TTY fallback.
pub fn detect_shell() -> Option<Shell> {
    let shell = std::env::var("SHELL").ok()?;
    let name = shell.rsplit('/').next()?;
    Shell::from_name(name)
}

/// If `aic` itself lives under a Homebrew prefix, returns that prefix so zsh
/// completions can land in the tap's conventional `site-functions` directory.
fn homebrew_prefix_from(exe: &Path) -> Option<PathBuf> {
    [Path::new("/opt/homebrew"), Path::new("/usr/local")]
        .into_iter()
        .find(|prefix| exe.starts_with(prefix))
        .map(Path::to_path_buf)
}

/// Writes `shell`'s completion script to `out`.
///
/// Side-effect-free over `out`: any `&mut dyn io::Write` works, so a test can
/// hand it a `Vec<u8>` buffer and assert on the emitted script.
pub(crate) fn write_completion(shell: Shell, out: &mut dyn std::io::Write) {
    let mut cmd = cli::Cli::command();
    let bin_name = cmd.get_name().to_owned();
    shell.generate(&mut cmd, &bin_name, out);
}

/// Writes `shell`'s completion script to its install location.
///
/// Split from [`install_completion`] so the file I/O can be exercised against a
/// temp directory instead of the real home.
pub(crate) fn install_completion_impl(
    shell: Shell,
    home: &Path,
    brew_prefix: Option<&Path>,
) -> anyhow::Result<CompletionTarget> {
    let target = shell.install_target(home, brew_prefix);
    if let Some(parent) = target.path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut buf = Vec::new();
    write_completion(shell, &mut buf);
    std::fs::write(&target.path, buf)?;
    Ok(target)
}

/// Installs `shell`'s completion to its conventional location and prints the
/// result, plus any follow-up the user needs.
pub fn install_completion(shell: Shell) -> anyhow::Result<()> {
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("could not determine your home directory"))?;
    let brew_prefix = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.canonicalize().ok())
        .and_then(|exe| homebrew_prefix_from(&exe));

    let target = install_completion_impl(shell, &home, brew_prefix.as_deref())?;
    eprintln!(
        "Installed {0} completion to: {1}",
        shell.name(),
        target.path.display()
    );
    if target.autoloaded {
        eprintln!(
            "Reload your shell (e.g. `exec {0}`) and Tab completion will be active.",
            shell.name()
        );
    } else if let Some(hint) = shell.follow_up(&target.path) {
        eprintln!("{hint}");
    }
    Ok(())
}

/// Interactively pick a shell to install completions for, defaulting the
/// highlight to `default` (usually the detected login shell). Returns `None`
/// when the user cancels (Esc / Ctrl-C).
pub fn prompt_shell(default: Option<Shell>) -> anyhow::Result<Option<Shell>> {
    use inquire::Select;
    use inquire::list_option::ListOption;

    let labels: Vec<&'static str> = Shell::ALL.iter().map(|s| Shell::name(*s)).collect();
    let highlight = default
        .and_then(|d| Shell::ALL.iter().position(|&s| s == d))
        .unwrap_or(0);

    let selection = Select::new("Install completions for which shell?", labels)
        .with_starting_cursor(highlight)
        .raw_prompt();
    Ok(inquire_opt(selection)?.map(|ListOption { index, .. }| Shell::ALL[index]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_completion_emits_nonempty_script_naming_aic_for_every_shell() {
        for shell in Shell::ALL {
            let mut buf = Vec::new();
            write_completion(shell, &mut buf);
            let script = String::from_utf8(buf).expect("completion output must be valid UTF-8");
            assert!(!script.is_empty(), "{shell:?}: completion script was empty");
            assert!(
                script.contains("aic"),
                "{shell:?}: completion script did not reference the `aic` binary"
            );
        }
    }

    #[test]
    fn from_name_maps_known_shells_and_rejects_unknown() {
        assert_eq!(Shell::from_name("zsh"), Some(Shell::Zsh));
        assert_eq!(Shell::from_name("bash"), Some(Shell::Bash));
        assert_eq!(Shell::from_name("fish"), Some(Shell::Fish));
        assert_eq!(Shell::from_name("nu"), Some(Shell::Nushell));
        assert_eq!(Shell::from_name("nushell"), Some(Shell::Nushell));
        assert_eq!(Shell::from_name("tcsh"), None);
        assert_eq!(Shell::from_name(""), None);
    }

    #[test]
    fn homebrew_prefix_matches_brew_locations_only() {
        use std::path::Path;
        assert_eq!(
            homebrew_prefix_from(Path::new("/opt/homebrew/bin/aic")),
            Some(Path::new("/opt/homebrew").to_path_buf())
        );
        assert_eq!(
            homebrew_prefix_from(Path::new("/usr/local/bin/aic")),
            Some(Path::new("/usr/local").to_path_buf())
        );
        assert_eq!(homebrew_prefix_from(Path::new("/usr/bin/aic")), None);
        assert_eq!(
            homebrew_prefix_from(Path::new("/home/me/.cargo/bin/aic")),
            None
        );
    }

    #[test]
    fn install_target_picks_autoloaded_dirs() {
        use std::path::Path;
        let home = Path::new("/home/me");

        // fish & bash: always autoloaded via their conventional dirs.
        let t = Shell::Fish.install_target(home, None);
        assert_eq!(
            t.path,
            Path::new("/home/me/.config/fish/completions/aic.fish")
        );
        assert!(t.autoloaded);

        let t = Shell::Bash.install_target(home, None);
        assert_eq!(
            t.path,
            Path::new("/home/me/.local/share/bash-completion/completions/aic")
        );
        assert!(t.autoloaded);

        // zsh under a Homebrew prefix: better location, but still not autoloaded
        // — the brew site-functions dir only loads if the user's zsh has it on
        // $fpath (Homebrew's own zsh yes, macOS system zsh no).
        let t = Shell::Zsh.install_target(home, Some(Path::new("/opt/homebrew")));
        assert_eq!(
            t.path,
            Path::new("/opt/homebrew/share/zsh/site-functions/_aic")
        );
        assert!(!t.autoloaded);

        // zsh elsewhere: XDG dir, needs the user to add it to $fpath.
        let t = Shell::Zsh.install_target(home, None);
        assert_eq!(
            t.path,
            Path::new("/home/me/.local/share/zsh/site-functions/_aic")
        );
        assert!(!t.autoloaded);

        // nushell: lands in its config dir but isn't autoloaded.
        let t = Shell::Nushell.install_target(home, None);
        assert_eq!(t.path, Path::new("/home/me/.config/nushell/aic.nu"));
        assert!(!t.autoloaded);
    }

    #[test]
    fn install_completion_impl_writes_a_nonempty_script_to_the_right_path() {
        let dir = tempfile::tempdir().expect("tempdir");

        // zsh (XDG fallback): file lands at the expected path and references aic.
        let target = install_completion_impl(Shell::Zsh, dir.path(), None).expect("install zsh");
        assert!(!target.autoloaded);
        assert!(
            target
                .path
                .ends_with(".local/share/zsh/site-functions/_aic")
        );
        let body = std::fs::read_to_string(&target.path).expect("read installed script");
        assert!(!body.is_empty());
        assert!(body.contains("aic"));

        // fish: autoloaded, distinct filename.
        let target = install_completion_impl(Shell::Fish, dir.path(), None).expect("install fish");
        assert!(target.autoloaded);
        assert!(target.path.ends_with(".config/fish/completions/aic.fish"));

        // nushell: installed to its config dir, not autoloaded.
        let target =
            install_completion_impl(Shell::Nushell, dir.path(), None).expect("install nushell");
        assert!(!target.autoloaded);
        assert!(target.path.ends_with(".config/nushell/aic.nu"));
        let body = std::fs::read_to_string(&target.path).expect("read installed script");
        assert!(!body.is_empty());
        assert!(body.contains("aic"));
    }

    /// `aic use <TAB>` must offer the full `use` vocabulary — CLI-agent
    /// presets plus every provider canonical name and alias not shadowed by
    /// a preset — instead of the `_default` action, which completes nothing
    /// (the reported "completion not working for aic use"). Asserts exact
    /// equality with [`cli::use_vocabulary`] (the single source both clap
    /// and this script derive from), not loose `contains` — the arg's help
    /// text already names several providers, so a loose check would pass
    /// even with an empty value list.
    #[test]
    fn zsh_completion_offers_use_vocabulary() {
        let mut buf = Vec::new();
        write_completion(Shell::Zsh, &mut buf);
        let script = String::from_utf8(buf).expect("completion output must be valid UTF-8");

        let spec = script
            .lines()
            .find(|l| l.contains("':provider"))
            .unwrap_or_else(|| panic!("no provider spec line in zsh script:\n{script}"));
        let (_, values) = spec
            .rsplit_once(":(")
            .unwrap_or_else(|| panic!("provider spec carries no value list:\n{spec}"));
        let offered: Vec<&str> = values
            .trim_end_matches(|c: char| !c.is_alphanumeric() && c != '-')
            .split_whitespace()
            .collect();

        assert_eq!(
            offered,
            cli::use_vocabulary(),
            "zsh script must offer exactly the `aic use` vocabulary"
        );
    }

    /// `detect_shell` maps `$SHELL` (basename) to a supported shell; unknown
    /// names and an unset variable yield `None`, so the completion prompt can
    /// fall back to a manual pick. Uses `temp_env` to avoid unsafe env
    /// mutation racing other tests.
    #[test]
    fn detect_shell_reads_shell_env() {
        temp_env::with_var("SHELL", Some("/bin/zsh"), || {
            assert_eq!(detect_shell(), Some(Shell::Zsh));
        });
        temp_env::with_var("SHELL", Some("/usr/bin/bash"), || {
            assert_eq!(detect_shell(), Some(Shell::Bash));
        });
        temp_env::with_var("SHELL", Some("fish"), || {
            assert_eq!(detect_shell(), Some(Shell::Fish));
        });
        temp_env::with_var("SHELL", Some("nu"), || {
            assert_eq!(detect_shell(), Some(Shell::Nushell));
        });
        temp_env::with_var("SHELL", Some("/bin/tcsh"), || {
            assert_eq!(detect_shell(), None);
        });
        temp_env::with_var("SHELL", None::<&str>, || {
            assert_eq!(detect_shell(), None);
        });
    }
}
