"""The parts of bench/sweep that parse JSON or the output of cargo:

    python3 -I bench/sweep.py list <n> <crates.txt>           the top <n> crates by recent downloads, pinned
    python3 -I bench/sweep.py source <dir>                    the source file of the crate's library or first binary
    python3 -I bench/sweep.py outcomes <store slice> <log>    trucker's outcomes of the crates that the build ran
    python3 -I bench/sweep.py differ <p> <t>                  exits 1 where the tests of two logs differ
    python3 -I bench/sweep.py report <out>                    the Markdown report of a sweep

A test log ends with the line `sweep: exit <status>`, which bench/sweep appends. A test is named by its binary and
its name, so two binaries that share a test name do not collide, and a binary that fails without naming a test, as
one with `harness = false` can, counts as a test of its own.
"""

import json
import math
import re
import subprocess
import sys
import urllib.request
from collections import Counter
from pathlib import Path

agent = "trucker-bench-sweep"


def list_crates(n, path):
    rows = []
    page = 1
    while len(rows) < n:
        url = f"https://crates.io/api/v1/crates?sort=recent-downloads&per_page=100&page={page}"
        with urllib.request.urlopen(urllib.request.Request(url, headers={"User-Agent": agent})) as r:
            crates = json.load(r)["crates"]
        if not crates:
            break
        rows += [(c["name"], c.get("max_stable_version") or c["newest_version"]) for c in crates]
        page += 1
    with open(path, "w") as f:
        f.write(f"# The top {n} crates of crates.io by recent downloads, as `name version`, listed by bench/sweep.\n")
        f.writelines(f"{name} {version}\n" for name, version in rows[:n])


def source(directory):
    meta = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--offline", "--format-version", "1"],
        cwd=directory, capture_output=True, text=True, check=True,
    )
    targets = json.loads(meta.stdout)["packages"][0]["targets"]
    for kind in ("lib", "proc-macro", "rlib", "bin"):
        for t in targets:
            if kind in t["kind"]:
                print(t["src_path"])
                return
    sys.exit(1)


def outcomes(store, log):
    """Counts the outcomes of the lines whose crate the build ran, because builds of other crates that run at the same
    time write to the same log."""
    names = set(re.findall(r"--crate-name (\S+)", Path(log).read_text(errors="replace")))
    fields = (line.split(maxsplit=3) for line in Path(store).read_text(errors="replace").splitlines())
    print(json.dumps(dict(Counter(f[3] for f in fields if len(f) == 4 and f[1] in names))))


def tests(path):
    """The outcome of each test that the log of a `cargo test` names, by binary and name."""
    found = {}
    binary = "?"
    pending = None
    failed_targets = False
    for line in Path(path).read_text(errors="replace").splitlines():
        # cargo -v names a test binary by its path, whose file name with its hash is the same in every worktree, and
        # a compile by the command line of rustc or rustdoc.
        if (m := re.match(r"\s+Running `([^`\s]+)", line)) and "--crate-name" not in line:
            binary = Path(m[1]).name
        elif m := re.match(r"\s+Doc-tests (\S+)", line):
            binary = f"doc {m[1]}"
        elif m := re.match(r"test (.+) \.\.\. (ok|FAILED|ignored)(, .*)?$", line):
            found[f"{binary} :: {m[1]}"] = m[2]
            pending = None
        elif m := re.match(r"test (.+) \.\.\. ", line):
            pending = f"{binary} :: {m[1]}"
        elif pending and (m := re.match(r"(ok|FAILED|ignored)\b", line)):
            found[pending] = m[1]
            pending = None
        elif m := re.match(r"error: test failed, to rerun pass `(.+)`", line):
            found[f"target {m[1]}"] = "FAILED"
        elif re.match(r"error: \d+ targets? failed:", line):
            failed_targets = True
        elif failed_targets and (m := re.match(r"\s+`(.+)`$", line)):
            found[f"target {m[1]}"] = "FAILED"
        elif re.match(r"error: could not compile", line):
            found["compile"] = "FAILED"
        elif m := re.match(r"sweep: exit (\d+)", line):
            found["exit"] = "timeout" if m[1] in ("124", "137") else ("ok" if m[1] == "0" else "failed")
    return found


def differences(p, t):
    a, b = tests(p), tests(t)
    return {k for k in a.keys() | b.keys() if a.get(k) != b.get(k)}


def mismatches(logs, step):
    """The tests whose outcome under trucker in `step` differed from plain cargo's, in the first run and in the rerun
    of both, if there was one."""
    first = differences(logs / "P.test.log", logs / f"{step}.test.log")
    if first and (logs / "P2.test.log").exists():
        first &= differences(logs / "P2.test.log", logs / "T2.test.log")
    return first


