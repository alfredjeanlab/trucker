"""Prints the results of bench/run, bench/throughput and bench/sentinel as Markdown tables.

    python3 -I bench/report.py <out>[,<out>]... [--trucker=<commit>] <mode> [project]...

    summary                 a table per scenario, a column per tool
    detail [project]...     a table per project and scenario, with the spread and the work done
    disk [project]...       the disk that each tool uses after each step, by df
    tests [project]...      the scenario `test` of bench/run
    growth [project]...     the disk used after each build of scenario 3
    conc [project]...       four worktrees building at once, the scenario `conc` of bench/run
    runs [project]...       each run of trucker's 1b to 2d apart, with the compiles from a carried session
    outcomes [project]...   trucker's outcomes in each step, from the lines of its store's log that the step names
    progress [project]...   trucker's rows of each <out>:<commit> beside plain cargo's
    readme                  the README's tables and the ranges of its comparison matrix
    geomean                 each tool's geometric mean ratio to plain cargo across projects
    sentinels               the builds and verdicts of bench/sentinel
    throughput [project]... worktrees building at once, from bench/throughput
    throughput-readme [project]...  the README's table of bench/throughput

A ratio compares a tool with plain cargo of the pass that measured the tool's row. Directories joined by `+`, as in
`a+b,c`, are one pass, whose later directory replaces rows of the earlier. `--trucker` names the build of trucker in
the tables.
"""

import json
import math
import statistics
import sys
from collections import Counter, defaultdict
from pathlib import Path

links = {
    "none": "cargo, no cache",
    "trucker": "trucker",
    "sccache": "[sccache](https://github.com/mozilla/sccache)",
    "mbx": "[mbx](https://mr-boxington.jdx.dev)",
    "kache": "[kache](https://github.com/kunobi-ninja/kache)",
    "shared": "shared `CARGO_TARGET_DIR`",
    "builddir": "shared `build.build-dir`",
    "reflink": "`cp --reflink` of a target",
}
short = {"none": "cargo", "shared": "shared target", "builddir": "shared build-dir", "reflink": "copied target"}
tools = list(links)
projects = {
    "ripgrep": "ripgrep 15.2.0",
    "cargo": "cargo 0.101.0",
    "cargo-vendored": "cargo 0.101.0, vendored OpenSSL",
    "serde": "serde 1.0.229",
    "rust": "rust-analyzer, in rust 1.100.0",
    "ruff": "ruff 0.17.5",
    "sccache": "sccache 0.19.0",
    "mr-boxington": "mr-boxington 1.30.0",
    "trucker": "trucker",
    "nushell": "nushell 0.117.0",
    "uv": "uv 0.13.9",
    "sqlx": "sqlx 0.8, offline",
    "wasm": "wasm32 workspace",
    "zed": "Zed 1.29.2",
}
scenarios = {
    "1a": "1a. Cold build, no cache, empty target",
    "1b": "1b. New worktree, warm cache",
    "1c": "1c. Separate clone at another path, warm cache",
    "1d": "1d. New worktree after the first was removed, warm cache",
    "2a": "2a. First edit of the most depended-on crate, in a new worktree",
    "2b": "2b. Second edit of the most depended-on crate",
    "2c": "2c. First edit of a leaf crate, in a new worktree",
    "2d": "2d. Second edit of a leaf crate",
    "t1b": "Tests: `cargo test --no-run` in a new worktree, warm cache",
    "t2a": "Tests: `cargo test --no-run` after an edit of the most depended-on crate",
}
concurrent = {
    "cwarm": "Four new worktrees building at once, warm cache",
    "ccold": "Four new worktrees building at once, cold cache",
}


def seconds(s):
    return f"{s * 1000:.0f}ms" if s < 1 else f"{s:.2f}s"


def size(n):
    if n is None:
        return "–"
    n = max(n, 0)
    for unit, scale in (("GB", 1e9), ("MB", 1e6), ("KB", 1e3)):
        if n >= scale:
            return f"{n / scale:.1f} {unit}" if n < 10 * scale else f"{n / scale:.0f} {unit}"
    return f"{n:.0f} B"


