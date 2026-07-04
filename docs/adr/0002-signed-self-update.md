# ADR 0002: Sign `aic update` downloads with zipsign

- **Status:** Accepted
- **Date:** 2026-07-04

## Context

`aic update` (for shell- and PowerShell-installed users) downloads the latest
release archive from GitHub and overwrites `current_exe()` via the `self_update`
crate. Before this change it performed **no integrity or authenticity check** on
the downloaded bytes, even though cargo-dist already produced `SHASUMS.txt`.

The gap matters only for specific threat models:

- **T1 — transit tampering / CDN-cache poisoning.** Already mitigated by GitHub
  HTTPS + `self_update`'s `rustls-tls`. Marginal residual.
- **T2 — compromised release asset on GitHub** (hijacked tag push, stolen
  publish token, a malicious dependency that tampers the artifact at build
  time). The `SHASUMS.txt` ships *next to* the asset, so an attacker who can
  replace one can replace both — it cannot defend T2.
- **T3 — compromised maintainer / signing key.** Out of scope for client-side
  verification.

We only needed to close T2.

## Decision

Enable `self_update`'s `signatures` feature and verify every download against an
ed25519ph public key embedded in the binary (`keys/zipsign.pub`, committed).
Release archives are signed in CI with the matching private key (the
`ZIPSIGN_PRIVATE_KEY` Actions secret) before cargo-dist generates checksums, so
the published `SHASUMS.txt` covers the signed bytes.

Mechanism chosen: **zipsign**, because it is what `self_update`'s `verifying_keys`
API consumes natively (`zipsign_api::verify`). cosign/Sigstore keyless would have
avoided a long-lived private key but required custom verification code this
crate does not otherwise need.

The Homebrew channel is untouched — `is_homebrew_install()` redirects brew users
to `brew upgrade` before this path is reached (see ADR-0001).

## Transition

`self_update` fails *open* when no keys are configured, so the existing
installed base (no embedded key) updates to the first signed release
**unverified** — no worse than today — and from that release onward verification
is active. No flag day, no stranded users.

## Rotation

Single embedded key. If the private key is compromised: generate a new keypair,
commit the new `keys/zipsign.pub`, and the next release embeds the new trust
root. Binaries already running the old embedded key would fail-closed on
newly-signed assets and need a one-time reinstall. For a personal CLI this is
acceptable; an overlap period with two embedded keys was considered and
deferred.

## Consequences

- **Positive:** A compromised release asset can no longer silently replace a
  running binary for self-updating users; the download is rejected.
- **Positive:** zipsign signatures are transparent to normal archive tools, so
  shell/PowerShell installers and `SHASUMS.txt` keep working unchanged.
- **Negative:** One Actions secret (`ZIPSIGN_PRIVATE_KEY`) must be maintained;
  if it is missing or wrong the release fails at the signing step (fail-closed).
- **Negative:** Key rotation strands old binaries pending a reinstall.

## Alternatives considered

- **cosign / Sigstore keyless.** No long-lived private key (OIDC + Rekor
  transparency log), but `self_update` has no native cosign verification, so it
  would mean bespoke verification code. Disproportionate for a personal CLI.
- **Fetch `SHASUMS.txt` from the release and check.** `self_update` does not
  support checksum-from-URL verification, and even if it did, a checksum that
  ships beside the asset cannot defend T2. Security theater.
- **Document the HTTPS-only trust boundary and do nothing.** Honest, but leaves
  the T2 gap open for cheap; we chose to close it.
