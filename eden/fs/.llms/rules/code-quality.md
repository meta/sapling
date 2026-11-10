---
name: code-quality
metadata:
  oncalls: ['scm_client_infra']
  strict: true
---

# Code Quality

## Commits and Stacks

- Every commit must be self contained, passing tests and safe to land.
- Commits should be small. Do not combine logically separatable changes into a single commit.
- Prefer to move and/or refactor code in separate commits before the behavior change.
- When creating commits, run "sl debugcommitmessage", fill in the template, and commit using "sl commit --message MSG" or "sl commit --logfile FILE".
  - "Summary" is a concise description what changed (NOT a bullet list of every changed function), an explanation of "why" we are making the changes, followed by any material commentary about design, performance, tradeoffs, etc.
  - "Test Plan" contains a brief description of added tests with commands used to invoke tests and/or manual tests, including test output.
  - "Reviewers" is `#scm_client` for EdenFS and/or Sapling.

## New Features

- Must be gated by config flags. If risk is low, default config flag to enabled.
- Add log messages for unexpected errors or infrequent, interesting occurrences. Add counters to track efficacy, performance, and error rate.
- Ensure that existing tests run with the new feature enabled to maximize coverage.
- Ensure backwards compatibility in case the feature must be disabled, or code version rolled back.

## Code Comments

- No trivial comments such as "// Get the value\nobj->getValue();".
- Add concise comments for code with subtle interactions, performance sensitivity, critical correctness concerns, interesting tradeoffs, etc.

## Testing

- Unit tests should be added when useful - do not add tests with no value.
- Add minimal test cases. Do not add unnecessary variations or an unnecessary amount of test data.
- Integration tests should be added early in the stack, and expanded in subsequent commits.
- Performance optimizations should show before and after improvement in Test Plan, ideally using an existing or new benchmark.

### Crash workflow Python typing

- In `scripts/facebook/crash/`, annotate positional parameters (except `self` and `cls`), keyword-only parameters, and return values, including nested test helpers and mocks. Match the API being mocked; do not use `Any` merely to silence the checker. Generic forwarding `*args` and `**kwargs` are outside this missing-annotation check.
- Before submitting a diff, inspect added or changed function signatures for missing annotations and run `arc pyrefly check-owning-targets` on every changed Python file, including tests. A successful type check alone does not establish annotation completeness: unannotated functions can escape checking.

## Duplication

- Avoid duplicating significant amounts of code, instead using functions to share code.
- Do not duplicate subtle, correctness-critical, or performance sensitive code.

## Performance

- Avoid unnecessary memory allocations (e.g. prefer string views in C++ or borrowed values in Rust).
- Avoid O(n^2) time/space complexities, or add comment if unavoidable.
- Ensure memory use is bounded.
- Ensure operations over many files/directories/commits are parallelized.
- Avoid locks where atomics suffice. Avoid mutual exclusion (i.e. mutex or write lock) in the common case.

## Errors

- Do not ignore errors. If code must intentionally skips errors, they should be logged.
- Maintain error context as errors propagate through different components.
