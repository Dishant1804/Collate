# Plan: make Collate the best diff viewer and manager

**Goal.** Collate should be the fastest way to read, judge and land a change on a Mac, whether a person or an agent wrote it.

**What "best" means, so we can test it**
- **Reading**: the diff gets the most screen and the least noise of any tool.
- **Trust**: no action can land code you haven't seen.
- **Loop**: review, fix and commit happen without leaving the window.
- **Speed**: published numbers on big repositories, measured on a release build.

**Not doing**: language servers, a file editor, Markdown or CSV viewers, Vim mode, a browser or remote mode, telemetry. These are an IDE's side; they would dilute Collate.

Sizes: S is a day or less, M is a few days, L is a week or more.
Status: ✅ done, 🔲 to do, ❔ undecided, ✖ dropped.

## Progress

| Phase | Done |
|---|---|
| 0 - Trust and room to grow | 3 of 3 |
| 1 - The best reader | 6 of 6 (1.7 dropped) |
| 2 - The agent loop | 6 of 6 |
| 3 - The best manager | 0 of 5 |
| 4 - Reach | 0 of 3 |

## Phase 0 - Trust and room to grow

| # | Item | Size | Status | Commit |
|---|---|---|---|---|
| 0.1 | Pin merge and review to the reviewed commit: `--match-head-commit` on merge, `commit_id` on review, and a "the PR moved, reread" message | S | ✅ | `db78598` |
| 0.2 | Split `src/main.rs` before adding features: view tests, settings, History, comments, keys and the watcher into their own files, no behaviour change | M | ✅ | `8318dc0`..`8ff11d4`, `2f2aab9` |
| 0.3 | A fixed benchmark set (Linux, Kubernetes, a 400,000-line file) with start and scroll times | M | ✅ | `c35dcff` |

## Phase 1 - The best reader

| # | Item | Size | Status | Commit |
|---|---|---|---|---|
| 1.1 | Progress: "File 3 of 31 - 12 viewed" | S | ✅ | `6a07dab` |
| 1.2 | Unpushed commits, from where the branch left its upstream, with the count on Push | M | ✅ | `33ca03e` |
| 1.3 | Compare any two refs | M | ✅ | `2415915` (was already there) |
| 1.4 | PR: changes since my last review, and commit-by-commit review | M | ✅ | `a226181` |
| 1.5 | PR: what a force-push changed (`git range-diff`) | M | ✅ | `a7ff4b4` |
| 1.6 | Review order: source first, tests next, generated and lock files last | S | ✅ | `57f39ca` |
| 1.7 | Syntax-aware diff: ignore pure reformatting, match moved blocks by structure | L | ✖ | Dropped |

## Phase 2 - The agent loop

Collate stays network-free: it hands work to the agent CLI the user already has (Claude Code, Codex, Gemini or Cursor, picked in Settings), in the ⌘J terminal.

| # | Item | Size | Status | Commit |
|---|---|---|---|---|
| 2.1 | Copy reference (`path#L10-L25`) and copy for agent (the lines as a diff), keys `y` and `Y` | S | ✅ | `a6e0ad6` |
| 2.2 | Send the review comments to the agent | M | ✅ | `587d14d` |
| 2.3 | Review checklist: saved questions answered by the agent about the change in view (⋯ → Ask Agent) | M | ✅ | `8f584e9` |
| 2.4 | Commit message from the staged diff, with an instructions file; fills the box, never commits | S | ✅ | `2941508` |
| 2.5 | Checkpoints: the working tree saved before an agent starts or on demand, "changes since", restore; HEAD and the index never move | L | ✅ | `ca959d7` |
| 2.6 | The agent settles a diverged pull, a stopped merge or rebase, or a PR's conflicts; never pushes | M | ✅ | `8733895` |

## Phase 3 - The best manager

| # | Item | Notes | Size | Status |
|---|---|---|---|---|
| 3.1 | PRs as worktrees: open a PR without touching your branch; list and remove worktrees | Replaces checkout as the default | M | 🔲 |
| 3.2 | Interactive rebase in History: reorder, squash, fixup, reword, drop | The main gap against Fork and Tower | L | 🔲 |
| 3.3 | Undo the last operation from the reflog | Commit, reset, rebase, discard (discard already backs up) | M | 🔲 |
| 3.4 | Three-pane conflict view: base, ours, theirs | Taking a side exists; this adds the view | L | 🔲 |
| 3.5 | Fixup into an earlier commit from selected lines | Pairs with line staging | M | 🔲 |

## Phase 4 - Reach

| # | Item | Size | Status |
|---|---|---|---|
| 4.1 | GitLab merge requests behind a forge trait (via `glab`, as GitHub goes via `gh`) | L | ❔ |
| 4.2 | Install without Node: Homebrew cask, signed and notarised app | M | 🔲 |
| 4.3 | Public benchmarks page and a short demo, from 0.3 | S | 🔲 |

## Decisions

- Handing work to a local agent CLI is in scope.
- 1.7 (syntax-aware diff) is dropped.
- GitLab (4.1) is undecided.

## How each item ships

1. Tests on a throwaway repository.
2. `cargo build`, `cargo clippy --all-targets -- -D warnings` and `cargo test` pass.
3. Anything touching refresh or scroll goes through perf-check first.
4. The staged diff is checked for private paths and secrets, then committed.
5. The app is reinstalled with `scripts/install.sh`.
6. The item's status and commit are updated here.
