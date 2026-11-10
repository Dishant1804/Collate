# Contributing to Collate

Thanks for helping. Small, focused pull requests are the easiest to review and land.

## Set up

You need macOS 13 or newer, `git`, and a stable Rust toolchain (Collate is developed on
Rust 1.98).

```sh
git clone <your fork>
cd collate
cargo run -- /path/to/a/repository   # debug build, opens that repository
```

The first build fetches Rune from GitHub and takes a few minutes.

## Before you open a pull request

Run all three. CI runs the same commands.

```sh
cargo build
cargo clippy --all-targets -- -D warnings
cargo test
```

The tests need nothing set up: the view tests make their own small repository in the
temp folder. To run them against a repository of your own, set `COLLATE_TEST_REPO` to one
with many changed files under `src/`.

## Code style

- **Don't run `cargo fmt`.** The code is laid out by hand and `rustfmt.toml` turns the
  formatter off. Match the file you are in: long lines, one builder chain per line.
- Comments say why, not what. Every function that isn't obvious has a short doc comment.
- Text in the app uses ` - ` as a separator, never `·`.
- No new dependencies unless there is no reasonable way without one. Say why in the PR.
- Errors reach the user (`Collate::say`) or the caller. Don't swallow them.
- Delete code you make unused. The build has no `dead_code` allowances.

## Where things are

| Path | What |
|---|---|
| `src/main.rs` | App state (`Collate`) and the views outside the diff without a file of their own |
| `src/keys.rs` | What each key does |
| `src/watcher.rs` | The watcher thread: it notices changes, rereads the repository and diffs the files |
| `src/history_view.rs` | History: the commit graph, the commit card and header |
| `src/settings_view.rs` | The settings panel |
| `src/comments.rs` | The reader's comments on lines |
| `src/view_tests.rs` | The view tests |
| `src/git.rs` | Everything that runs `git`: status, snapshots, staging, discard, commit |
| `src/patch.rs` | Building the patches that stage or discard hunks, lines and words |
| `src/diff/` | The diff itself: model and parsing, Myers diff, row state, drawing |
| `src/graph.rs` | The commit graph |
| `src/map.rs`, `src/map_view.rs` | The repository map |
| `src/settings.rs` | Settings and their file |
| `src/avatars.rs` | Commit authors' pictures |
| `scripts/install.sh` | Builds and installs `Collate.app` |

## UI changes

Sizes and spacing come from a short set of constants (`EDGE`, `PANEL_INSET`,
`COLUMN_TOP`, `GHOST_PAD`) and from the kit (`k.ty.*` for type, `k.c.*` for colour,
`k.radius.*`). Use those before a new number. Several tests check that the three columns
line up; if one fails after your change, the layout moved.

Attach a before and after screenshot to any pull request that changes how something
looks.

## Performance changes

Measure before and after, on a release build, and put the numbers in the pull request:

```sh
python3 scripts/bench-repo.py /tmp/collate-bench
COLLATE_BENCH_REPO=/tmp/collate-bench cargo test --release bench_refresh -- --ignored --nocapture
```

## Reporting bugs

Open an issue with what you did, what you expected, what happened, your macOS version,
and the commit you built. For a security problem, see [SECURITY.md](SECURITY.md).

## License

By contributing you agree that your work is released under the MIT License.
