#!/usr/bin/env python3
"""Race Scarlet against other runtimes on the programs in this directory.

Each benchmark is a directory `benches/<name>/` holding the same program in
each language (`<name>.sca`, `.lua`, `.py`, `.js`, `.erl`) and `expected.txt`,
the output every one of them must print. `startup` is the empty program: its
cost is taken off every other benchmark, so what is left is the cost of
running the program and not of starting the runtime or compiling the source.

    benches/run.py                         # every benchmark, every runtime
    benches/run.py fib nbody               # only these
    benches/run.py --runtimes scarlet,luajit-interp
    benches/run.py --scarlet new=target/release/scarlet --scarlet old=/path/to/scarlet
    benches/run.py --json out.json         # save this run
    benches/run.py --baseline out.json     # and compare a later one with it

Judge a change by the instruction counts, which are printed first. On macOS
`/usr/bin/time -l` counts the machine instructions a program retired, and that
count repeats to about one part in a thousand from run to run, so a change
smaller than that is not a change. Cycles are printed
beside them: they follow the instructions but also move with the caches and
with whatever else the machine is doing. Time is printed last, and on a busy
machine it is noise.

A Scarlet binary that takes `scarlet run --counters` is also run once more
for the VM's own counts: how many VM instructions ran, of which kinds, and
how many heap cells were made and reused. Those are exact. They are the same
on every run of the same program with the same input, on any machine. That
run is made with `--audit` too, so a benchmark whose reference counts go wrong
fails the whole race.

Runs are taken in turn across runtimes and the smallest of each measure is
kept, because noise only ever adds.
"""

import argparse
import json
import math
import platform
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
DARWIN = platform.system() == "Darwin"
# What the ratios are against: the fastest interpreter with no JIT we know of.
REFERENCE = "luajit-interp"
# The measures taken off by the startup program's own. Peak memory is not one.
LESS_STARTUP = ("seconds", "instructions", "cycles")
# The VM's counts that are plain numbers. `ops` is a count per kind beside them.
COUNTERS = ("instructions", "cells_made", "cells_reused", "cells_live")


class Runtime:
    def __init__(self, name, ext, tool, command):
        self.name = name
        self.ext = ext
        self.tool = tool
        self.command = command

    def present(self):
        return shutil.which(self.tool) is not None or Path(self.tool).is_file()

    def is_scarlet(self):
        return self.ext == "sca"


def erlang(build):
    def command(source):
        out = build / source.stem
        out.mkdir(parents=True, exist_ok=True)
        subprocess.run(["erlc", "-o", str(out), str(source)], check=True)
        return ["erl", "-noshell", "-pa", str(out), "-run", source.stem, "main", "-s", "init", "stop"]

    return command


def runtimes(scarlets, build):
    made = [
        Runtime(label, "sca", path, lambda source, path=path: [path, "run", str(source)])
        for label, path in scarlets
    ]
    made += [
        Runtime("luajit-interp", "lua", "luajit", lambda s: ["luajit", "-joff", str(s)]),
        Runtime("luajit-jit", "lua", "luajit", lambda s: ["luajit", str(s)]),
        Runtime("python", "py", "python3", lambda s: ["python3", str(s)]),
        Runtime("node-jitless", "js", "node", lambda s: ["node", "--jitless", str(s)]),
        Runtime("beam-jit", "erl", "erl", erlang(build)),
    ]
    return made


def measure(command):
    """Run `command` once. Returns its stdout and what `/usr/bin/time -l` measured."""
    done = subprocess.run(["/usr/bin/time", "-l", *command], capture_output=True, text=True)
    if done.returncode != 0:
        raise RuntimeError(f"{' '.join(command)} exited {done.returncode}:\n{done.stderr}")

    def count(what):
        return int(re.search(rf"(\d+)\s+{what}", done.stderr).group(1))

    return done.stdout, {
        "seconds": float(re.search(r"([\d.]+) user", done.stderr).group(1)),
        "instructions": count("instructions retired"),
        "cycles": count("cycles elapsed"),
        "peak_bytes": count("maximum resident set size"),
    }


def measure_portable(command):
    """`measure` for hosts without `/usr/bin/time -l`: time and memory, no instructions or cycles."""
    import resource

    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    done = subprocess.run(command, capture_output=True, text=True)
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    if done.returncode != 0:
        raise RuntimeError(f"{' '.join(command)} exited {done.returncode}:\n{done.stderr}")
    # ru_maxrss of RUSAGE_CHILDREN is the largest child so far, so it is only
    # an upper bound for this one.
    return done.stdout, {
        "seconds": after.ru_utime - before.ru_utime,
        "instructions": None,
        "cycles": None,
        "peak_bytes": after.ru_maxrss * 1024,
    }


