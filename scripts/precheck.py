#!/usr/bin/env python3
"""Cheap pre-push checks for rtrash that need no cargo build.

Each check mirrors a mistake that cost a CI round (see the agent rule table
in AGENTS.md). Run from the repo root:

    python3 scripts/precheck.py            # whole tree
    python3 scripts/precheck.py --no-fmt   # skip the cargo fmt step
    python3 scripts/precheck.py --base origin/main   # also diff-aware checks

When cargo fmt works (a toolchain with rustfmt), it also runs
`cargo fmt --all --check`, which formats in memory and builds nothing.

Exit 1 on any error. Warnings print but do not fail.
"""
import re, shutil, subprocess, sys
from pathlib import Path
from collections import defaultdict

ROOT = Path(__file__).resolve().parent.parent
SUBS = ["put", "empty", "list", "status", "trash-stats", "du", "restore",
        "undo", "rm", "who", "reap", "finder", "wsl", "config"]
SRC = {s: f"src/{s.replace('-', '_')}.rs" for s in SUBS}
MULTI = [("trash-put", "put"), ("trash-empty", "empty"), ("trash-list", "list"),
         ("trash-restore", "restore"), ("trash-rm", "rm")]
errors, warnings = [], []


def read(rel):
    return (ROOT / rel).read_text(encoding="utf-8")


def help_text(sub):
    """The Unix HELP const of the module that runs SUB, as printed."""
    body = read(SRC[sub])
    m = re.search(r'(?m)^const HELP: &str = "\\\n(.*?)(?<!\\)";', body, re.S)
    if not m:
        if sub != "wsl":  # wsl prints a cfg-dependent help; the cargo test covers it
            warnings.append(f"no HELP const in {SRC[sub]}; completion check skipped")
        return ""
    return m.group(1).replace("\\n\\\n", "\n").replace("\\\n", "").replace('\\"', '"')


def help_long_opts(sub):
    opts = set()
    for line in help_text(sub).splitlines():
        if not line.startswith(" "):
            continue
        rest = line.lstrip()
        if len(rest) > 4 and rest.startswith("-") and rest[2] == ",":
            rest = rest[3:].lstrip()
        if rest.startswith("--"):
            name = re.match(r"[A-Za-z0-9-]*", rest[2:]).group(0)
            if name:
                opts.add(name)
    return sorted(opts)


def array_block(script, name):
    out, inside = [], False
    for line in script.splitlines():
        if not inside:
            inside = line.lstrip().startswith(f"{name}=(")
            continue
        if line.strip() == ")":
            return "\n".join(out)
        out.append(line)
    return None


def fish_lines(script, sub):
    in_loop = sub in ("list", "status")
    return "\n".join(l for l in script.splitlines()
                     if f"string match -q {sub}'" in l
                     or (sub == "put" and "$put_cond" in l)
                     or (in_loop and 'string match -q $sub"' in l))


def fish_multicall_lines(script, name):
    if name == "trash-put":
        out, inside = [], False
        for line in script.splitlines():
            if line.startswith("for cmd in trash-put"):
                inside = True
            elif inside and line.strip() == "end":
                break
            elif inside:
                out.append(line)
        return "\n".join(out)
    return "\n".join(l for l in script.splitlines() if l.startswith(f"complete -c {name} "))


def fish_has(lines, opt):
    for l in lines.split("\n"):
        w = l.split(" -d ")[0].split()
        if any(f"{a} {b}" == f"-l {opt}" for a, b in zip(w, w[1:])):
            return True
    return False


def nu_block(script, name):
    out, inside = [], False
    for line in script.splitlines():
        if not inside:
            inside = line == f'export extern "{name}" ['
            continue
        if line == "]":
            return "\n".join(out)
        out.append(line)
    return None


