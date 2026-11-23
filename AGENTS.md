# Collate

A git diff viewer for macOS and Linux, in Rust, on the Rune UI framework. One binary crate.
Linux code is behind `cfg(target_os = ...)`, so the Mac build carries none of it.

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
  change to it with `npm publish` there after bumping its `package.json` version. The
  workflow also sets the new version in the Homebrew cask (`Dishant1804/homebrew-tap`,
  `Casks/collate.rb`), pushing with the deploy key in the `HOMEBREW_TAP_KEY` secret.
  Before it, a job per architecture on Ubuntu 22.04 runs `scripts/package-linux.sh`, whose
  tarballs join the release and its `SHA256SUMS`.
- Linux: CI builds and tests on Ubuntu too. To check Linux from a Mac, build in a Linux
  container with `libxkbcommon-dev libwayland-dev libx11-dev libxcb1-dev libfontconfig-dev
  pkg-config` (and `poppler-utils` for the PDF test).

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
- `src/pulls.rs`: pull requests and their overview. `src/issues.rs`, `src/github_issues.rs`: issues,
  their overview and dialogs, and their `gh` calls. `src/body.rs`: comment bodies, their HTML as
  Markdown and `<details>` as folds.
- `src/git.rs`: every `git` call. `src/patch.rs`: patches for staging hunks, lines, words.
- `src/diff/`: `model.rs` (files, hunks, parsing), `myers.rs` (line and word diff),
  `state.rs` (rows, folds, selection), `view.rs` (drawing).
- `src/graph.rs`: commit graph. `src/map.rs`, `src/map_view.rs`: repository map.
- `src/terminal.rs`: the terminal drawer (⌘J), shells on `alacritty_terminal`.
- `src/update.rs`: checks GitHub for a newer release or commit and installs it over the app.
- `src/control.rs`: agents drive a window over its socket (`~/Library/Application Support/Collate/
  control/<pid>.sock`; `$XDG_RUNTIME_DIR/collate/` on Linux), and `collate --mcp` serves those sockets as MCP tools. A new command in
  `src/commands.rs` reaches agents by its id; a new kind of request goes in `handle` and `TOOLS`.
  `skills/collate/SKILL.md` teaches agents the tools; `scripts/install-agent.sh` installs both.
- `src/dirs.rs`: where files live (Library on macOS, XDG on Linux). `src/desktop.rs`: opening
  links and files, notifications, new windows (`open` and AppleScript, or `xdg-open` and
  `notify-send`). Put a new platform difference in one of these, not at the call site.
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
