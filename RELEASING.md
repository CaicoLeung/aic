# Releasing `aic`

This is the authoritative procedure for cutting a release and the runbook for
rolling one back. It documents what the scripts and CI do, and the two secrets
that must be healthy before you start.

## Prerequisites (one-time / maintenance)

| Secret | Used by | Required for |
|---|---|---|
| `ZIPSIGN_PRIVATE_KEY` | `release.yml` `build-global-artifacts` (signs archives) | Every release from `0.1.6` onward. Set with `base64 < keys/zipsign.key \| gh secret set ZIPSIGN_PRIVATE_KEY`. Missing/wrong → release fails closed at signing. |
| `HOMEBREW_TAP_TOKEN` | `release.yml` `publish-homebrew-formula` | Every non-prerelease release (publishes the formula to `CaicoLeung/homebrew-aic`). A weekly monitor probes this and opens an issue before it expires; the release job also fail-fast probes it. |

`keys/zipsign.pub` is committed; `keys/zipsign.key` is gitignored and never
leaves your machine. See `docs/adr/0002-signed-self-update.md`.

## Cutting a release

From a clean `main`, synced with `origin/main`:

```bash
scripts/prepare-release.sh 0.1.6            # bump Cargo.toml + Cargo.lock,
                                            # regenerate CHANGELOG.md via
                                            # git-cliff, commit as
                                            # chore(release): v0.1.6
git show HEAD                                               # review the diff
scripts/release.sh 0.1.6 --dry-run          # assert release-readiness
scripts/release.sh 0.1.6                    # assert, tag v0.1.6, push main + tag
```

`prepare-release.sh` deliberately **does not tag** — the tag push is
irreversible (it triggers the release workflow, and branch protection blocks
force-push), so review the generated CHANGELOG and version bump first.

Once the tag is pushed, CI does the rest:

1. `release.yml` — preflight (token check) → build 5 targets → smoke-test 4 →
   sign archives (zipsign) → checksum + installers → publish GitHub Release →
   push Homebrew formula.
2. `changelog.yml` — rewrites the release body with git-cliff notes and commits
   `CHANGELOG.md` back to `main` (3-attempt retry on push races).

`self_update` users run `aic update`; Homebrew users run `brew upgrade aic`.

## Rollback runbook

You cannot truly "un-publish" a release. Two forces make rollback asymmetric:

- **`self_update` won't downgrade** (semver — it only moves to higher versions).
  Users who auto-updated to a broken release are stranded until a *higher* good
  release exists.
- **Retagging confuses anyone who already fetched the tag**, and is avoided.

So recovery is two **complementary** moves:

### 1. Stop the bleeding (new installs)

Prevent new users from installing the broken version:

```bash
# Remove the broken release (the tag stays; only the release page + assets go).
gh release delete vX.Y.Z --yes

# Revert the formula commit that the publish-homebrew-formula job pushed.
cd ../homebrew-aic        # or: gh repo clone CaicoLeung/homebrew-aic
git log --oneline -3      # find the "<name> <version>" commit for the broken release
git revert <commit-sha>
git push
```

After this, `brew install aic` and the shell/PowerShell installers resolve to
the prior good release (the GitHub "latest" pointer falls back to it once the
broken release is deleted).

### 2. Roll forward (existing self-update users)

The only path off a broken version for `aic update` users is a higher good one:

```bash
# Fix the regression, then prepare and ship the next version as usual:
scripts/prepare-release.sh 0.1.7
scripts/release.sh 0.1.7
```

`aic update` on the broken `0.1.6` will now move to `0.1.7`.

### Notes

- With zipsign live (from `0.1.6`), reverting the Homebrew formula still points
  at *signed* prior assets — verification holds. The first signed release
  (`0.1.6`) is also the floor below which old binaries cannot verify, so "revert
  to pre-0.1.6" is not a clean option for self-update users — roll forward
  instead.
- Prerelease-gated releases (`vX.Y.Z-rc.1` → smoke → promote) are a future
  hardening option, not yet in use; the release smoke test is the current gate.
