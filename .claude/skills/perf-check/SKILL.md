---
name: perf-check
description: Use when a change could affect how fast Collate refreshes, diffs, scrolls or starts, or when asked to make Collate faster.
---

# Measuring Collate

Measure before changing anything, and again after, the same way. A change without both
numbers is a guess.

## The bench

```sh
python3 scripts/bench-repo.py /tmp/collate-bench     # once: a heavy review, about 416 files
COLLATE_BENCH_REPO=/tmp/collate-bench cargo test --release bench_refresh -- --ignored --nocapture
COLLATE_BENCH_REPO=/tmp/collate-bench cargo test --release heap_sizes -- --ignored --nocapture
```

`bench_refresh` prints medians of 7 runs per phase, in milliseconds: `git status`,
snapshot, diff cold, diff with one file changed, apply, first frame. Run it twice; treat
a difference under about 5% as noise.

`COLLATE_COLOUR=on|off cargo test --release colour_ab -- --ignored --nocapture` measures
scrolling a 60,000-line diff with and without syntax colours.

## Finding the cost

Build with symbols and sample the phase you care about:

```sh
CARGO_PROFILE_RELEASE_STRIP=false CARGO_PROFILE_RELEASE_DEBUG=1 cargo test --release --no-run
/usr/bin/sample <pid> 6 1
```

## What to keep in mind

- `git` and diffing run on the watcher thread. Nothing slow goes on the UI thread.
- A refresh rediffs only files whose content changed (`DiffCache`, `git::Memo`).
- Rune asks for every row's key when the row list changes, so key building scales with
  the number of rows. Keep `DiffState::key_of` free of `format!`.
- Rows are memoised. Anything that changes a row's look belongs in its `memo` version;
  anything that doesn't must stay out, or every row rebuilds.
- Don't reach for `unsafe`, a custom allocator or a new dependency before the simple fix
  is shown not to be enough.

## Reporting

Give before and after for each phase, the machine, and what got worse if anything did.
Don't commit sample output or benchmark dumps.
