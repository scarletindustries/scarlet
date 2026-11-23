#!/usr/bin/env python3
"""Race Scarlet against other runtimes on the programs in this directory.

Each benchmark is a directory `benches/<name>/` holding the same program in
each language (`<name>.sca`, `.lua`, `.py`, `.js`, `.erl`) and `expected.txt`,
the output every one of them must print. `startup` is the empty program: its
cost is taken off every other benchmark, so what is left is the time spent
running the program and not starting the runtime or compiling the source.

    benches/run.py                         # every benchmark, every runtime
    benches/run.py fib nbody               # only these
    benches/run.py --runtimes scarlet,luajit-interp
    benches/run.py --scarlet new=target/release/scarlet --scarlet old=/path/to/scarlet
    benches/run.py --json out.json

Runs are taken in turn across runtimes and the smallest of each measure is
kept, because noise on a busy machine only ever adds. On macOS it also counts
instructions, which repeat far better than time does. Nothing else should be
running: a compile going on next to it will move every number.
"""

import argparse
import json
import math
import os
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


class Runtime:
    def __init__(self, name, ext, tool, command):
        self.name = name
        self.ext = ext
        self.tool = tool
        self.command = command

    def present(self):
        return shutil.which(self.tool) is not None or Path(self.tool).is_file()


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
    """Run `command` once. Returns (stdout, user seconds, instructions, peak bytes)."""
    done = subprocess.run(["/usr/bin/time", "-l", *command], capture_output=True, text=True)
    if done.returncode != 0:
        raise RuntimeError(f"{' '.join(command)} exited {done.returncode}:\n{done.stderr}")
    user = float(re.search(r"([\d.]+) user", done.stderr).group(1))
    instructions = int(re.search(r"(\d+)\s+instructions retired", done.stderr).group(1))
    peak = int(re.search(r"(\d+)\s+maximum resident set size", done.stderr).group(1))
    return done.stdout, user, instructions, peak


def measure_portable(command):
    """`measure` for hosts without `/usr/bin/time -l`: time and memory, no instruction count."""
    import resource

    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    done = subprocess.run(command, capture_output=True, text=True)
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    if done.returncode != 0:
        raise RuntimeError(f"{' '.join(command)} exited {done.returncode}:\n{done.stderr}")
    # ru_maxrss of RUSAGE_CHILDREN is the largest child so far, so it is only
    # an upper bound for this one.
    return done.stdout, after.ru_utime - before.ru_utime, None, after.ru_maxrss * 1024


def race(bench, chosen, runs):
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
            stdout, user, instructions, peak = (measure if DARWIN else measure_portable)(command)
            if stdout != expected:
                raise RuntimeError(
                    f"{bench.name} on {name} printed the wrong thing.\nwanted:\n{expected}\ngot:\n{stdout}"
                )
            seen = best.setdefault(name, {"seconds": user, "instructions": instructions, "peak_bytes": peak})
            seen["seconds"] = min(seen["seconds"], user)
            seen["peak_bytes"] = min(seen["peak_bytes"], peak)
            if instructions is not None:
                seen["instructions"] = min(seen["instructions"], instructions)
    return best


def without_startup(result, startup):
    """Take the empty program's cost off each runtime's numbers."""
    cleaned = {}
    for name, got in result.items():
        base = startup.get(name)
        if base is None:
            cleaned[name] = dict(got)
            continue
        cleaned[name] = {
            "seconds": max(got["seconds"] - base["seconds"], 0.0),
            "instructions": None
            if got["instructions"] is None
            else max(got["instructions"] - base["instructions"], 0),
            "peak_bytes": got["peak_bytes"],
        }
    return cleaned


def table(title, names, results, cell):
    lines = [f"\n{title}", "| benchmark | " + " | ".join(names) + " |", "|---|" + "---:|" * len(names)]
    for bench, result in results.items():
        cells = [cell(result[name]) if name in result else "" for name in names]
        lines.append(f"| {bench} | " + " | ".join(cells) + " |")
    return lines


def ratios(names, results, measure_name):
    """Each runtime's cost over the reference's, per benchmark, with a geometric mean."""
    lines = [
        f"\n{measure_name} relative to {REFERENCE} (lower is better)",
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
    means = [f"{math.exp(sum(l) / len(l)):.2f}" if l else "" for l in logs.values()]
    lines.append("| **geomean** | " + " | ".join(means) + " |")
    return lines


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("benchmarks", nargs="*", help="benchmark names (default: all)")
    parser.add_argument("--runs", type=int, default=5, help="runs of each program (default 5)")
    parser.add_argument("--runtimes", help="comma-separated runtime names (default: all installed)")
    parser.add_argument(
        "--scarlet",
        action="append",
        metavar="LABEL=PATH",
        help="a scarlet binary to race, may be given more than once (default scarlet=target/release/scarlet)",
    )
    parser.add_argument("--json", metavar="PATH", help="also write the numbers here")
    args = parser.parse_args()

    scarlets = []
    for spec in args.scarlet or [f"scarlet={ROOT / 'target' / 'release' / 'scarlet'}"]:
        label, _, path = spec.partition("=")
        if not path:
            parser.error(f"--scarlet wants LABEL=PATH, got {spec}")
        if not Path(path).is_file():
            parser.error(f"no scarlet binary at {path}; run cargo build --release")
        scarlets.append((label, str(Path(path).resolve())))

    with tempfile.TemporaryDirectory() as build:
        every = runtimes(scarlets, Path(build))
        wanted = args.runtimes.split(",") if args.runtimes else None
        if wanted:
            unknown = set(wanted) - {r.name for r in every}
            if unknown:
                parser.error(f"unknown runtime {', '.join(sorted(unknown))}")
        chosen = [r for r in every if (wanted is None or r.name in wanted or r.ext == "sca")]
        for runtime in chosen:
            if not runtime.present():
                print(f"skipping {runtime.name}: {runtime.tool} is not installed", file=sys.stderr)
        chosen = [r for r in chosen if r.present()]

        found = sorted(p for p in HERE.iterdir() if (p / "expected.txt").is_file())
        if args.benchmarks:
            missing = set(args.benchmarks) - {p.name for p in found}
            if missing:
                parser.error(f"no benchmark named {', '.join(sorted(missing))}")
        picked = [p for p in found if p.name != "startup" and (not args.benchmarks or p.name in args.benchmarks)]

        print("startup", file=sys.stderr)
        startup = race(HERE / "startup", chosen, args.runs)
        results = {}
        for bench in picked:
            print(bench.name, file=sys.stderr)
            results[bench.name] = without_startup(race(bench, chosen, args.runs), startup)

    names = [r.name for r in chosen]
    lines = table("startup, ms", names, {"startup": startup}, lambda got: f"{got['seconds'] * 1000:.0f}")
    lines += table("time less startup, ms", names, results, lambda got: f"{got['seconds'] * 1000:.0f}")
    lines += ratios(names, results, "seconds")
    if DARWIN:
        lines += table(
            "instructions less startup, millions",
            names,
            results,
            lambda got: f"{got['instructions'] / 1e6:.0f}",
        )
        lines += ratios(names, results, "instructions")
    lines += table("peak memory, MB", names, results, lambda got: f"{got['peak_bytes'] / 2**20:.0f}")
    print("\n".join(lines))

    if args.json:
        Path(args.json).write_text(json.dumps({"startup": startup, "results": results}, indent=2) + "\n")


if __name__ == "__main__":
    main()
