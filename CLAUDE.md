# connor

A terminal text editor for working alongside a coding agent — README.md explains in more detail. Priorities, in order: fast, ergonomic, robust, simple.

**Scope test:** a feature belongs if it serves reading, reviewing and correcting code in a terminal, or plain everyday editing — and it must be usable without learning anything first. Overlapping a dedicated tool is fine when the point is having it in-editor; growing into that tool is not.

## Conventions

- The code stays simple; nothing joins the codebase without earning its place.
- rustfmt defaults; clippy warnings are errors.
- The single-threaded core owns all state; background work stays off the render path, and nothing blocks a frame.
- Render only in response to an event — block waiting for input, never poll or redraw continuously — and never heap-allocate on the render path.
- Logic that runs without a terminal gets unit tests; terminal behaviour is verified by driving the editor in a real terminal — look and feel is the human's call.
- connor stays a single self-contained binary: copy it to another machine and it runs — no runtime dependencies, no install step.
- Comments only where their absence would cause a wrong decision; rationale lives in commits and issues.
- If the right tool or library isn't available, ask the human — don't quietly pick a worse one.
- If an instruction or issue seems wrong, or a better approach exists, say so before implementing — pushback beats literal compliance.

## Workflow

- Tasks are GitHub issues: the title states the outcome, the body is a short scope paragraph plus `Done when:` bullets.
- Each issue gets its own branch (`N-short-slug`) and PR (`Closes #N`); the human always performs the squash-merge.
- Commits: plain imperative subject ≤ 72 chars, capitalised, no `type:` prefix; the body says why.
- No agent attribution anywhere — no `Co-Authored-By`, no generated-by footers, no session trailers.