def steps(dirs):
    """The records of steps.jsonl in each of `dirs`, each with the directory it came from. A tool's rows of one
    project on one filesystem come from the latest directory that measured them, so that each such set of rows comes
    from one build of the tool."""
    key = lambda r: (r["fs"], r["project"], r["tool"])
    lines = lambda d: (d / "steps.jsonl").read_text().splitlines() if (d / "steps.jsonl").exists() else []
    latest = {}
    for d in dirs:
        for line in lines(d):
            latest[key(json.loads(line))] = d
    out = []
    for d in dirs:
        for line in lines(d):
            r = json.loads(line)
            if latest[key(r)] == d:
                out.append(dict(r, dir=d))
    return out


def load(dirs):
    """Each (fs, project, tool, step) with its times and the median of its work and disk, from steps.jsonl. A record
    of hyperfine's runs, as of 1a, gives the time of each run in `times`."""
    records = defaultdict(list)
    for r in steps(dirs):
        if r["ok"]:
            records[(r["fs"], r["project"], r["tool"], r["step"])].append(r)
    results = {}
    for key, rs in records.items():
        fs, project, tool, step = key
        times = [t for r in rs for t in r.get("times", [r["seconds"]]) if t is not None]
        keys = ("rustc", "restored", "fresh", "target", "store", "added", "total")
        median = {k: statistics.median(r[k] for r in rs) for k in keys}
        results[key] = dict(median, times=times, dir=rs[0]["dir"])
    return results


def mean(r):
    return statistics.mean(r["times"])


def stddev(r):
    return statistics.stdev(r["times"]) if len(r["times"]) > 1 else 0.0


def reference(results, fs, project, step, tool=None):
    """Plain cargo's time in the same scenario, where its new worktree or clone is a cold build, from the pass that
    measured the tool's row where a tool is given."""
    if (r := results.get((fs, project, tool, step))) is not None:
        results = passes[r["dir"]]
    for s in (step, "1a") if step in ("1b", "1c", "1d") else (step,):
        r = results.get((fs, project, "none", s))
        if r and r["times"]:
            return r, s
    return None, None


def detail(results, fs, names, only=None):
    for project in names:
        if only and not any((fs, project, t, st) in results for t in tools for st in only):
            continue
        print(f"#### {projects.get(project, project)}\n")
        for step, title in scenarios.items():
            if only and step not in only:
                continue
            rows = [(t, results[(fs, project, t, step)]) for t in tools if (fs, project, t, step) in results]
            rows = [(t, r) for t, r in rows if r["times"]]
            if not rows:
                continue
            ref, ref_step = reference(results, fs, project, step)
            print(f"{title}\n")
            print("| Tool | Runs | Ran / restored / fresh | Target | Store | Disk added | Time |")
            print("| ---- | ---: | ---------------------: | -----: | ----: | ---------: | ---: |")
            fastest = min(mean(r) for _, r in rows)
            if ref_step and ref_step != step:
                r = ref
                print(f"| {links['none']} (as 1a) | {len(r['times'])} | {r['rustc']:.0f} / 0 / 0 | "
                      f"{size(r['target'])} | – | {size(r['total'])} | {seconds(mean(r))} ± {seconds(stddev(r))} (1.00x) |")
            for t, r in sorted(rows, key=lambda tr: mean(tr[1])):
                time = f"{seconds(mean(r))} ± {seconds(stddev(r))}"
                if mean(r) == fastest:
                    time = f"**{time}**"
                tref, _ = reference(results, fs, project, step, t)
                ratio = f" ({mean(r) / mean(tref):.2f}x)" if tref else ""
                added = r["total"] if step == "1a" else r["added"]
                store = size(r["store"]) if t in ("trucker", "sccache", "mbx", "kache", "builddir") else "–"
                work = f"{r['rustc']:.0f} / {r['restored']:.0f} / {r['fresh']:.0f}"
                print(f"| {links[t]} | {len(r['times'])} | {work} | "
                      f"{size(r['target'])} | {store} | {size(added)} | {time}{ratio} |")
            print()


def cpu_of(dirs):
    """The CPU seconds of each concurrent run, from cpu.jsonl, keyed as the results are."""
    cpu = {}
    for d in dirs:
        path = d / "cpu.jsonl"
        for line in path.read_text().splitlines() if path.exists() else []:
            r = json.loads(line)
            cpu[(r["fs"], r["project"], r["tool"], r["step"].removesuffix("-cpu"))] = r["cpu"]
    return cpu