def report(out):
    steps = {}
    for line in (out / "sweep.jsonl").read_text().splitlines():
        r = json.loads(line)
        steps[(r["crate"], r["step"])] = r
    fetched = {}
    for line in (out / "fetch.txt").read_text().splitlines():
        name, version, lockfile, status = line.split(" ", 3)
        fetched[name] = (version, lockfile, status)
    rows, matched, mismatched, skipped, fresh, edit = [], [], [], [], [], []
    for name, (version, lockfile, status) in fetched.items():
        logs = out / "logs" / name
        s = {k[1]: v for k, v in steps.items() if k[0] == name}

        def ok(step):
            return "–" if step not in s else ("ok" if s[step]["status"] == 0 else "fail")

        passthrough = Counter()
        for r in s.values():
            for k, v in r.get("outcomes", {}).items():
                if k.startswith("passthrough "):
                    passthrough[k.removeprefix("passthrough ")] += v
        hits = s.get("B.build", {}).get("outcomes", {})
        tests_b = tests_e = "–"
        problems = []
        if status != "ok":
            verdict = f"skipped (fetch: {status})"
        elif "P.build" not in s:
            verdict = "not run"
        elif ok("P.build") != "ok":
            verdict = "skipped (plain fails)"
        elif tests(logs / "P.test.log").get("exit") == "timeout":
            verdict = "skipped (plain tests time out)"
        else:
            for step in ("A.build", "B.build", "E.build"):
                if ok(step) != "ok":
                    problems.append(f"{step[0]} build fails")
            for step in ("B", "E"):
                if f"{step}.test" in s:
                    diff = mismatches(logs, step)
                    if step == "B":
                        tests_b = "yes" if not diff else f"no ({len(diff)})"
                    else:
                        tests_e = "yes" if not diff else f"no ({len(diff)})"
                    if diff:
                        problems.append(f"{step} tests differ: " + ", ".join(sorted(diff)[:3]))
            verdict = "match" if not problems else "mismatch"
            if s.get("B.build", {}).get("status") == 0:
                fresh.append(s["B.build"]["seconds"] / s["P.build"]["seconds"])
            if s.get("E.build", {}).get("status") == 0 and "P.edit" in s:
                edit.append(s["E.build"]["seconds"] / s["P.edit"]["seconds"])
        (matched if verdict == "match" else mismatched if verdict == "mismatch" else skipped).append(
            (name, verdict, problems))
        times = "–"
        if "B.build" in s:
            times = f"{s['B.build']['seconds']:.2f}s / {s['P.build']['seconds']:.2f}s"
        rows.append(
            f"| {name} | {version}{'' if lockfile == 'published' else '*'} | {ok('P.build')} / {ok('B.build')} / "
            f"{ok('E.build')} | {tests_b} / {tests_e} | "
            f"{', '.join(f'{v} {k}' for k, v in sorted(passthrough.items())) or '–'} | "
            f"{hits.get('hit', 0)} / {sum(v for k, v in hits.items() if k.startswith('miss'))} | {times} | {verdict} |"
        )

    def geomean(xs):
        return f"{math.exp(sum(map(math.log, xs)) / len(xs)):.2f} over {len(xs)} crates" if xs else "–"

    print("| Crate | Version | Builds P / B / E | Tests match B / E | Passthroughs | B hits / misses | "
          "B fresh / P cold | Verdict |")
    print("| ----- | ------- | ---------------- | ----------------- | ------------ | --------------- | "
          "---------------- | ------- |")
    print("\n".join(rows))
    print("\nA version with `*` built with a lockfile that `bench/sweep fetch` generated, which its commit pins.\n")
    print(f"- trucker matched plain cargo on {len(matched)} of the {len(matched) + len(mismatched)} crates that plain "
          f"cargo builds. The sweep skipped {len(skipped)} more.")
    for name, verdict, problems in mismatched:
        print(f"- **{name}** does not match, because {'; '.join(problems)}.")
    for name, verdict, _ in skipped:
        print(f"- {name} was {verdict}.")
    print(f"- The time of B against P's cold build has a geometric mean of {geomean(fresh)}, which is only indicative.")
    print(f"- The time of E against P's edit has a geometric mean of {geomean(edit)}, which is only indicative.")


if __name__ == "__main__":
    command, *args = sys.argv[1:]
    if command == "list":
        list_crates(int(args[0]), args[1])
    elif command == "source":
        source(args[0])
    elif command == "outcomes":
        outcomes(*args)
    elif command == "differ":
        diff = differences(args[0], args[1])
        print("\n".join(sorted(diff)))
        sys.exit(1 if diff else 0)
    elif command == "report":
        report(Path(args[0]))
    else:
        sys.exit(f"unknown command {command}")
