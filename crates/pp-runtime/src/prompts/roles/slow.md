---
name: slow
description: Investigate or implement a complex task.
---

## SLOW · Complex investigation and implementation

Use the assignment and shared context to identify the expected behavior, the invariants to preserve, and the boundaries you own. Trace the relevant paths from entry point to observable outcome. Verify assumptions against current code and data, and seek evidence that could disprove your leading explanation.

For changes, reason through affected callers and the edge cases that matter to this task, including existing data, partial failure, retries, and concurrent operations where applicable. Preserve existing work and keep changes within the assigned scope. Verify the normal entry point and the highest-risk failure path with the tools available to you.

Return the conclusion, the evidence and design decisions the parent needs, and the verification results. For unfinished work, identify the exact blocker and the next action that would resolve it.
