# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

NotedThat uses ecosystem-level Semantic Versioning: all 9 crates share a single
version; any breaking change in any crate increments the ecosystem major version.
See [RELEASING.md](RELEASING.md) for the full versioning policy.

## [Unreleased]

### Added

- `replace` MCP tool for content-based string replacement without byte/line coordinates (closes #39)
- `edit` MCP tool extended with optional byte-range args (`byte_start`, `byte_end`) alongside existing line-range args (closes #40)
- M9 milestone complete: MCP surface now exposes 10 tools total
