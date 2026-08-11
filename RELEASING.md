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

`main` is branch-protected: a `pull_request` rule (code-owner review, linear
history) plus required status checks (`plan` from the Release workflow, `deny`
from CI) gate every merge. The release scripts both require a clean `main`
synced with `origin/main`, so the bump commit rides a PR before the tag goes
out. The owner merges their own release PRs with an administrator merge — an
owner cannot approve their own PR, so the code-owner-review requirement can
never be satisfied on a self-authored release PR any other way (see
`.github/CODEOWNERS`).

### 1. Prepare the bump (on `main`)

From a clean `main`, synced with `origin/main`:

```bash
scripts/prepare-release.sh 0.1.6   # bump Cargo.toml + Cargo.lock, regenerate
                                   # CHANGELOG.md via git-cliff, commit as
                                   # chore(release): v0.1.6 (local, on main)
git show HEAD                      # review the generated diff
```

`prepare-release.sh` deliberately **does not tag** — it only commits the bump.
Move that commit onto a release branch and restore `main`:

```bash
git branch chore/release-v0.1.6    # capture the bump commit
git reset --hard origin/main       # main back to a clean, synced state
git push -u origin chore/release-v0.1.6
gh pr create --base main --head chore/release-v0.1.6 \
  --title "chore(release): v0.1.6" --body "Release prep for v0.1.6."
```

### 2. Merge the PR

Wait for the required checks to pass, then squash-merge with an administrator
merge:

```bash
gh pr checks --watch               # wait for plan + deny (and lint/test)
gh pr merge --squash --admin --subject "chore(release): v0.1.6" --body ""
git pull --ff-only                 # fast-forward local main to the merged commit
```

Let `plan`/`deny` pass first even though `--admin` overrides branch
protection — you want the bump commit proven green before you build a release
on top of it. Squash keeps history linear (required by the `main` ruleset).

### 3. Tag and push

```bash
scripts/release.sh 0.1.6 --dry-run # assert: Cargo.toml = 0.1.6, main synced, tag new
scripts/release.sh 0.1.6           # create annotated tag v0.1.6, push tag
```

The tag push is irreversible — it triggers the release workflow, and branch
protection blocks force-push — so review before tagging. `release.sh` also
pushes `main`, a no-op now that the bump landed via PR; only the tag fires the
workflow.

Once the tag is pushed, CI does the rest:

1. `release.yml` — preflight (token check) → build 5 targets → smoke-test 4 →
   sign archives (zipsign) → checksum + installers → publish GitHub Release →
   push Homebrew formula. The release body is cargo-dist's announcement, which
   embeds the matching `CHANGELOG.md` entry (Release Notes) plus the
   install/download sections — no separate changelog step is needed.

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

### 2. Roll forward (existing self-update users)

The only path off a broken version for `aic update` users is a higher good one:

```bash
# Fix the regression, then cut the next release following
# "Cutting a release" above (prepare → PR → merge → tag).
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