def conc(results, fs, names):
    cpu = cpu_of(dirs)
    for project in names:
        for step, title in concurrent.items():
            rows = [(t, results.get((fs, project, t, step))) for t in tools]
            if step == "ccold":
                # Plain cargo's warm run is its cold one.
                rows = [(t, r or (results.get((fs, project, t, "cwarm")) if t == "none" else None)) for t, r in rows]
            rows = [(t, r) for t, r in rows if r and r["times"]]
            if not rows:
                continue
            ref = dict(rows).get("none")
            print(f"#### {projects.get(project, project)}: {title}\n")
            print("| Tool | Ran / restored / fresh | Disk added | CPU | Makespan |")
            print("| ---- | ---------------------: | ---------: | --: | -------: |")
            for t, r in sorted(rows, key=lambda tr: mean(tr[1])):
                key = (fs, project, t, step if (fs, project, t, step) in cpu else "cwarm")
                ratio = f" ({mean(r) / mean(ref):.2f}x)" if ref else ""
                c = cpu.get(key)
                print(f"| {links[t]} | {r['rustc']:.0f} / {r['restored']:.0f} / {r['fresh']:.0f} | {size(r['added'])} | "
                      f"{f'{c:.0f}s' if c is not None else '–'} | {seconds(mean(r))}{ratio} |")
            print()


def progress(pairs, fs, names):
    """trucker's rows of each project in each of `pairs`, a list of (directory, commit), beside plain cargo's: the time
    and the disk added of 1b and each edit."""
    steps_ = ("1b", "2c", "2d", "2a", "2b")
    for project in names:
        rows = []
        for d, commit in pairs:
            res = load([d])
            if (fs, project, "trucker", "1b") in res:
                rows.append((f"trucker {commit}", res, "trucker"))
        plain = load([d for d, _ in pairs])
        if not rows:
            continue
        print(f"#### {projects.get(project, project)}\n")
        print("| Build | " + " | ".join(steps_) + " |")
        print("| ----- | " + " | ".join("---:" for _ in steps_) + " |")
        for label, res, tool in rows + [(links["none"], plain, "none")]:
            cells = []
            for st in steps_:
                r = res.get((fs, project, tool, st))
                if tool == "none" and st == "1b":
                    r = res.get((fs, project, tool, "1a"))
                cells.append(f"{seconds(mean(r))}, {size(r['total'] if tool == 'none' and st == '1b' else r['added'])}"
                             if r and r["times"] else "–")
            print(f"| {label} | " + " | ".join(cells) + " |")
        print()


def count(lines):
    """The outcomes of the compiles among `lines` of the log of trucker's store, `<time> <crate> <milliseconds>
    <outcome>`, without the lines of cargo's probes of rustc and of gc, which do not name a crate."""
    fields = (line.split(maxsplit=3) for line in lines)
    return dict(Counter(f[3].strip() for f in fields if len(f) == 4 and f[1] != "-"))


def outcome_records(dirs):
    """trucker's outcomes for each project, step and run of bench/run, from the lines of its store's log that each
    record of steps.jsonl names in `store_log` and `store_lines`, the first and last line, counted from 1."""
    logs = {}
    records = []
    for d in dirs:
        path = d / "steps.jsonl"
        for line in path.read_text().splitlines() if path.exists() else []:
            r = json.loads(line)
            if "store_lines" not in r:
                continue
            log = d / r["store_log"]
            # The lines as `wc -l` counts them, which is how the scripts number them.
            if log not in logs:
                logs[log] = log.read_bytes().decode().split("\n")
            first, last = r["store_lines"]
            records.append({"project": r["project"], "step": r["step"], "run": r["run"],
                            "outcomes": count(logs[log][first - 1:last])})
    return sorted(records, key=lambda r: (r["project"], r["run"], r["step"]))


def outcomes(dirs, names):
    """trucker's outcomes in each run of each step."""
    rows = defaultdict(list)
    for r in outcome_records(dirs):
        rows[(r["project"], r["step"])].append(r["outcomes"])
    labels = dict(scenarios, **concurrent)
    for project in names:
        present = [s for s in ("1a", "1b", "2c", "2d", "2a", "2b", "cwarm", "ccold") if (project, s) in rows]
        if not present:
            continue
        print(f"#### {projects.get(project, project)}\n")
        print("| Step | Outcomes of each run |")
        print("| ---- | -------------------- |")
        for s in present:
            cells = []
            for r in rows[(project, s)]:
                cells.append(", ".join(f"{n} {o}" for o, n in sorted(r.items(), key=lambda on: -on[1])) or "nothing")
            label = s if s in scenarios else labels[s]
            print(f"| {label} | " + "; ".join(cells) + " |")
        print()


