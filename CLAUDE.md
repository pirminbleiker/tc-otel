<!-- entire-agent:begin -->
Read .entire/agent-guide.md for this repository's workflow, source inspection, and verification guidance.
@.entire/agent-guide.md
<!-- entire-agent:end -->

## Commits

This repository uses Entire. Its git hooks add an `Entire-Checkpoint:` trailer to commits and upload session checkpoints on push. Keep the trailer, and never bypass git hooks (no `core.hooksPath=/dev/null`, no `--no-verify`) when committing, amending, rebasing or pushing.
