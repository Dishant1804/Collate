# Collate

A fast, lightweight git diff viewer for macOS, built on [Rune](https://github.com/anomalyco/rune).

Collate shows what changed in a git repository beside a file tree and refreshes as you edit.
Review, stage, discard and commit without leaving the diff; review GitHub pull requests too.

## Install

Collate is built from source. It isn't signed with an Apple Developer ID yet, so there is
no download to drag into Applications; building it on your Mac takes a few minutes.

### 1. Get the tools (once)

You need macOS 13 or newer.

```sh
xcode-select --install                                          # git, a C compiler and the linker
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh  # Rust (pick the default install)
source "$HOME/.cargo/env"                                       # or open a new terminal
```

Skip either line if `git --version` or `cargo --version` already works.

### 2. Build and install

```sh
git clone https://github.com/Dishant1804/Collate.git
cd Collate
scripts/install.sh
```

The first build downloads its dependencies and takes a few minutes. When it finishes,
`Collate.app` is in `/Applications` (or `~/Applications` when that isn't writable) and a
`collate` command is in `~/.local/bin`.

### 3. Open it

Open Collate from Launchpad or Spotlight, or from a terminal:

```sh
collate            # the repository in the current folder
collate ~/code/app # that one
```

If the shell says `collate: command not found`, add `~/.local/bin` to your `PATH`:

```sh
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc && source ~/.zshrc
```

### Update

```sh
cd Collate
git pull
osascript -e 'quit app "Collate"'   # the installer won't replace a running app
scripts/install.sh
```

### Uninstall

```sh
rm -rf /Applications/Collate.app ~/Applications/Collate.app ~/.local/bin/collate
rm -rf ~/Library/Application\ Support/Collate ~/Library/Caches/Collate   # settings and caches
```

### Optional

- The GitHub CLI (`brew install gh`, then `gh auth login`) for pull requests and commit
  authors' pictures.
- Extra languages (C, C++, Java, Ruby and about forty more) are compiled on first use with
  the tools from step 1.

### Install with an AI agent

Paste this into Claude Code, Codex, Cursor or any coding agent that can run commands:

```text
Install Collate, a git diff viewer for macOS, from https://github.com/Dishant1804/Collate.
1. Check this is macOS 13 or newer (`sw_vers -productVersion`). Stop and tell me if not.
2. If `xcode-select -p` fails, run `xcode-select --install`, then wait and ask me to
   finish the installer dialog before going on.
3. If `cargo --version` fails, install Rust with
   `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y`, then
   `source "$HOME/.cargo/env"`.
4. Clone the repository into ~/Collate (or `git pull` there if it already exists).
5. If Collate is running, quit it: `osascript -e 'quit app "Collate"'`.
6. In the clone, run `scripts/install.sh` and wait for "Installed ...". Show me any error
   in full instead of working around it.
7. If `~/.local/bin` isn't on my PATH, add it to ~/.zshrc.
8. Run `open -a Collate` and tell me where the app and the `collate` command were installed.
```

## Run from source

For working on Collate itself, without installing:

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
credentials. Apart from those, the only network use is the pictures and the grammars for
extra languages. For a project hosted on github.com, Collate asks GitHub which account a commit belongs to (through `gh` when it is installed, else the
public API) and downloads that account's avatar. That request names the repository and a
commit hash. Nothing else leaves your machine, and there is no telemetry.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Agents working in this repository read
[AGENTS.md](AGENTS.md).

## License

MIT. See [LICENSE](LICENSE).