def runs(dirs, fs, names, tool="trucker"):
    """Each run of 1b to 2d of a tool apart, with its time, the disk that it added and the compiles that started from a
    carried session."""
    sessions = {(r["project"], r["step"], r["run"]): r["outcomes"].get("miss session", 0) for r in outcome_records(dirs)}
    steps_ = ("1b", "2c", "2d", "2a", "2b")
    for project in names:
        rows = defaultdict(dict)
        for r in steps(dirs):
            if r["fs"] == fs and r["project"] == project and r["tool"] == tool and r["step"] in steps_ and r["ok"]:
                rows[r["run"]][r["step"]] = r
        if not rows:
            continue
        present = [s for s in steps_ if any(s in v for v in rows.values())]
        print(f"#### {projects.get(project, project)}\n")
        print("| Run | " + " | ".join(present) + " |")
        print("| --: | " + " | ".join("---:" for _ in present) + " |")
        for run in sorted(rows):
            cells = []
            for s in present:
                r = rows[run].get(s)
                n = sessions.get((project, s, run))
                carried = f", {n} from a session" if n else ""
                cells.append(f"{seconds(r['seconds'])}, {size(r['added'])}{carried}" if r else "–")
            print(f"| {run} | " + " | ".join(cells) + " |")
        print()


def sentinels(dirs):
    """The records of bench/sentinel, from sentinels.jsonl: one row per sentinel with the times of trucker and plain
    cargo, the reasons of trucker's passthroughs and the verdict, then each sentinel's builds."""
    records = defaultdict(list)
    order = []
    for d in dirs:
        path = d / "sentinels.jsonl"
        for line in path.read_text().splitlines() if path.exists() else []:
            r = json.loads(line)
            if r["sentinel"] not in records:
                order.append(r["sentinel"])
            records[r["sentinel"]].append(r)

    def clean(outcomes):
        return {o: n for o, n in outcomes.items() if not o.startswith("gc ")}

    print("| Sentinel | Builds, trucker against plain cargo | Passthroughs | Verdict |")
    print("| -------- | ----------------------------------- | ------------ | ------- |")
    for name in order:
        builds = [r for r in records[name] if "step" in r]
        verdicts = [r for r in records[name] if "verdict" in r]
        cells = []
        for step in dict.fromkeys(r["step"] for r in builds):
            t = [r for r in builds if r["step"] == step and r["tool"] == "trucker"]
            n = [r for r in builds if r["step"] == step and r["tool"] == "none"]
            if t and n:
                cells.append(f"{step} {seconds(t[0]['seconds'])} / {seconds(n[0]['seconds'])}")
            elif t:
                cells.append(f"{step} {seconds(t[0]['seconds'])}")
            if any(r["status"] for r in t + n):
                cells[-1] += " (failed)"
        reasons = defaultdict(int)
        for r in builds:
            for o, k in clean(r["outcomes"]).items():
                if o.startswith("passthrough"):
                    reasons[o.removeprefix("passthrough ")] += k
        verdict = "; ".join(f"**{v['verdict']}**, {v['detail'].splitlines()[0] if v['detail'] else ''}".rstrip(", ")
                            for v in verdicts) or "–"
        print(f"| {name} | {', '.join(cells)} | {', '.join(f'{k} {o}' for o, k in reasons.items()) or '–'} | {verdict} |")
    print()
    for name in order:
        builds = [r for r in records[name] if "step" in r]
        print(f"#### {name}\n")
        print("| Step | Tool | Time | Status | Ran / fresh | Outcomes |")
        print("| ---- | ---- | ---: | -----: | ----------: | -------- |")
        for r in builds:
            out = ", ".join(f"{k} {o}" for o, k in sorted(clean(r["outcomes"]).items(), key=lambda ok: -ok[1]))
            print(f"| {r['step']} | {links['trucker'] if r['tool'] == 'trucker' else links['none']} | "
                  f"{seconds(r['seconds'])} | {r['status']} | {r['ran']} / {r['fresh']} | {out or '–'} |")
        for v in (r for r in records[name] if "verdict" in r):
            print(f"\nVerdict: **{v['verdict']}**. {' '.join(v['detail'].split())}")
        print()


