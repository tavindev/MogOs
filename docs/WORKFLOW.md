# Workflow

The loop every change follows. Agents live in `.claude/agents/`; the orchestrator (main thread) plans and dispatches.

| # | Step | Who |
| --- | --- | --- |
| 1 | Plan: the behavior, the test that proves it, the smallest change. Check the phase doc. | Orchestrator |
| 1b | Review the plan (phase-doc steps, done-whens, prescribed design) for simplicity and speed before any code is written. | `reviewer` |
| 2 | Write the failing test: QEMU boot test (`crates/e2e/tests/boot.rs`) first, crate public API next. | `implementor` |
| 3 | Implement the smallest diff that makes it pass. | `implementor` |
| 4 | `cargo fmt`, `cargo clippy` clean, `cargo test-host` green; before/after benchmark if a hot path changed (`docs/BENCHMARKS.md`). | `implementor` (or `test-runner`) |
| 5 | Review for correctness (incl. tests) and simplicity, given the task and changed files. | `reviewer` |
| 6 | Fix the findings; re-run step 4 and re-review if the fix is non-trivial. | `implementor` |
| 7 | Update the phase doc's "What was done" and any baseline in `docs/BENCHMARKS.md`. | `implementor` |

- A change is not done until step 5 has run and its findings are fixed or explicitly declined.
- For a larger diff, `/cleanup` (global skill) can apply simplification fixes after step 5; it does not replace the reviewer pass.
- A PostToolUse hook runs `rustfmt` on each edited `.rs` file; nothing heavier runs automatically.
