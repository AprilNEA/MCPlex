# Releasing

MCPlex uses release-plz for versioning, changelog updates, crates.io publication,
and the `vX.Y.Z` tag. The tag starts cargo-dist, which builds the release archives,
creates the GitHub Release, and dispatches the matching update to
`AprilNEA/homebrew-tap`.

## Required credentials

Configure these GitHub Actions repository secrets:

- `OP_SERVICE_ACCOUNT_TOKEN`: a 1Password service-account token.
- `OP_GITHUB_APP_ITEM`: a 1Password item reference containing
  `GITHUB_APP_ID`, base64-encoded `GITHUB_APP_PRIVATE_KEY`, and
  `CARGO_REGISTRY_TOKEN` fields.
- `OP_APPLE_SECRET_ITEM`: the same Apple release item used by OpenLogi,
  containing `APPLE_SIGNING_IDENTITY`, base64-encoded `APPLE_CERTIFICATE`,
  `APPLE_CERTIFICATE_PASSWORD`, `APPLE_ID`, `APPLE_PASSWORD`, and
  `APPLE_TEAM_ID` fields.

The GitHub App must be installed on both `AprilNEA/MCPlex` and
`AprilNEA/homebrew-tap`. It needs repository contents and pull-request write
access on MCPlex, and access to dispatch the tap workflow. The tap workflow uses
its own `GITHUB_TOKEN` with `contents: write` to update `Formula/mcplex.rb`.

## Flow

1. Conventional commits land on `master`.
2. If `Cargo.toml` references an unpublished `mcplex-rmcp` version, publish that
   compatibility crate first with
   `cargo publish --manifest-path vendor/rmcp/Cargo.toml`. This is a separate,
   irreversible crates.io publication and requires maintainer approval.
3. release-plz opens or updates a `release-plz/` release PR.
4. Merge the release PR with the generated `chore: release vX.Y.Z` subject.
5. release-plz publishes the crate and creates `vX.Y.Z` with the GitHub App token.
6. cargo-dist builds Linux and macOS archives. The release workflow signs the
   macOS `mcplex` and `mcplex-daemon` executables as
   `com.aprilnea.mcplex.cli` and `com.aprilnea.mcplex.daemon`, respectively,
   with one mutually compatible designated requirement so newly created
   Keychain items can be used by either executable without an authorization prompt,
   then submits each architecture to Apple's notary service before repackaging.
   These are explicit identifiers on the bare Mach-O signatures; the
   `com.aprilnea.mcplex` root remains available for a future native app bundle.
   Bare executables cannot carry a stapled ticket, so `notarytool submit --wait`
   is the release gate and Apple's online ticket is associated with the signed
   code hash.
7. cargo-dist creates the GitHub Release.
8. The successful release dispatches `update-mcplex` to homebrew-tap, which
   downloads the published archives, verifies they exist, computes their hashes,
   and updates the formula.

Do not create the GitHub Release from release-plz: cargo-dist is its sole owner.
On a failed publish, rerun the original release workflow run, which remains pinned
to its release commit. If crates.io publication succeeds but tag creation fails,
first verify that the exact version exists on crates.io, then recover the missing
tag by manually running the Release PR workflow with `release_tag` set to the
missing `vX.Y.Z`. The recovery validates the exact release line, version, and
merged `master` ancestry before using the GitHub App token. When the exact crate
version is already published, it creates the missing tag through the GitHub API
without attempting another crates.io upload, so cargo-dist is triggered from the
original release commit.

If a tag exists but cargo-dist cannot complete because its original runner image
was retired, first update `github-custom-runners` and the Release workflow on
`master`. The workflow is intentionally excluded from cargo-dist freshness
checks with `allow-dirty = ["ci"]` because it adds this guarded recovery path to
the generated jobs. Then cancel the permanently queued run and manually run the
Release workflow with its existing `release_tag`. The recovery checks out and
validates that tag, refuses to overwrite an existing GitHub Release, and keeps
the Release target pinned to the tagged commit.
