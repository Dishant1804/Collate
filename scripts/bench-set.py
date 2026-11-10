#!/usr/bin/env python3
"""Makes the fixed set of repositories Collate's numbers are recorded on.

    python3 scripts/bench-set.py /tmp/collate-benches
    for r in heavy linux kubernetes bigfile; do
        for b in bench_refresh bench_start_scroll; do
            COLLATE_BENCH_REPO=/tmp/collate-benches/$r cargo test --release $b -- --ignored --nocapture
        done
    done

- heavy: a review of 416 changed files (scripts/bench-repo.py).
- linux: Linux v6.12, about 87,000 files, with about 300 of them changed.
- kubernetes: Kubernetes v1.31.0, a large monorepo with vendored code, about 300 files changed.
- bigfile: a 400,000-line file edited every 50 lines, and a new 400,000-line file. Their lines
  are short so each stays under the 8 MB Collate diffs (`git::MAX_BYTES`).

Linux and Kubernetes are shallow clones at a fixed tag, and every change comes from a fixed
seed, so two machines or two runs read the same repositories. Run it again to put back a
repository whose changes were lost; the clones are kept.
"""
import os
import random
import subprocess
import sys

CLONES = {
    "linux": ("https://github.com/torvalds/linux.git", "v6.12"),
    "kubernetes": ("https://github.com/kubernetes/kubernetes.git", "v1.31.0"),
}
CHANGED = 300


def git(d, *args, out=False):
    r = subprocess.run(["git", "-C", d, *args], check=True, stdout=subprocess.PIPE if out else subprocess.DEVNULL)
    return r.stdout.decode() if out else None


def edit(lines, r):
    """Scattered edits, insertions and deletions, as a review of a real change has."""
    lines = list(lines)
    for _ in range(r.randint(2, 30)):
        if not lines:
            break
        at = r.randrange(len(lines))
        c = r.random()
        if c < 0.5:
            lines[at] = lines[at] + " // bench"
        elif c < 0.8:
            lines[at:at] = [f"// bench line {r.randint(0, 99999)}" for _ in range(r.randint(1, 12))]
        else:
            del lines[at:at + r.randint(1, 8)]
    return lines


def change_clone(d):
    """Puts back the clone's tree, then changes `CHANGED` of its text files the same way each time."""
    git(d, "reset", "-q", "--hard")
    git(d, "clean", "-qfd")
    files = sorted(p for p in git(d, "ls-files", "-z", out=True).split("\0") if p)
    r = random.Random(42)
    for k, path in enumerate(r.sample(files, CHANGED)):
        full = os.path.join(d, path)
        try:
            with open(full, encoding="utf-8") as f:
                lines = f.read().split("\n")
        except (UnicodeDecodeError, IsADirectoryError, FileNotFoundError):
            continue
        if k % 40 == 0:
            os.remove(full)
            continue
        with open(full, "w", encoding="utf-8") as f:
            f.write("\n".join(edit(lines, random.Random(k))))
    for i in range(10):
        with open(os.path.join(d, f"bench_new_{i}.txt"), "w") as f:
            f.write("\n".join(f"new line {j} of {i}" for j in range(400)) + "\n")


def bigfile(d):
    if not os.path.exists(d):
        os.makedirs(d)
        git(d, "init", "-q", "-b", "main")
        for key, value in [("user.email", "bench@example.invalid"), ("user.name", "bench"), ("commit.gpgsign", "false")]:
            git(d, "config", key, value)
        with open(f"{d}/big.ts", "w") as f:
            f.write("".join(f"let v{i} = {i % 97};\n" for i in range(400_000)))
        with open(f"{d}/README.md", "w") as f:
            f.write("# bench\n")
        git(d, "add", "-A")
        git(d, "commit", "-qm", "init")
    git(d, "reset", "-q", "--hard")
    git(d, "clean", "-qfd")
    with open(f"{d}/big.ts") as f:
        lines = f.read().split("\n")
    for i in range(25, len(lines), 50):
        lines[i] = lines[i].replace("let", "var")
    with open(f"{d}/big.ts", "w") as f:
        f.write("\n".join(lines))
    with open(f"{d}/added.ts", "w") as f:
        f.write("".join(f"f{i}({i % 13});\n" for i in range(400_000)))


def main():
    if len(sys.argv) != 2:
        sys.exit("usage: bench-set.py <folder>")
    root = sys.argv[1]
    os.makedirs(root, exist_ok=True)
    heavy = os.path.join(root, "heavy")
    if not os.path.exists(heavy):
        subprocess.run([sys.executable, os.path.join(os.path.dirname(__file__), "bench-repo.py"), heavy], check=True)
    for name, (url, tag) in CLONES.items():
        d = os.path.join(root, name)
        if not os.path.exists(d):
            subprocess.run(["git", "clone", "-q", "--depth", "1", "--branch", tag, url, d], check=True)
        change_clone(d)
    bigfile(os.path.join(root, "bigfile"))
    print(f"made {root}: heavy, linux, kubernetes, bigfile")


if __name__ == "__main__":
    main()