def counts_its_runs(runtime, build):
    """Whether this Scarlet binary takes `run --counters`. One from before the flag does not."""
    probe = Path(build) / f"{runtime.name}.probe.json"
    command = [runtime.tool, "run", "--counters", str(probe), str(HERE / "startup" / "startup.sca")]
    return subprocess.run(command, capture_output=True).returncode == 0 and probe.is_file()


def counters(runtime, source, expected, build):
    """The VM's own counts for one run of `source`. They are exact, so one run is enough.

    The run is not timed, so it is also audited: a benchmark that ends with a
    heap cell nothing holds, or gives a cell up twice, fails here.
    """
    out = Path(build) / f"{runtime.name}.{source.stem}.counters.json"
    command = [runtime.tool, "run", "--audit", "--counters", str(out), str(source)]
    done = subprocess.run(command, capture_output=True, text=True)
    if done.returncode != 0:
        raise RuntimeError(f"{' '.join(command)} exited {done.returncode}:\n{done.stderr}")
    if done.stdout != expected:
        raise RuntimeError(f"{source.stem} on {runtime.name} printed the wrong thing while counting")
    return json.loads(out.read_text())


def race(bench, chosen, counting, runs, build):
    """The smallest of each measure for every runtime that has this benchmark."""
    expected = (bench / "expected.txt").read_text()
    commands = {}
    for runtime in chosen:
        source = bench / f"{bench.name}.{runtime.ext}"
        if source.is_file():
            commands[runtime.name] = runtime.command(source)
    best = {}
    for _ in range(runs):
        for name, command in commands.items():
            stdout, got = (measure if DARWIN else measure_portable)(command)
            if stdout != expected:
                raise RuntimeError(
                    f"{bench.name} on {name} printed the wrong thing.\nwanted:\n{expected}\ngot:\n{stdout}"
                )
            seen = best.setdefault(name, got)
            for what, n in got.items():
                if n is not None:
                    seen[what] = min(seen[what], n)
    for runtime in counting:
        source = bench / f"{bench.name}.{runtime.ext}"
        if runtime.name in best and source.is_file():
            best[runtime.name]["counters"] = counters(runtime, source, expected, build)
    return best


def less(got, base):
    """`got` with `base` taken off, and never below zero. Either may be missing."""
    return None if got is None or base is None else max(got - base, 0)


def without_startup(result, startup):
    """Take the empty program's cost off each runtime's numbers."""
    cleaned = {}
    for name, got in result.items():
        base = startup.get(name)
        cleaned[name] = dict(got)
        if base is None:
            continue
        for what in LESS_STARTUP:
            cleaned[name][what] = less(got[what], base[what])
        if "counters" in got and "counters" in base:
            ours, theirs = got["counters"], base["counters"]
            cleaned[name]["counters"] = {what: less(ours[what], theirs[what]) for what in COUNTERS}
            cleaned[name]["counters"]["ops"] = {
                op: less(n, theirs["ops"].get(op, 0)) for op, n in ours["ops"].items()
            }
    return cleaned


def table(title, names, results, cell):
    lines = [f"\n{title}", "| benchmark | " + " | ".join(names) + " |", "|---|" + "---:|" * len(names)]
    for bench, result in results.items():
        cells = [cell(result[name]) if name in result else "" for name in names]
        lines.append(f"| {bench} | " + " | ".join(cells) + " |")
    return lines


def geomean(logs):
    return math.exp(sum(logs) / len(logs)) if logs else None


def ratios(names, results, measure_name, note=""):
    """Each runtime's cost over the reference's, per benchmark, with a geometric mean.

    A run without the reference has no ratios to print.
    """
    if REFERENCE not in names:
        return []
    lines = [
        f"\n{measure_name} relative to {REFERENCE} (lower is better){note}",
        "| benchmark | " + " | ".join(names) + " |",
        "|---|" + "---:|" * len(names),
    ]
    logs = {name: [] for name in names}
    for bench, result in results.items():
        base = result.get(REFERENCE, {}).get(measure_name)
        cells = []
        for name in names:
            got = result.get(name, {}).get(measure_name)
            if not base or not got:
                cells.append("")
                continue
            cells.append(f"{got / base:.2f}")
            logs[name].append(math.log(got / base))
        lines.append(f"| {bench} | " + " | ".join(cells) + " |")
    means = [f"{geomean(l):.2f}" if l else "" for l in logs.values()]
    lines.append("| **geomean** | " + " | ".join(means) + " |")
    return lines


