# Collate

A fast, lightweight git diff viewer for macOS, built on [Rune](https://github.com/anomalyco/rune).

Collate shows what changed in a git repository beside a file tree and refreshes as you edit.
Review, stage, discard and commit without leaving the diff; review GitHub pull requests too.

## Install

```sh
npx collate-app@latest
```

That is the whole install. It checks your Mac, installs anything missing, puts Collate in
Applications and adds a `collate` command for the terminal. Run it again to update.

You need macOS 13 or newer and [Node.js](https://nodejs.org) 18 or newer (for `npx`). If
git is missing, the installer adds Xcode's command line tools; finish the dialog it opens.

- `npx collate-app@latest --build` builds the latest source instead of downloading the
  newest release. It installs Rust if you don't have it.
- `npx collate-app@latest --uninstall` removes the app and the command.

Then open Collate from Launchpad, or run `collate` in a repository.

Optional: the GitHub CLI (`brew install gh`, then `gh auth login`) for pull requests and
commit authors' pictures.

## Build from source

For working on Collate itself, with Rust installed:

```sh
git clone https://github.com/Dishant1804/Collate.git
cd Collate
cargo run --release -- /path/to/a/repository   # run without installing
scripts/install.sh                             # or install it
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
credentials. Apart from those, the only network use is the pictures and the grammars for
extra languages. For a project hosted on github.com, Collate asks GitHub which account a commit belongs to (through `gh` when it is installed, else the
public API) and downloads that account's avatar. That request names the repository and a
commit hash. Nothing else leaves your machine, and there is no telemetry.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Agents working in this repository read
[AGENTS.md](AGENTS.md).

## License

MIT. See [LICENSE](LICENSE).
