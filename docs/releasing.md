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

The GitHub App must be installed on both `AprilNEA/MCPlex` and
`AprilNEA/homebrew-tap`. It needs repository contents and pull-request write
access on MCPlex, and access to dispatch the tap workflow. The tap workflow uses
its own `GITHUB_TOKEN` with `contents: write` to update `Formula/mcplex.rb`.

## Flow

1. Conventional commits land on `master`.
2. release-plz opens or updates a `release-plz/` release PR.
3. Merge the release PR with the generated `chore: release vX.Y.Z` subject.
4. release-plz publishes the crate and creates `vX.Y.Z` with the GitHub App token.
5. cargo-dist builds Linux and macOS archives and creates the GitHub Release.
6. The successful release dispatches `update-mcplex` to homebrew-tap, which
   downloads the published archives, verifies they exist, computes their hashes,
   and updates the formula.

Do not create the GitHub Release from release-plz: cargo-dist is its sole owner.
On a failed publish, rerun the original release workflow run, which remains pinned
to its release commit. If crates.io publication succeeds but tag creation fails,
first verify that the exact version exists on crates.io, then recover the missing
tag by manually running the Release PR workflow with `release_tag` set to the
missing `vX.Y.Z`. The recovery validates the exact release line, version, and
merged `master` ancestry before using the GitHub App token, so cargo-dist is
triggered from the original release commit.
