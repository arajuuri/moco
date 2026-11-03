# Project practices

How moco's issues are classified, prioritized, sized and tracked. Work lives in
[arajuuri/moco](https://github.com/arajuuri/moco) issues and the org project
[Moco](https://github.com/orgs/arajuuri/projects/1). Code conventions and commands are in
[AGENTS.md](AGENTS.md).

**Principle: use GitHub's structured fields, not labels or title prefixes.** Type, priority,
status, size and estimate each have a field. Don't encode them in labels or in the title.

## Issue types

Set the issue **Type** (an organization-level field). Every issue has exactly one.

| Type | Use for | Examples |
|---|---|---|
| **Bug** | Existing code does the wrong thing: wrong results, crashes, hangs, data loss, security holes, silently swallowed errors. | #6 lost updates, #9 panic leaves session claimed, #1 credential redirect |
| **Feature** | A new capability that did not exist. | #25 spending limits, #26 check hooks, #28 shell classifier |
| **Enhancement** | An improvement to something that already works: performance, ergonomics, robustness that is not a defect. | #14 snapshot cost, #22 edit-tool robustness, #23 `sessions export` |
| **Task** | Work with no user-visible behavior change: cleanup, refactor, tooling, CI. | #20 duplicated transport code, #21 CI, #30 doc comment cleanup |

Rules of thumb:

- Ask "was it broken before?" Yes is a Bug, no and it is new is a Feature, and no and it is an
  improvement is an Enhancement.
- An unbounded-memory or resource-exhaustion failure is a Bug, not performance work.
- Types are defined for the whole `arajuuri` organization (Organization settings → Planning →
  Issue types). Add a type there if a genuinely new category appears. The Enhancement
  description is "Improvement to an existing feature or behavior."

## Priority

Set the **Priority** issue field: **Urgent**, **High**, **Medium** or **Low**.

- **High**: data loss or corruption, security problems, crashes or stuck sessions on common
  paths.
- **Medium**: real defects and valuable work with a workaround or limited blast radius.
- **Low**: polish, cleanup, rare edge cases, efficiency work that nobody feels yet.
- **Urgent**: reserved for drop-everything problems. Nothing has used it so far.

These definitions describe how the existing issues were triaged. They are guidance, not a
formula.

## Labels

Labels describe the **area**, not the type or priority. An issue can carry several.

| Label | Area |
|---|---|
| `security` | Vulnerabilities and hardening |
| `stability` | Crashes, races, resource handling |
| `performance` | Speed and memory efficiency |
| `error-handling` | Failure paths, ignored or mishandled errors |
| `code-quality` | Maintainability, duplication, lint |

GitHub's default labels (`documentation`, `accessibility`, `good first issue`, `help wanted`,
`question`, `duplicate`, `invalid`, `wontfix`) stay for their usual meaning.

Do not recreate `bug`, `enhancement` or `priority: *`. Type and Priority replaced them.

## Titles and bodies

- Titles are plain, descriptive sentences. **No `[High]`, `[Bug]`, `[Stability]` prefixes**;
  that information is in the fields.
- Bug reports should state where (files/functions), the problem, and a concrete failure
  scenario. Issues found while reviewing unmerged work say so ("Found in review of ... #N").
- Some older issue bodies still start with a `**Type:** ... **Priority:** ...` line. The fields
  are authoritative. Ignore or delete those lines.
- Link related issues with `#N` and say when one should land first.

## Status (project board)

The **Status** field on the Moco project:

| Status | Meaning |
|---|---|
| **Backlog** | Known, not yet triaged or scheduled. Default for new issues. |
| **Ready** | Triaged: type, priority, size and estimate are set, and nothing blocks starting. |
| **In progress** | Someone is actively working on it. |
| **In review** | A pull request is open and awaiting review. |
| **Done** | Merged or otherwise finished. Close the issue. |

Move an issue to In progress when work starts, not when it is merely planned.

## Pull requests

- One PR per issue. Put `Closes #N` in the description. If the work has no issue (a bug found
  while using moco, say), open the PR without one and say so in the description.
- Do not add pull requests to the project board, and do not set Status, Size, Estimate or
  Priority on them. The issue carries those fields: when the PR is open, move the issue to
  **In review**. A PR with no issue is not tracked on the board.
- Say in the PR description which checks were run and on which machine (Mojave and/or Linux),
  and which were not.

## Size and estimate

Two fields on the project, both set on every issue that still has work left.

**Size** is the relative effort, judged from the code touched, the design decisions involved
and the tests needed:

| Size | Meaning | Estimate (hours) |
|---|---|---|
| XS | One-spot change | 1 |
| S | A small, well-understood fix | 3 |
| M | Several files or a design decision | 8 |
| L | Cross-cutting, or starts with investigation | 20 |
| XL | Too big. Split it before starting. | none yet |

**Estimate** is a number of **hours of focused work, including tests and review**. Start from the
table, then adjust per issue: concurrency work gets a little more for careful tests (#7, #8,
#13 at 10), well-trodden fixes a little less (#1, #3, #17, #21 at 6), and an issue that begins
with an investigation more (#22 at 24).

- Estimates are guesses from reading the issue. For L items expect to be off by a factor of
  two.
- Estimate remaining work only. Leave it empty for Done and In progress items. Re-estimate when
  scope changes.
- The Backlog estimates currently add up to 232 hours.

## Working with the fields from the command line

Needs `gh auth refresh -s project` once for project access.

```bash
# Issue type (names: Bug, Feature, Enhancement, Task)
gh api -X PATCH repos/arajuuri/moco/issues/N -f type=Bug

# Priority issue field. The body must be JSON: the field id has to be an integer.
# Look ids up with: gh api orgs/arajuuri/issue-fields
# (`gh project field-list` shows Priority with no options: it is an issue field, not a
# project single-select field.)
echo '{"issue_field_values":[{"field_id":49312686,"value":"High"}]}' \
  | gh api -X POST repos/arajuuri/moco/issues/N/issue-field-values --input -

# Project fields (Status, Size, Estimate)
gh project field-list 1 --owner arajuuri --format json
gh project item-list 1 --owner arajuuri --limit 100 --format json
gh project item-edit --id ITEM_ID --project-id PROJECT_ID --field-id FIELD_ID \
  --single-select-option-id OPTION_ID        # Status and Size
gh project item-edit ... --number 8          # Estimate (hours)
```

Closed issues are not added to the project automatically. Add one with
`gh project item-add 1 --owner arajuuri --url ISSUE_URL --format json`; the `id` in the
output is the ITEM_ID for `item-edit`. The project id is `PVT_kwDOFOudCc4BpvtY`.

After a bulk change, read the values back before you trust them.