def check_completions():
    bash, zsh = read("completions/rtrash.bash"), read("completions/_rtrash")
    fish, nu = read("completions/rtrash.fish"), read("completions/rtrash.nu")
    pwsh, elv = read("completions/rtrash.ps1"), read("completions/rtrash.elv")
    man = read("man/rtrash.1") if (ROOT / "man/rtrash.1").exists() else ""
    opts = {s: help_long_opts(s) for s in SUBS}
    for sub in SUBS:
        u = sub.replace("-", "_")
        b, z = array_block(bash, f"_rtrash_{u}_opts"), array_block(zsh, f"{u}_opts")
        f = fish_lines(fish, sub)
        for o in opts[sub]:
            flag = f"--{o}"
            if b is not None and not any(w.rstrip("=") == flag for w in b.split()):
                errors.append(f"bash: --{o} of `rtrash {sub}` missing from _rtrash_{u}_opts in completions/rtrash.bash")
            if z is not None and flag not in z:
                errors.append(f"zsh: --{o} of `rtrash {sub}` missing from {u}_opts in completions/_rtrash")
            if o not in ("help", "version") and not fish_has(f, o):
                errors.append(f"fish: `complete ... -l {o}` missing for `rtrash {sub}` in completions/rtrash.fish")
            for name, txt in (("pwsh", pwsh), ("elvish", elv)):
                if flag not in txt:
                    errors.append(f"{name}: {flag} (rtrash {sub}) appears nowhere in completions/rtrash.{ 'ps1' if name=='pwsh' else 'elv'}")
            if man and flag not in man.replace("\\-", "-"):
                errors.append(f"man: {flag} (rtrash {sub}) not documented in man/rtrash.1")
    for name, sub in [(f"rtrash {s}", s) for s in SUBS] + MULTI:
        blk = nu_block(nu, name)
        if blk is None:
            continue
        for o in opts[sub]:
            words = {re.match(r"[A-Za-z0-9-]*", l.lstrip()).group(0) for l in blk.splitlines()}
            if f"--{o}" not in words:
                errors.append(f'nu: --{o} missing from extern "{name}" in completions/rtrash.nu')
    for name, sub in MULTI:
        lines = fish_multicall_lines(fish, name)
        for o in opts[sub]:
            if not fish_has(lines, o):
                errors.append(f"fish: `complete -c {name} -l {o}` missing (multicall of `rtrash {sub}`) in completions/rtrash.fish")


def check_test_tags():
    """Two tests that build a temp tree with the same literal tag share a dir."""
    pat = re.compile(r'\b(Tree::new|tmpdir|scratch|temp_tree|TestDir::new)\(\s*"([^"]+)"')
    for path in sorted((ROOT / "tests").glob("*.rs")):
        seen = defaultdict(list)
        for i, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            for m in pat.finditer(line):
                seen[(m.group(1), m.group(2))].append(i)
        for (ctor, tag), lines in seen.items():
            if len(lines) > 1:
                errors.append(f"{path.relative_to(ROOT)}:{lines[1]}: {ctor}(\"{tag}\") also used at line {lines[0]}; "
                              "tests run in parallel and share the temp dir, pick a unique tag")


# Hand-built temp paths in tests/ (pid + name) as of the scratch_dir
# helper. New tests take tests/common::scratch_dir; this only shrinks.
PID_PATHS = {"cli.rs": 6, "completions_parity.rs": 3, "reflink_fs.rs": 1,
             "trash_hostile.rs": 1, "tui_logic.rs": 2, "windows_completions.rs": 1,
             "windows_mount.rs": 17, "windows_names.rs": 11, "windows_process.rs": 3,
             "windows_space.rs": 12, "wsl_route.rs": 1}


def check_pid_paths():
    for path in sorted((ROOT / "tests").glob("*.rs")):
        n = path.read_text(encoding="utf-8").count("process::id()")
        allowed = PID_PATHS.get(path.name, 0)
        if n > allowed:
            errors.append(f"tests/{path.name}: {n} uses of process::id() (allowed {allowed}); "
                          "build scratch paths with common::scratch_dir(prefix, tag) from tests/common/mod.rs")
        elif n < allowed:
            warnings.append(f"tests/{path.name}: process::id() count fell to {n}; lower PID_PATHS in scripts/precheck.py")


# A sleep of a second or more in tests/ is a guess at the whole-second
# trash clock; common::next_trash_second waits for the real tick.
LONG_SLEEP = re.compile(r"sleep\(\s*(?:std::time::)?Duration::from_(?:secs|millis\((\d[\d_]*)\))")


def check_long_sleeps():
    for path in sorted((ROOT / "tests").glob("*.rs")):
        for i, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            for m in LONG_SLEEP.finditer(line):
                ms = m.group(1)
                if ms is None or int(ms.replace("_", "")) >= 1000:
                    errors.append(f"tests/{path.name}:{i}: a sleep of a second or more; trash times are "
                                  "whole seconds, so call common::next_trash_second() between puts")


def strip_comments(s):
    s = re.sub(r'"(?:\\.|[^"\\])*"', '""', s)
    return re.sub(r"//[^\n]*", "", s)