def summary(results, fs, steps):
    names = [p for p in projects if any(k[1] == p and k[0] == fs for k in results)]
    present = [t for t in tools if any(k[2] == t and k[0] == fs for k in results)]
    for step in steps:
        print(f"{scenarios[step]}:\n")
        print("| Project | " + " | ".join(short.get(t, links[t]) for t in present) + " |")
        print("| ------- | " + " | ".join("---:" for _ in present) + " |")
        for project in names:
            ref, _ = reference(results, fs, project, step)
            cells = {}
            for t in present:
                r = results.get((fs, project, t, step))
                if t == "none" and step in ("1b", "1c", "1d"):
                    r = ref
                cells[t] = r if r and r["times"] else None
            if not any(cells.values()):
                continue
            fastest = min(mean(r) for r in cells.values() if r)
            row = []
            for t in present:
                r = cells[t]
                if not r:
                    row.append("–")
                    continue
                cell = seconds(mean(r))
                cell = f"**{cell}**" if mean(r) == fastest else cell
                tref, _ = reference(results, fs, project, step, t)
                if tref and t != "none":
                    cell += f" ({mean(r) / mean(tref):.2f}x)"
                added = r["total"] if step == "1a" or (t == "none" and step in ("1b", "1c", "1d")) else r["added"]
                row.append(f"{cell}, {size(added)}")
            print(f"| {projects.get(project, project)} | " + " | ".join(row) + " |")
        print()


def growth(results, fs, names):
    for project in names:
        steps = [f"g{i}" for i in range(12)]
        present = [t for t in tools if (fs, project, t, "g0") in results]
        if not present:
            continue
        print(f"#### {projects.get(project, project)}\n")
        print("| Build | " + " | ".join(short.get(t, links[t]) for t in present) + " |")
        print("| ----- | " + " | ".join("---:" for _ in present) + " |")
        labels = ["release"] + [f"{i} commits back" if i > 1 else "1 commit back" for i in range(1, 7)]
        labels += ["release again"] + [f"edit {i}" for i in range(1, 5)]
        for step, label in zip(steps, labels):
            row = []
            for t in present:
                r = results.get((fs, project, t, step))
                row.append(f"{size(r['total'])} ({seconds(mean(r))})" if r and r["times"] else "–")
            print(f"| {label} | " + " | ".join(row) + " |")
        print()


def disk(results, fs, names, worktrees=4):
    """Real allocated space, by df: what one worktree costs after its cold build, what each further worktree adds and
    what `worktrees` of them use together, what each edit adds, and the total after all the runs of each scenario."""
    raw = defaultdict(list)
    for rec in steps(dirs):
        if rec["fs"] == fs and rec["ok"]:
            raw[(rec["project"], rec["tool"])].append(rec)
    for project in names:
        present = [t for t in tools if (project, t) in raw]
        if not present:
            continue
        cold = results.get((fs, project, "none", "1a"))

        def first(t):
            r = results.get((fs, project, t, "1a"))
            return (r or cold or {}).get("total")

        def added(t, step):
            if t == "none" and step in ("1b", "1c", "1d"):
                return cold and cold["total"]
            r = results.get((fs, project, t, step))
            return r and r["added"]

        rows = [("One worktree, after its cold build (1a)", first)]
        if any(added(t, "1b") is not None for t in present if t != "none"):
            rows.append(("Each further worktree (1b)", lambda t: added(t, "1b")))
            rows.append((f"{worktrees} worktrees", lambda t: first(t) + (worktrees - 1) * added(t, "1b")
                         if first(t) is not None and added(t, "1b") is not None else None))
        for step in ("2c", "2d", "2a", "2b"):
            if any((fs, project, t, step) in results for t in present):
                rows.append((f"Each {step} edit", lambda t, step=step: added(t, step)))
        seq = {t: [r for r in raw[(project, t)] if r["step"] in ("1b", "2a", "2b", "2c", "2d")] for t in present}
        if any(seq.values()):
            rows.append(("Total after every run of 1b to 2d", lambda t: seq[t][-1]["total"] if seq[t] else None))
        grown = {t: [r for r in raw[(project, t)] if r["step"] == "g11"] for t in present}
        if any(grown.values()):
            rows.append(("Total after scenario 3", lambda t: grown[t][-1]["total"] if grown[t] else None))
        last = {t: raw[(project, t)][-1] for t in present}
        rows.append(("Of which the store, at the end, by `du`", lambda t: last[t]["store"] if t in ("trucker", "sccache", "mbx", "kache", "builddir") else None))
        print(f"#### {projects.get(project, project)}\n")
        print("| After | " + " | ".join(short.get(t, links[t]) for t in present) + " |")
        print("| ----- | " + " | ".join("---:" for _ in present) + " |")
        for label, value in rows:
            print(f"| {label} | " + " | ".join(size(value(t)) for t in present) + " |")
        print()


