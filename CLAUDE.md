# Claude AI Guidelines

## Documentation

**DRY Principle**: Write each piece of information in ONE place only.

- **README.md**: Overview and command reference only
- **docs/guides/**: Detailed explanations

Never duplicate content across files.

For documentation directory roles and rules, see @docs/guides/10-documentation-structure.md.

### Markdown Files (100+ lines)

- Always include an Overview section at the beginning
- The Overview should summarize the document's purpose and key points
- This is critical because automated tools may read only the beginning of .md files
- Without an Overview at the top, tools cannot understand the document's content

## Code Quality

After making code changes, always run:

```bash
cargo build && cargo test && cargo clippy --all-targets -- -D warnings
```

Fix any issues before considering the task complete.

### Fix issues as you find them

- When you notice code smells or inappropriate patterns (silent error suppression, missing logs, inconsistent naming, etc.) during implementation, fix them in the same PR. Do not leave them for later or require the user to point them out.
- Do not defer cleanup of code you just wrote to a future PR. Duplicated queries, awkward interfaces, and missing abstractions in newly written code should be addressed immediately — merging bad code and fixing it later costs more than getting it right now. Reserve "out of scope" for genuinely unrelated large-scale refactors, not for polish on your own changes.

## Language

Documentation, code comments, commit messages, and pull-request descriptions are written in English.
