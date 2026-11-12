# Collate

A git diff viewer for macOS, in Rust, on the Rune UI framework. One binary crate.

## Commands

- `cargo build`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test` (needs nothing set up; view tests build their own repository in the temp folder)
- `cargo run -- <repo>` to look at a change; `scripts/install.sh` installs the app.
- Releases: bump `version` in `Cargo.toml`, then push a tag `v<version>`. The `Release`
  workflow runs `scripts/package.sh` (universal app zip and disk image, binary tarball, all
  signed with the Developer ID in the repository's secrets and notarized) and publishes the
  release with notes from `scripts/release-notes.sh`. `npx collate-app@latest` installs the
  newest release, so a new tag reaches every user. The installer lives in `npm/`; publish a
  change to it with `npm publish` there after bumping its `package.json` version.

Run all three checks before saying a change is done. Never run `cargo fmt`: the code is
laid out by hand and `rustfmt.toml` disables the formatter.

After a change to the app passes the checks, reinstall it so the installed app is the new
build: quit the running one (`osascript -e 'quit app "Collate"'`; the installer refuses
while it runs), run `scripts/install.sh`, which builds a release and replaces the old
`Collate.app`, then reopen it (`open -a Collate`) so the reader doesn't have to. Say if the
install failed; a `cargo build` alone doesn't update the app.

## Layout

- `src/main.rs`: app state (`Collate`) and the views outside the diff that have no file of
  their own. `src/keys.rs`: keys. `src/commands.rs`: every command, which the menu bar is
  built from; add a new action there too. `src/watcher.rs`: the watcher thread and `prepare`.
- `src/history_view.rs`, `src/settings_view.rs`: History's graph and commit card, the settings
  panel. `src/comments.rs`: the reader's comments. `src/view_tests.rs`: the view tests.
- `src/git.rs`: every `git` call. `src/patch.rs`: patches for staging hunks, lines, words.
- `src/diff/`: `model.rs` (files, hunks, parsing), `myers.rs` (line and word diff),
  `state.rs` (rows, folds, selection), `view.rs` (drawing).
- `src/graph.rs`: commit graph. `src/map.rs`, `src/map_view.rs`: repository map.
- `src/terminal.rs`: the terminal drawer (⌘J), shells on `alacritty_terminal`.
- `src/update.rs`: checks GitHub for a newer release or commit and installs it over the app.
- `src/settings.rs`, `src/avatars.rs`, `src/nav.rs`.

Rune's source is the reference for its API: `~/.cargo/git/checkouts/rune-*/<rev>/crates/`
(`rune` for layout and runtime, `rune-kit` for components). Read it instead of guessing.

## How a refresh flows

The watcher thread runs `git status`, fingerprints it, takes a snapshot (`Repo::snapshot_with`),
diffs the files in parallel (`prepare`), and hands the result to the UI thread
(`Collate::apply`), which sets the diff state and rebuilds its rows. Keep `git` and diffing
off the UI thread.

## Rules

- Small, reviewable changes. Match the file you are in: naming, comment density, long lines.
- Comments say why. Keep doc comments on non-obvious functions.
- No new dependencies without a stated reason.
- Don't hide errors or fall back silently. Show them with `Collate::say` or return them.
- Delete what you make unused; don't add `#[allow(dead_code)]`.
- App text separates with ` - `, never `·`.
- UI: use the shared constants (`EDGE`, `PANEL_INSET`, `COLUMN_TOP`, `GHOST_PAD`) and the
  kit's tokens (`k.ty.*`, `k.c.*`, `k.radius.*`) before a new number. See
  `.claude/skills/ui-change`.
- Performance: measure first, on a release build. See `.claude/skills/perf-check`.
- Rows of the diff are virtual and memoised. A row's key must stay stable for the same
  line, and whatever changes a row's look must be in its `memo` version.
- Tests that touch a repository make a throwaway one in the temp folder. Only
  `the_review_loop_stages_discards_and_commits` may set the shared `REPO`.

## This repository is public

Before committing, check the staged diff for home-folder paths, tokens, machine names
and private links: `git diff --cached | grep -nE '/Users/|/private/var/|token|secret'`.
Write repo-relative paths. Don't commit logs, profiles or benchmark dumps.