def check_fn_params(limit=7):
    """clippy::too_many_arguments fires above 7 parameters (self excluded)."""
    sig = re.compile(r"\bfn\s+(\w+)\s*(<[^>{]*>)?\s*\(")
    for path in sorted((ROOT / "src").rglob("*.rs")) + sorted((ROOT / "tests").glob("*.rs")):
        text = strip_comments(path.read_text(encoding="utf-8"))
        for m in sig.finditer(text):
            depth, i, start = 1, m.end(), m.end()
            while depth and i < len(text):
                depth += {"(": 1, ")": -1, "<": 0}.get(text[i], 0)
                i += 1
            args, d, cur = [], 0, ""
            for ch in text[start:i - 1]:
                if ch in "(<[{":
                    d += 1
                elif ch in ")>]}":
                    d -= 1
                if ch == "," and d == 0:
                    args.append(cur); cur = ""
                else:
                    cur += ch
            args.append(cur)
            args = [a.strip() for a in args if a.strip() and not re.fullmatch(r"&?('\w+\s+)?(mut\s+)?self", a.strip())]
            if len(args) > limit:
                before = text[:m.start()]
                allowed = "too_many_arguments" in before[-300:]
                if not allowed:
                    line = before.count("\n") + 1
                    errors.append(f"{path.relative_to(ROOT)}:{line}: fn {m.group(1)} takes {len(args)} parameters "
                                  f"(clippy limit {limit}); group the options in a struct (see finder::ListShape)")


def check_long_lines(base):
    """rustfmt rewraps lines over 100 columns; flag added ones so the push is fmt-clean."""
    if not base:
        return
    diff = subprocess.run(["git", "diff", "-U0", base, "--", "*.rs"], cwd=ROOT,
                          capture_output=True, text=True).stdout
    f = None
    for line in diff.splitlines():
        if line.startswith("+++ "):
            f = line[6:]
        elif line.startswith("+") and len(line) - 1 > 100:
            warnings.append(f"{f}: added line of {len(line) - 1} columns; rustfmt will rewrap it")


def check_removed_literals(base):
    """A string literal removed from product code but still in a test is likely a stale assert."""
    if not base:
        return
    diff = subprocess.run(["git", "diff", "-U0", base, "--", "src"], cwd=ROOT,
                          capture_output=True, text=True).stdout
    removed, added = set(), set()
    for line in diff.splitlines():
        for tok in re.findall(r'"(?:\\.|[^"\\])*"', line[1:]):
            lit = tok[1:-1]
            if len(lit) < 6 or not re.search(r"[A-Za-z]{3}", lit):
                continue
            (removed if line.startswith("-") else added if line.startswith("+") else set()).add(lit)
    gone = removed - added
    if not gone:
        return
    files = [p for p in list((ROOT / "src").rglob("*.rs")) + list((ROOT / "tests").rglob("*.rs"))]
    for lit in sorted(gone):
        for p in files:
            t = p.read_text(encoding="utf-8")
            if "src" in p.relative_to(ROOT).parts[:1]:
                t = t[t.find("#[cfg(test)]"):] if "#[cfg(test)]" in t else ""
            if lit in t:
                warnings.append(f"{p.relative_to(ROOT)}: still contains removed literal \"{lit}\"; "
                                "update the assert or the example (tests/windows_mount.rs runs only in CI)")


def check_rustfmt():
    """`cargo fmt --check` when a toolchain is there; three rounds in a row
    (170-172) needed a format-only amend after CI."""
    cargo = shutil.which("cargo")
    ok = cargo and subprocess.run([cargo, "fmt", "--version"], cwd=ROOT,
                                  capture_output=True).returncode == 0
    if not ok:
        warnings.append("cargo fmt not available; rustfmt check skipped (CI still runs it)")
        return
    r = subprocess.run([cargo, "fmt", "--all", "--check"], cwd=ROOT,
                       capture_output=True, text=True, timeout=600)
    if r.returncode != 0:
        files = sorted(set(re.findall(r"(?m)^Diff in (.+?):\d+:", r.stdout)))
        where = ", ".join(str(Path(f).relative_to(ROOT)) if f.startswith(str(ROOT)) else f
                          for f in files) or (r.stderr.strip() or "see cargo fmt --check")
        errors.append(f"cargo fmt --check failed; run `cargo fmt` (unformatted: {where})")


def main():
    base = sys.argv[sys.argv.index("--base") + 1] if "--base" in sys.argv else None
    check_completions(); check_test_tags(); check_pid_paths(); check_long_sleeps()
    check_fn_params()
    check_long_lines(base); check_removed_literals(base)
    if "--no-fmt" not in sys.argv:
        check_rustfmt()
    for w in warnings:
        print("warning:", w)
    for e in errors:
        print("error:", e)
    print(f"precheck: {len(errors)} error(s), {len(warnings)} warning(s)")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