def throughput(groups, fs, names):
    """The records of bench/throughput, a table per project and count of worktrees: the mean time until the last build
    ends, the spread of each worktree's time, and the median of the rest over the runs."""
    rows = defaultdict(list)
    for group in groups:
        # A later directory of a pass replaces the rows of the earlier.
        passed = {}
        for d in group:
            path = d / "throughput.jsonl"
            found = defaultdict(list)
            for line in path.read_text().splitlines() if path.exists() else []:
                r = json.loads(line)
                if r["fs"] == fs and r["ok"]:
                    found[(r["project"], r["worktrees"], r["step"], r["tool"])].append(r)
            passed.update(found)
        for key, rs in passed.items():
            rows[key].extend(rs)
    titles = {"1b": "New worktrees", "2c": "Leaf edit", "2a": "Deep edit", "2b": "Second deep edit"}
    med = lambda rs, f: statistics.median(f(r) for r in rs) if all(f(r) is not None for r in rs) else None
    disk = lambda r: r["io"] and r["io"].get(r["device"])
    for project in names:
        for n in sorted({k[1] for k in rows if k[0] == project}):
            print(f"#### {projects.get(project, project)}, {n} worktrees at once\n")
            print("| Step | Tool | Runs | Ran / restored | Last done | Each | Read / written | Peak in use / page cache "
                  "| Disk added |")
            print("| ---- | ---- | ---: | -------------: | --------: | ---: | -------------: | -----------------------: "
                  "| ---------: |")
            for step, title in titles.items():
                ref = rows.get((project, n, step, "none"))
                for t in tools:
                    rs = rows.get((project, n, step, t))
                    if not rs:
                        continue
                    last = statistics.mean(r["seconds"] for r in rs)
                    ratio = f" ({last / statistics.mean(r['seconds'] for r in ref):.2f}x)" if ref and t != "none" else ""
                    each = [s for r in rs for s in r["times"]]
                    read, written = med(rs, lambda r: disk(r) and disk(r)["read"]), med(rs, lambda r: disk(r) and disk(r)["written"])
                    memory, cached = med(rs, lambda r: r["memory"]), med(rs, lambda r: r["cached"])
                    print(f"| {title} | {links[t]} | {len(rs)} | {med(rs, lambda r: r['rustc']):.0f} / "
                          f"{med(rs, lambda r: r['restored']):.0f} | {seconds(last)}{ratio} | "
                          f"{seconds(min(each))}–{seconds(max(each))} | {size(read)} / {size(written)} | "
                          f"{size(memory)} / {size(cached)} | {size(med(rs, lambda r: r['added']))} |")
            print()


def throughput_readme(groups, fs, names, n=4):
    """The README's table of bench/throughput: the mean time until the last of n worktrees is done, per project and
    step, for plain cargo, a shared target and trucker."""
    rows = defaultdict(list)
    for group in groups:
        for d in group:
            path = d / "throughput.jsonl"
            for line in path.read_text().splitlines() if path.exists() else []:
                r = json.loads(line)
                if r["fs"] == fs and r["ok"] and r["worktrees"] == n:
                    rows[(r["project"], r["step"], r["tool"])].append(r["seconds"])
    titles = {"1b": "Fresh worktree", "2c": "Leaf edit", "2a": "Deep edit", "2b": "Second deep edit"}
    cols = ("none", "shared", "trucker")
    print("| Project | Step | cargo | [shared `target/`][shared] | trucker |")
    print("| --- | --- | ---: | ---: | ---: |")
    for p in names:
        for step, title in titles.items():
            means = {t: statistics.mean(rows[(p, step, t)]) for t in cols if rows.get((p, step, t))}
            if not means:
                continue
            best = seconds(min(means.values()))
            cells = [("—" if t not in means else f"**{s}**" if (s := seconds(means[t])) == best else s) for t in cols]
            print(f"| {readme_projects.get(p, p)} | {title} | " + " | ".join(cells) + " |")
    print()


