# Collate

A fast, lightweight git diff viewer for macOS, built on [Rune](https://github.com/anomalyco/rune).

Collate opens a repository, shows what changed beside a file tree, and refreshes as files
change. You review, stage, discard and commit without leaving the diff.

## What it does

- Unified or split diffs with syntax colours and word-level changes. Fourteen languages
  are built in; about forty more (C, C++, Java, Kotlin, Ruby, Prisma, ...) are fetched and
  built the first time a diff shows one, or from Settings > Languages.
- Stage, unstage or discard a file, a hunk, picked lines or single word changes.
- Moved blocks shown apart from new code, with a key to jump between the two ends.
- Views for all changes, staged, unstaged, a pull request preview against a base branch,
  and the commit graph.
- A map of the whole repository with the current changes on it.
- Search in the diff (match case, whole words, regular expressions), viewed marks that
  survive restarts, folding of large and low-signal files (lockfiles, bundles, generated code).
- Image diffs: a changed PNG, JPEG, SVG or PDF (its first page) shows before and after
  side by side.
- Conflicts: keep ours, theirs or both, for a whole file or one conflict at a time.
- Compare any two branches, tags or commits; blame and the history of a file.
- Fetch, pull, push and force push (with lease); switch, create, rename, delete, merge,
  rebase and stash branches; tags; amend; cherry-pick, revert and reset from the commit
  graph. A merge or rebase stopped on conflicts waits with Continue, Skip and Abort.
- Pull requests on GitHub, through the GitHub CLI: the open ones grouped by what they ask
  of you, and any one with its diff, checks, conversation and line threads. Approve,
  request changes, comment, suggest changes, reply and resolve; merge, update the branch,
  re-run failed checks, edit the title, reviewers and labels; open a new one. A
  notification says when someone asks for your review.
- Comments on lines, kept on your Mac, copied out as Markdown for a pull request.
- Go to a file or line (⌘P, ⌘L), and marks beside the scroll bar where the changes are.
- Keyboard first: press `?` in the app for the full list of keys.

## Requirements

- macOS 13 or newer.
- `git` on your `PATH`.
- For downloaded languages: Xcode's command line tools (`xcode-select --install`).
- A stable Rust toolchain to build it. It is developed on Rust 1.98.
- Optional: the GitHub CLI (`gh`, signed in with `gh auth login`), for pull requests and
  commit authors' pictures.

## Install

Build it from source:

```sh
git clone <this repository>
cd collate
scripts/install.sh
```

This builds a release binary, installs `Collate.app` into `/Applications` (or
`~/Applications` when that isn't writable) and puts a `collate` command in `~/.local/bin`.

```sh
collate            # open the repository in the current folder
collate ~/code/app # open that one
```

A build from source is signed ad hoc for the Mac that built it.

## Run from source

```sh
cargo run --release -- /path/to/a/repository
```

Without a path, Collate opens the project you used last, or offers to add one.

## Keys

| Key | Action |
|---|---|
| `j` `k` | Next / previous change |
| `]` `[` | Next / previous file |
| `a` `u` `x` | Stage, unstage, discard the selected lines, or the change at the top |
| `A` `U` `X` | The same for the whole file |
| `v` | Mark the file viewed |
| `c` | Comment on the selected line |
| `o` | Open the file in your editor |
| `⌘F` or `⌘K`, `n` `N` | Search this view (the diff, the Map's files, History's commits and branches), next / previous match |
| `⌥⌘C` `⌥⌘W` `⌥⌘R` | Search: match case, whole words, regular expression |
| `⌘P` `⌘L` | Go to a file (`file:line` for a line), go to a line |
| `m` | Jump to the other end of a move |
| `p` | Pick single word changes to stage |
| `z` `Z` | Fold / unfold every file |
| `s` | Unified / split |
| `w` `W` | Wrap long lines / hide whitespace changes |
| `1` to `5` | All changes, staged, unstaged, pull request preview, history |
| `⌘↩` | Commit what's staged |
| `⌘O` | Add a project |
| `r` | Read everything again |
| `?` | Every key |
| `⌘,` | Settings |

## What Collate stores and sends

Collate keeps its files under your home folder and nowhere else:

- `~/Library/Application Support/Collate/`: settings, the list of projects, viewed marks,
  comments.
- `~/Library/Caches/Collate/avatars/`: commit authors' pictures.

Fetch, pull and push talk to your remotes when you ask, through `git` and your own
credentials. Apart from those, the only network use is the pictures. For a project hosted on github.com, Collate asks
GitHub which account a commit belongs to (through `gh` when it is installed, else the
public API) and downloads that account's avatar. That request names the repository and a
commit hash. Nothing else leaves your machine, and there is no telemetry.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Agents working in this repository read
[AGENTS.md](AGENTS.md).

## License

MIT. See [LICENSE](LICENSE).
