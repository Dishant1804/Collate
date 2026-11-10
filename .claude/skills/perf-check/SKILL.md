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

## The fixed set

For start, refresh and scroll on real trees, `scripts/bench-set.py <folder>` makes four
repositories with seeded changes: `heavy` (the one above), `linux` (v6.12), `kubernetes`
(v1.31.0) and `bigfile` (a 400,000-line file edited every 50 lines and a new 400,000-line
file). Clones are shallow, about 2.2 GB in all; run it again to put the changes back.

```sh
for r in heavy linux kubernetes bigfile; do
    for b in bench_refresh bench_start_scroll; do
        COLLATE_BENCH_REPO=<folder>/$r cargo test --release $b -- --ignored --nocapture
    done
done
```

`bench_start_scroll` times each step from opening the repository to the first frame, then
scrolls with every file unfolded. Baseline, Apple M5 Pro, 2026-11-11 (medians, ms):

| Repository | Files, diff lines | git status | Start | Refresh, 1 file edited | Scroll p99 reading / flicking |
|---|---|---|---|---|---|
| heavy | 416, 95,280 | 9 | 108 | 8 + 4 + 2 | 9.2 / 8.2 |
| linux | 323, 39,407 | 197 | 278 | 11 + 2 + 1 | 9.0 / 11.5 |
| kubernetes | 298, 26,926 | 129 | 183 | 12 + 1 + 1 | 9.0 / 8.4 |
| bigfile | 2, 464,000 | 40 | 325 | 7 + 56 + 5 | 7.8 / 6.7 |

Refresh is snapshot + diff + apply from `bench_refresh`; every refresh also pays one
`git status`, which dominates on the big trees. Start in `bigfile` is mostly its first
frame (90 ms) and the cold diff (159 ms). Files over 8 MB (`git::MAX_BYTES`) aren't diffed at
all, so a 400,000-line file of ordinary width shows as too large.

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