readme_tools = {
    "none": "cargo",
    "trucker": "trucker",
    "kache": "[kache]",
    "mbx": "[mr-boxington]",
    "shared": "[shared `target/`][shared]",
    "sccache": "[sccache]",
}
readme_links = {
    "sccache": "docs/comparisons/vs-sccache.md",
    "mr-boxington": "docs/comparisons/vs-mr-boxington.md",
    "kache": "docs/comparisons/vs-kache.md",
    "shared": "docs/comparisons/vs-cargo.md",
}
readme_projects = {
    "ripgrep": "ripgrep", "cargo": "cargo", "cargo-vendored": "cargo-vendored", "rust": "rust-analyzer",
    "zed": "Zed",
}
# The projects whose range of ratios the comparison matrix gives.
matrix_projects = ("ripgrep", "cargo", "rust", "zed")
readme_scenarios = {
    "1b": ("Fresh worktree, warm cache",
           "A new git worktree of the same commit builds, with the cache warm from the cold build of another worktree."),
    "2a": ("First edit of the deepest crate",
           "The new worktree appends an unused function to the crate that every other own crate depends on, and builds."),
    "2c": ("First edit of a leaf crate",
           "The new worktree appends an unused function to the binary crate, which nothing depends on, and builds."),
    "disk": ("Disk use by four worktrees",
             "The filesystem's used space, by `df`, after one cold build and three new worktrees."),
    "1a": ("Cold build",
           "One worktree builds with an empty target and an empty cache. mr-boxington turns off cargo's incremental compilation, so its cold build "
           "writes less and its first edits recompile more."),
}


def readme_value(results, fs, project, tool, step):
    """The mean seconds of a tool's step, or for `disk` the bytes of four worktrees, as the README shows them."""
    get = lambda t, s: results.get((fs, project, t, s))
    cold = get("none", "1a")
    if step == "disk":
        first, each = get(tool, "1a") or cold, cold if tool == "none" else get(tool, "1b")
        return first["total"] + 3 * (each["total"] if tool == "none" else each["added"]) if first and each else None
    if tool == "none" and step == "1b":
        return mean(cold) if cold else None
    if tool == "shared" and step == "1a":
        return None
    r = get(tool, step)
    return mean(r) if r and r["times"] else None


def readme(results, fs):
    """The README's benchmark tables, one per scenario with a row per project, then the ratios of its comparison
    matrix. Plain cargo's column is in italics, and the lowest value of a row in bold."""
    fmt = lambda step, v: "—" if v is None else (size(v) if step == "disk" else seconds(v))
    value = lambda p, t, s: readme_value(results, fs, p, t, s)
    for step, (title, sentence) in readme_scenarios.items():
        print(f"### {title}\n\n{sentence}\n")
        # A shared target has no cold build of its own, so the cold build leaves out its column.
        tools_ = {t: n for t, n in readme_tools.items() if not (step == "1a" and t == "shared")}
        print("| Project | " + " | ".join(tools_.values()) + " |")
        print("| --- | " + " | ".join("---:" for _ in tools_) + " |")
        for p in matrix_projects:
            name = readme_projects[p]
            # Every value that shows the same as the lowest is bold, and a missing one never is.
            shown = {t: fmt(step, value(p, t, step)) for t in tools_}
            best = fmt(step, min((v for t in tools_ if (v := value(p, t, step)) is not None), default=None))
            cells = [f"**{s}**" if s == best and s != "—" else f"*{s}*" if t == "none" else s for t, s in shown.items()]
            print(f"| {name} | " + " | ".join(cells) + " |")
        print()

    def span(ratios):
        """A range of ratios to plain cargo as the README's matrix gives it, such as `0.05–0.15×` or `~1×`."""
        n = lambda x: f"{x:.2f}" if x < 10 else f"{x:.0f}"
        a, b = n(min(ratios)), n(max(ratios))
        return f"~{a}×" if a == b else f"{a}–{b}×"

    print("| | " + " | ".join(readme_tools.values()) + " |")
    print("| --- | " + " | ".join("---" for _ in readme_tools) + " |")
    # A row of edits spans the edits of the most depended-on crate and of the leaf crate.
    for label, steps in (("First build in a fresh worktree", ("1b",)), ("First edit in a fresh worktree", ("2a", "2c")),
                         ("Second edit in a fresh worktree", ("2b", "2d"))):
        cells = []
        for t in readme_tools:
            ratios = []
            for p in matrix_projects:
                for step in steps:
                    v, (ref, _) = value(p, t, step), reference(results, fs, p, step, t)
                    if v is not None and ref:
                        ratios.append(v / mean(ref))
            cells.append("1×" if t == "none" else span(ratios) if ratios else "—")
        print(f"| {label} | " + " | ".join(cells) + " |")
    print()
    print("\n".join(f"[{k}]: {v}" for k, v in readme_links.items()))