def change(before, now):
    """How far a count moved, as text. A count that was 0 has no percentage to move by."""
    if before:
        return f"{100 * (now / before - 1):+.2f}%"
    return f"new, {now:+,}" if now else "+0.00%"


def saved_run(baseline):
    """Whether `baseline` has the shape `--json` writes: each benchmark's result for each runtime."""
    if not isinstance(baseline, dict):
        return False
    results = baseline.get("results")
    return (
        isinstance(results, dict)
        and isinstance(baseline.get("startup", {}), dict)
        and all(
            isinstance(got, dict) and all(isinstance(one, dict) for one in got.values())
            for got in results.values()
        )
    )


def earlier_name(name, baseline, others):
    """The runtime in a saved run to compare Scarlet binary `name` with.

    It is the one saved under the same label. A saved run with one Scarlet
    binary in it is compared whatever that binary was called, so a run of
    `new=...` can be judged against a board saved as `scarlet`.
    """
    saved = set(baseline.get("startup", {}))
    for result in baseline.get("results", {}).values():
        saved |= set(result)
    if name in saved:
        return name
    scarlets = sorted(saved - others)
    return scarlets[0] if len(scarlets) == 1 else None


def against(path, baseline, scarlets, others, results):
    """How far each Scarlet binary's counts moved from the saved run's, per benchmark."""
    lines = []
    for name in scarlets:
        earlier = earlier_name(name, baseline, others)
        if earlier is None:
            print(f"{path} has no one Scarlet binary to compare {name} with", file=sys.stderr)
            continue
        was = {bench: got[earlier] for bench, got in baseline["results"].items() if earlier in got}
        pairs = {
            bench: (was[bench], got[name])
            for bench, got in results.items()
            if bench in was and name in got
        }
        told = f"{name} now against {earlier} in {path}"
        lines += moved(
            f"instructions less startup, millions: {told}",
            pairs,
            lambda got: got.get("instructions"),
            1e6,
        )
        counted = {
            bench: (a["counters"], b["counters"])
            for bench, (a, b) in pairs.items()
            if isinstance(a.get("counters"), dict) and isinstance(b.get("counters"), dict)
        }
        if counted:
            lines += moved(
                f"VM instructions less startup, millions: {told}",
                counted,
                lambda c: c.get("instructions"),
                1e6,
            )
            lines += moved(
                f"heap cells made less startup, thousands: {told}",
                counted,
                lambda c: c.get("cells_made"),
                1e3,
            )
    return lines


def moved(title, pairs, read, unit):
    """A table of one count before and now, with the change and its geometric mean."""
    lines = [f"\n{title}", "| benchmark | before | now | change |", "|---|---:|---:|---:|"]
    logs = []
    left_out = []
    for bench, (a, b) in pairs.items():
        before, now = read(a), read(b)
        if before is None or now is None:
            continue
        if before and now:
            logs.append(math.log(now / before))
        elif before or now:
            left_out.append(bench)
        lines.append(f"| {bench} | {before / unit:.1f} | {now / unit:.1f} | {change(before, now)} |")
    mean = geomean(logs)
    lines.append(f"| **geomean** | | | {100 * (mean - 1):+.2f}% |" if mean else "| **geomean** | | | |")
    if left_out:
        lines.append(
            f"\nThe geomean leaves out {', '.join(left_out)}: a count that is 0 before or now has no ratio."
        )
    return lines


def most_run(ops, total, top=4):
    """The kinds of instruction that ran most, each with its share of the run."""
    if not total:
        return ""
    ranked = sorted(ops.items(), key=lambda kind: -kind[1])[:top]
    return ", ".join(f"{op} {100 * n / total:.0f}%" for op, n in ranked if n)


