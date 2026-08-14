# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- support running Homebrew installations as a persistent user service with
  `brew services`

## [0.5.2] - 2026-08-13

### Changed

- avoid automatic macOS Keychain prompts by keeping the local control token in a
  private file and signing the CLI and daemon with a shared designated requirement
- use a new app-owned OAuth Keychain service; existing OAuth connections must be
  authorized again with `mcplex auth login ID`

## [0.5.1] - 2026-08-13

### Added

- add a dedicated `mcplex-daemon` executable for user services

### Changed

- sign and notarize macOS release binaries with stable CLI and daemon identifiers

### Fixed

- use app token for GitHub releases
- recover already-published release tags

## [0.5.0] - 2026-08-13

### Added

- support MCP `2026-07-28` request metadata, tasks, subscriptions, and related
  request-scoped transport parameters

### Changed

- preserve legacy lifecycle isolation while allowing modern stateless requests

## [0.4.0] - 2026-08-11

### Added

- adopt ecosystem crates and automate releases