def geomean(results, fs, steps=("1a", "1b", "2c")):
    """Per scenario and tool, the geometric mean of the ratio to plain cargo across every project that both measured,
    with the lowest and highest ratio, then each project's ratios.

    cargo-vendored is left out, because it is cargo again with another OpenSSL and would count cargo twice."""
    names = [p for p in dict.fromkeys(k[1] for k in results if k[0] == fs) if p != "cargo-vendored"]
    present = [t for t in tools if t != "none" and any(k[2] == t and k[0] == fs for k in results)]
    ratios = defaultdict(dict)
    for step in steps:
        for p in names:
            for t in present:
                r = results.get((fs, p, t, step))
                ref, _ = reference(results, fs, p, step, t)
                if ref and r and r["times"]:
                    ratios[(step, t)][p] = mean(r) / mean(ref)
    print("| Scenario | " + " | ".join(short.get(t, links[t]) for t in present) + " |")
    print("| -------- | " + " | ".join("---:" for _ in present) + " |")
    for step in steps:
        cells = []
        for t in present:
            v = list(ratios[(step, t)].values())
            if not v:
                cells.append("–")
                continue
            g = math.exp(statistics.mean(math.log(x) for x in v))
            cells.append(f"{g:.2f}x ({min(v):.2f}–{max(v):.2f}, {len(v)})")
        print(f"| {scenarios[step]} | " + " | ".join(cells) + " |")
    print()
    for step in steps:
        print(f"{scenarios[step]}, each project's ratio to plain cargo:\n")
        print("| Project | " + " | ".join(short.get(t, links[t]) for t in present) + " |")
        print("| ------- | " + " | ".join("---:" for _ in present) + " |")
        for p in names:
            if not any(p in ratios[(step, t)] for t in present):
                continue
            cells = [f"{ratios[(step, t)][p]:.2f}x" if p in ratios[(step, t)] else "–" for t in present]
            print(f"| {projects.get(p, p)} | " + " | ".join(cells) + " |")
        print()


groups = [[Path(d.rsplit(":", 1)[0]) if ":" in d else Path(d) for d in g.split("+")] for g in sys.argv[1].split(",")]
dirs = [d for g in groups for d in g]
fs = "xfs"
args = sys.argv[2:]
while args and args[0].startswith("--"):
    flag, _, value = args[0][2:].partition("=")
    if flag == "fs":
        fs = value
    elif flag == "trucker":
        # The build of trucker that the rows measured, which the tables name.
        links["trucker"] = short["trucker"] = f"trucker {value}"
    args = args[1:]
results = load(dirs)
passes = {d: res for g in groups for res in [load(g)] for d in g}
kind, names = args[0], args[1:] or list(projects)
if kind == "detail":
    detail(results, fs, names)
elif kind == "summary":
    summary(results, fs, names if args[1:] else list(scenarios))
elif kind == "growth":
    growth(results, fs, names)
elif kind == "disk":
    disk(results, fs, names)
elif kind == "tests":
    detail(results, fs, names, only=("t1b", "t2a"))
elif kind == "progress":
    # The directories, in order, each as <directory>:<commit>.
    pairs = [tuple(x.rsplit(":", 1)) for x in sys.argv[1].split(",")]
    progress([(Path(d), c) for d, c in pairs], fs, names)
elif kind == "conc":
    conc(results, fs, names)
elif kind == "outcomes":
    outcomes(dirs, names)
elif kind == "runs":
    runs(dirs, fs, names)
elif kind == "sentinels":
    sentinels(dirs)
elif kind == "throughput":
    throughput(groups, fs, names)
elif kind == "throughput-readme":
    throughput_readme(groups, fs, args[1:] or ["cargo", "cargo-vendored", "zed"])
elif kind == "readme":
    readme(results, fs)
elif kind == "geomean":
    geomean(results, fs)
