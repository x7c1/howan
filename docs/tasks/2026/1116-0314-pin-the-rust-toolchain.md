---
status: completed
pipeline_phase: null
plan: null
base_ref: null
perspectives: null
max_refine_rounds: 3
retries_remaining: 1
check_command: "cargo build && cargo test && cargo clippy --all-targets -- -D warnings && grep -qE '^channel = \"1\\.100\\.0\"' rust-toolchain.toml && [ \"$(rustc --version | cut -d' ' -f2)\" = 1.100.0 ] && ! grep -rq 'dtolnay/rust-toolchain' .github/workflows/ && grep -q 'package-ecosystem: rust-toolchain' .github/dependabot.yml"
assignee: null
branch: task/1116-0314-pin-the-rust-toolchain
created_at: 2026-11-15T18:14:38Z
updated_at: 2026-11-15T18:20:06Z
---

# build: pin the Rust toolchain so a new release arrives as a pull request

## Overview

CI installs whatever Rust is `stable` on the day it runs (`dtolnay/rust-toolchain` with `toolchain: stable` in `.github/workflows/ci.yml`). A new Rust release can therefore fail `main` on a commit that changed no code, typically through a new clippy lint under `-D warnings`. Pin the version instead and let Dependabot raise it, so a new release arrives as a pull request whose checks show what it breaks.

- Add `rust-toolchain.toml` at the repository root pinning `channel = "1.100.0"` (the current stable) with `components = ["rustfmt", "clippy"]`, and a short comment saying what the file is for and that Dependabot raises it. Every `cargo` call in the repository then resolves this version; rustup installs it on first use.
- In every workflow under `.github/workflows/`, replace the `dtolnay/rust-toolchain` step with `rustup toolchain install` (which reads the version and components from `rust-toolchain.toml` when given no toolchain name), so the file is the only place the version is written. Where a cargo cache key exists, include the hash of `rust-toolchain.toml` in it.
- Add a weekly `rust-toolchain` entry (`directory: /`) to `.github/dependabot.yml`, on the same schedule as the existing entries.
- Fix whatever Rust 1.100.0's compiler or clippy reports, in this same change. Do not allow a lint or skip a test to get green.
- Document the pinned toolchain where the README or `docs/guides/` explains how to build (where the version is pinned, that rustup installs it on first use, how it is raised).

## Acceptance criteria

### Automated (pipeline-verified)

- [x] `rust-toolchain.toml` pins 1.100.0, and `rustc` run in the repository reports 1.100.0 (the grep and version comparison in `check_command`)
- [x] `cargo build`, `cargo test` and `cargo clippy --all-targets -- -D warnings` pass on the pinned toolchain
- [x] No workflow uses `dtolnay/rust-toolchain`, and `.github/dependabot.yml` has a `rust-toolchain` entry (the greps in `check_command`)
