#!/usr/bin/env python3
"""Makes a repository that looks like a heavy review, for the benches.

    python3 scripts/bench-repo.py /tmp/collate-bench
    COLLATE_BENCH_REPO=/tmp/collate-bench cargo test --release bench_refresh -- --ignored --nocapture

400 files of 200 to 3000 lines are committed, then changed without committing: scattered
edits, a few whole rewrites, moved blocks, deletions and new files. The same seed gives the
same repository every time, so numbers from two runs compare.
"""
import os
import random
import subprocess
import sys

WORDS = ["request", "response", "handler", "config", "session", "buffer", "token", "index",
         "result", "value", "client", "server", "cache", "queue", "worker"]


def git(d, *args):
    subprocess.run(["git", "-C", d, *args], check=True, stdout=subprocess.DEVNULL)


def line(r):
    k = r.random()
    w = lambda: r.choice(WORDS)
    if k < 0.1:
        return ""
    if k < 0.2:
        return f"    // {w()} {w()} {w()} for the {w()}"
    if k < 0.3:
        return "    }"
    if k < 0.45:
        return f"    if {w()}.{w()}() > {r.randint(0, 99)} {{"
    return f"    let {w()}_{r.randint(0, 999)} = {w()}.{w()}({w()}, {r.randint(0, 9999)})?;"


def write(path, lines):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        f.write("\n".join(lines) + "\n")


def main():
    if len(sys.argv) != 2:
        sys.exit("usage: bench-repo.py <new folder>")
    d = sys.argv[1]
    if os.path.exists(d):
        sys.exit(f"{d} already exists; give a folder that doesn't")
    os.makedirs(d)
    git(d, "init", "-q", "-b", "main")
    for key, value in [("user.email", "bench@example.invalid"), ("user.name", "bench"), ("commit.gpgsign", "false")]:
        git(d, "config", key, value)

    files = {}
    for i in range(400):
        r = random.Random(i)
        files[f"src/mod{i % 20}/file{i}.rs"] = [line(r) for _ in range(r.choice([200, 300, 500, 800, 1200, 3000]))]
    for path, lines in files.items():
        write(f"{d}/{path}", lines)
    write(f"{d}/README.md", ["# bench", ""] + [f"line {i}" for i in range(200)])
    git(d, "add", "-A")
    git(d, "commit", "-qm", "init")

    for k, (path, lines) in enumerate(files.items()):
        r = random.Random(1000 + k)
        lines = list(lines)
        if k % 40 == 0:
            os.remove(f"{d}/{path}")
            continue
        if k % 25 == 1:
            # Rewritten wholesale.
            lines = [line(r) for _ in range(len(lines))]
        else:
            for _ in range(r.randint(2, 30)):
                at = r.randrange(len(lines))
                c = r.random()
                if c < 0.5:
                    lines[at] = lines[at].replace("let ", "let mut ", 1) if "let " in lines[at] else line(r)
                elif c < 0.8:
                    lines[at:at] = [line(r) for _ in range(r.randint(1, 12))]
                else:
                    del lines[at:at + r.randint(1, 8)]
            if k % 10 == 2:
                # A moved block.
                a = r.randrange(len(lines) - 40)
                block = lines[a:a + 20]
                del lines[a:a + 20]
                b = r.randrange(len(lines))
                lines[b:b] = block
        write(f"{d}/{path}", lines)
    for i in range(15):
        r = random.Random(5000 + i)
        write(f"{d}/src/mod{i}/new{i}.rs", [line(r) for _ in range(400)])
    with open(f"{d}/README.md", "a") as f:
        f.write("edited\n")
    print(f"made {d}")


if __name__ == "__main__":
    main()
