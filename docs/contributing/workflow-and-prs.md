# Workflow, PRs, and TODOs

## Branch and Git Worktree Workflow

Start each independent task in a dedicated branch and Git worktree based on the
latest `origin/main`. Make and validate the task's changes in that worktree,
preserving unrelated work in other worktrees.

- Do not commit or push task changes directly to `main`.
- Push the task branch and open one PR per coherent unit of work against `main`.
- Follow the PR's CI checks through completion and fix failures before declaring
  it ready for review. Report any checks that remain pending or blocked explicitly.
- Hand off the PR URL with validation results and any limitations, and leave the
  PR open for review unless the user requests a merge.
- After completing a task, remove its local build artifacts, temporary files, and
  dedicated worktree once any needed outputs and changes are safely preserved.
  Leave shared build caches and worktrees for other tasks intact.

## Commit and Pull Request Conventions

- History uses `docs:`. Use concise, imperative subjects and appropriate prefixes
  such as `feat:` or `fix:` for implementation.
- Keep commits focused.
- PRs should explain the problem, resulting behavior, validation, and
  limitations. Link relevant issues and acceptance IDs; include screenshots for
  visible UI changes.

## Task Scope and TODO-to-Issue Format

Keep changes focused on the current task's objective. When you discover problems
or improvements outside that objective, leave an actionable TODO comment near
the relevant code and defer implementation to a follow-up task. Complete any
changes required for the current task to work correctly as part of the current
task.

The [TODO to Issue workflow](../../.github/workflows/todo-to-issue.yml) uses
[alstr/todo-to-issue-action](https://github.com/alstr/todo-to-issue-action) to
create GitHub issues from TODO comments added by pushes to `main` (including
merged PRs):

- Use the file's comment syntax with `TODO: <unique, specific title>`.
- Keep the complete title on the same physical line as `TODO:` (shorten a long
  title instead of wrapping it).
- Put the context and next steps in the comment lines immediately below the
  title, with one physical line per paragraph. The workflow turns each
  description line into a separate paragraph in the issue body.
- TODO titles and descriptions are exempt from line-length limits.
- Removing a TODO attempts to close its matching issue. Without an explicit
  `Issue URL:` comment, the action matches issues by title, so use unique
  titles and preserve any `Issue URL:` line when editing a tracked TODO.
- The workflow does not insert URLs or commit changes, ignores Markdown files,
  and processes only the pushed diff.

Example:

```rust
// TODO: Isolate Desktop ANSI parsing from the UI lock
// Publish a completed screen snapshot so large frames cannot block UI/input reads.
```

## GitHub Release Titles

Use the exact version tag as the GitHub Release title, such as `v0.1.2` or
`v0.2.0-rc.1`. Do not add `Corvid`, a product name, or any other prefix or
suffix.
