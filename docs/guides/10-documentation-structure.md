# Documentation Structure

## Overview

This guide explains the role of each directory under `docs/`.

## Directories

### `plans/`

Planning documents for past and upcoming work. Each plan captures the requirements, technical approach, and implementation steps for a specific task.

Each plan lives in `plans/<year>/<MMDD>-<slug>/README.md` with a YAML frontmatter `status` field:

```yaml
---
status: Draft
---
```

Valid status values: `Draft` (being written), `Open` (reviewed, ready for implementation), `Completed` (fully implemented), `Cancelled` (abandoned). Once a plan is completed, it is not modified. This preserves the context and decisions made at the time of planning, even if the codebase has since changed.

Plans may include ADRs (Architecture Decision Records) that document significant technical decisions and the reasoning behind them.

### `concepts/`

Living documentation that defines the shared vocabulary for the product. Concept docs describe what domain terms mean in the product context, not how they are implemented.

Unlike plans, concept docs are updated over time to reflect the current understanding of the domain.

### `guides/`

How-to guides for development. Covers topics like installation, development workflow, debugging, testing, and architecture.

### `examples/`

Example files such as configuration samples or shader snippets. These serve as reference for how data structures are used in practice.

### `learning/`

Learning notes and reference material gathered during development.

### `proposals/`

Improvement proposals for review and triage. Proposals capture ideas for future work before they are promoted to full plans.
