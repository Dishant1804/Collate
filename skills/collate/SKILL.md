---
name: collate
description: Use when the reader has Collate (the git diff viewer for macOS and Linux) open, or asks to show, walk through or review a change in Collate - point them at a file or line, select lines, leave comments, switch views (staged, unstaged, branch, history), open a commit, or run any Collate command (stage, commit, fetch, push). Drives the running window through the `collate` MCP server's `collate_*` tools.
---

# Driving Collate

Collate is a git diff viewer the reader is looking at. The `collate` MCP server
(`collate --mcp`) reads and drives its windows. Everything you do happens on the reader's
screen, so move the way a colleague pointing at their screen would: one step at a time,
saying why.

## Start

1. `collate_state`. It returns the repository, the view, where the reader is (`place`), the
   files of the diff, the file in view, the selection, any dialog in front (`modal`) and the
   notice shown. Read it before acting and after anything that changes the view.
2. No window? `collate_launch` with the repository's folder. Several windows? Calls go to the
   one on your working folder; name another with `repo` (a folder) or `window` (a process id
   from `collate_windows`).

## Move

- `collate_show` `view`: `all` (everything uncommitted), `staged`, `unstaged`, `branch` (the
  branch against its base, as its pull request will read), `history` (the commit graph),
  `pulls`, `issues` (with `number`, that issue), `map`.
- `collate_open_commit` `rev`: a commit (`HEAD~2`, a tag) or a range `main..topic`; shows it in
  History.
- `collate_open_file` `path` [`line`, `side`]: scrolls to the file, or to the line and selects it.
- `collate_select` `path` `start` [`end`, `side`]: selects lines, as clicking their numbers
  does. Lines count on the new side unless `side` is `old` (use `old` for removed lines).
- Paths and files must be in the diff shown: `state.files` lists them. A file not there is in
  another view, or unchanged.

## Read

- `collate_diff` [`path`]: the shown diff as a unified patch. Prefer one file at a time.
- `collate_comments`: the reader's comments on lines, with their index.

## Act

- `collate_commands` lists every menu command: `id`, `title`, `key`, `enabled`, `checked`.
  `collate_run` `command` runs one by id, as choosing it in the menu does, and returns the
  state after. A disabled command is refused with the reason (often a dialog in front).
- Commands on lines act on the selection, else on the change in view: select first, then
  run `stage`, `unstage`, `discard`, `comment`, `copy-lines`, `add-to-chat`.
- Useful ids: `next-change`, `prev-change`, `next-file`, `prev-file`, `split`, `wrap`,
  `whitespace`, `collapse`, `expand`, `viewed`, `stage-file`, `unstage-file`, `stage-all`,
  `commit`, `amend`, `fetch`, `pull`, `push`, `new-branch`, `stash`, `blame`, `terminal`.
- On an open issue (`state.issue`): `issue-comment` (focuses the reader's comment box),
  `issue-edit`, `issue-close`, `issue-close-not-planned`, `issue-reopen`, `issue-copy-link`,
  `issue-on-github`; `new-issue` and `reload-issues` from the list. Ask before closing or
  reopening.
- `collate_comment` `path` `line` `body` leaves a comment under a line; `collate_delete_comment`
  removes one by index. Comments are the reader's review notes: add them when asked to review,
  and say how many you left.
- `collate_say` `text` shows the reader a notice. Use it to say what you just pointed at.
- `collate_dismiss` closes a dialog, palette or settings, as Escape does.

## Rules

- Ask the reader before `commit`, `push`, `pull`, `discard`, `discard-file`, `uncommit`,
  `undo`, `stash`, or anything else that changes the repository or the remote. Reading,
  moving, selecting and commenting need no asking.
- Discards and other risky commands open a confirmation in the window: leave it for the
  reader to answer. Don't dismiss it to get past it.
- Background work (fetch, push, commit hooks) finishes after `collate_run` returns: check
  `state.busy` and `state.notice` again.
- After `stage`, `commit` and the like, the window rereads the repository a moment later;
  read `collate_state` again before trusting counts.
- Collate shows the change; git still runs in your shell. For edits or history rewriting,
  use git and let Collate show the result.

## Walking a reader through a change

1. `collate_show` the view the change is in (`branch` for a pull request's worth of work,
   `all` for uncommitted work, `collate_open_commit` for a commit).
2. For each point: `collate_open_file` or `collate_select` the lines, then explain them in your
   reply (or with `collate_say` when short).
3. Leave review notes with `collate_comment`; the reader sends them on from the window.