def vm_counters(name, results):
    """One Scarlet binary's own counts, per benchmark."""
    lines = [
        f"\nVM counters less startup: {name}",
        "| benchmark | VM instructions, millions | machine instructions for each | "
        "cells made, thousands | cells reused, thousands | ran most |",
        "|---|---:|---:|---:|---:|---|",
    ]
    for bench, result in results.items():
        got = result.get(name, {})
        counted = got.get("counters")
        if counted is None:
            continue
        ran = counted["instructions"]
        each = f"{got['instructions'] / ran:.1f}" if ran and got.get("instructions") else ""
        lines.append(
            f"| {bench} | {ran / 1e6:.1f} | {each} | {counted['cells_made'] / 1e3:.1f} | "
            f"{counted['cells_reused'] / 1e3:.1f} | {most_run(counted['ops'], ran)} |"
        )
    return lines


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("benchmarks", nargs="*", help="benchmark names (default: all)")
    parser.add_argument("--runs", type=int, default=5, help="runs of each program (default 5)")
    parser.add_argument(
        "--runtimes",
        help="comma-separated runtime names (default: all installed). "
        "`scarlet` is every Scarlet binary, and those are always raced",
    )
    parser.add_argument(
        "--scarlet",
        action="append",
        metavar="LABEL=PATH",
        help="a scarlet binary to race, may be given more than once (default scarlet=target/release/scarlet)",
    )
    parser.add_argument("--json", metavar="PATH", help="also write the numbers here")
    parser.add_argument(
        "--baseline",
        metavar="PATH",
        help="a file an earlier run wrote with --json: also print how far each "
        "benchmark's instruction count has moved from it",
    )
    args = parser.parse_args()

    scarlets = []
    for spec in args.scarlet or [f"scarlet={ROOT / 'target' / 'release' / 'scarlet'}"]:
        label, _, path = spec.partition("=")
        if not path:
            parser.error(f"--scarlet wants LABEL=PATH, got {spec}")
        if not Path(path).is_file():
            parser.error(f"no scarlet binary at {path}; run cargo build --release")
        scarlets.append((label, str(Path(path).resolve())))

    baseline = None
    if args.baseline:
        try:
            baseline = json.loads(Path(args.baseline).read_text())
        except (OSError, ValueError) as e:
            parser.error(f"cannot read the baseline {args.baseline}: {e}")
        if not saved_run(baseline):
            parser.error(f"the baseline {args.baseline} is not a run saved with --json")

    with tempfile.TemporaryDirectory() as build:
        every = runtimes(scarlets, Path(build))
        others = {r.name for r in every if not r.is_scarlet()}
        wanted = args.runtimes.split(",") if args.runtimes else None
        if wanted:
            unknown = set(wanted) - {r.name for r in every} - {"scarlet"}
            if unknown:
                parser.error(f"unknown runtime {', '.join(sorted(unknown))}")
        chosen = [r for r in every if (wanted is None or r.name in wanted or r.is_scarlet())]
        for runtime in chosen:
            if not runtime.present():
                print(f"skipping {runtime.name}: {runtime.tool} is not installed", file=sys.stderr)
        chosen = [r for r in chosen if r.present()]
        counting = [r for r in chosen if r.is_scarlet() and counts_its_runs(r, build)]
        for runtime in chosen:
            if runtime.is_scarlet() and runtime not in counting:
                print(f"{runtime.name} has no `run --counters`, so no VM counters for it", file=sys.stderr)

        found = sorted(p for p in HERE.iterdir() if (p / "expected.txt").is_file())
        if args.benchmarks:
            missing = set(args.benchmarks) - {p.name for p in found}
            if missing:
                parser.error(f"no benchmark named {', '.join(sorted(missing))}")
        picked = [p for p in found if p.name != "startup" and (not args.benchmarks or p.name in args.benchmarks)]

        print("startup", file=sys.stderr)
        startup = race(HERE / "startup", chosen, counting, args.runs, build)
        results = {}
        for bench in picked:
            print(bench.name, file=sys.stderr)
            results[bench.name] = without_startup(race(bench, chosen, counting, args.runs, build), startup)

    names = [r.name for r in chosen]

    def millions(what):
        return lambda got: f"{got[what] / 1e6:.0f}"

    lines = []
    if baseline is not None and not DARWIN:
        print("a baseline is compared by instruction counts, which this host cannot take", file=sys.stderr)
    if DARWIN:
        lines += table("startup, millions of instructions", names, {"startup": startup}, millions("instructions"))
        lines += table("instructions less startup, millions", names, results, millions("instructions"))
        lines += ratios(names, results, "instructions")
        lines += table("cycles less startup, millions", names, results, millions("cycles"))
        lines += ratios(names, results, "cycles")
        if baseline is not None:
            lines += against(args.baseline, baseline, [r.name for r in chosen if r.is_scarlet()], others, results)
    for runtime in counting:
        lines += vm_counters(runtime.name, results)
    lines += table("peak memory, MB", names, results, lambda got: f"{got['peak_bytes'] / 2**20:.0f}")
    noisy = " (noisy: do not judge a change by time)"
    lines += table(f"startup, ms{noisy}", names, {"startup": startup}, lambda got: f"{got['seconds'] * 1000:.0f}")
    lines += table(f"time less startup, ms{noisy}", names, results, lambda got: f"{got['seconds'] * 1000:.0f}")
    lines += ratios(names, results, "seconds", noisy)
    print("\n".join(lines))

    if args.json:
        Path(args.json).write_text(json.dumps({"startup": startup, "results": results}, indent=2) + "\n")


if __name__ == "__main__":
    main()
