#!/usr/bin/env python3
"""Cheap pre-push checks for rtrash that need no cargo build.

Each check mirrors a mistake that cost a CI round (see the agent rule table
in AGENTS.md). Run from the repo root:

    python3 scripts/precheck.py            # whole tree
    python3 scripts/precheck.py --no-fmt   # skip the cargo fmt step
    python3 scripts/precheck.py --base origin/main   # also diff-aware checks, codespell
    python3 scripts/precheck.py --commit-msg FILE    # the commit-msg hook only

When cargo fmt works (a toolchain with rustfmt), it also runs
`cargo fmt --all --check`, which formats in memory and builds nothing.

Exit 1 on any error. Warnings print but do not fail.
"""
import fnmatch, re, shutil, subprocess, sys
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


# A loop on the system clock in tests/ waits for the wrong clock: a
# Finder item's date is its change time, stamped by the file system's
# clock, which trails the system clock (round 208). common::next_trash_second
# waits on both.
CLOCK_WAIT = re.compile(r"\bwhile\b[^{]*(?:epoch_secs\(\)|SystemTime::now\(\))")


def check_clock_waits():
    for path in sorted((ROOT / "tests").glob("*.rs")):
        for i, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            if CLOCK_WAIT.search(line):
                errors.append(f"tests/{path.name}:{i}: a loop that waits on the system clock; call "
                              "common::next_trash_second(), which also waits on the file system's clock")


# An assert that a test finished within a few seconds of wall time fails
# on a loaded runner: next_trash_second_waits_for_a_later_second asserted
# under 1.5 s and a busy macOS runner went past it (round 215). Assert the
# behaviour instead (count the polls through a seam), and keep a time
# bound only as a hang guard of HANG_GUARD_MS or more.
HANG_GUARD_MS = 5000
TIGHT_ELAPSED = re.compile(
    r"\bassert!\([^;]*?\belapsed\(\)(?:\.as_(secs|millis)\(\))?\s*<=?\s*"
    r"(?:(?:std::time::)?Duration::from_(secs|millis)\(\s*(\d[\d_]*)\s*\)|(\d[\d_]*)\b)")


def check_tight_elapsed():
    for path in sorted((ROOT / "tests").glob("*.rs")):
        text = path.read_text(encoding="utf-8")
        for m in TIGHT_ELAPSED.finditer(text):
            unit = m.group(2) or m.group(1) or "secs"
            n = int((m.group(3) or m.group(4)).replace("_", ""))
            ms = n * 1000 if unit == "secs" else n
            if ms < HANG_GUARD_MS:
                line = text.count("\n", 0, text.index("elapsed()", m.start())) + 1
                errors.append(f"tests/{path.name}:{line}: asserts elapsed() under {ms} ms, which a loaded runner "
                              f"can exceed; assert the behaviour, or keep a hang guard of {HANG_GUARD_MS} ms or more")


IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
RAW_OPEN = re.compile(r'(#*)"')


def rust_code(s, strings=False):
    """Rust source with its comments gone and its char literals written
    as `' '`. String literals become `""` unless `strings` keeps them.
    Every newline stays, so line numbers hold. One pass, left to right:
    a '"' char literal, a lifetime, a raw string and a nested block
    comment each end where Rust ends them (round 221; the regexes this
    replaces lost step after a '"' and blanked code as string)."""
    out, i, n = [], 0, len(s)
    lines_in = lambda t: "\n" * t.count("\n")
    while i < n:
        c = s[i]
        if c == "/" and s.startswith("//", i):
            j = s.find("\n", i)
            i = n if j < 0 else j
        elif c == "/" and s.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if s.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif s.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            out.append(" " + lines_in(s[i:j]))
            i = j
        elif c == '"':
            j = i + 1
            while j < n and s[j] != '"':
                j += 2 if s[j] == "\\" else 1
            j = min(j + 1, n)
            out.append(s[i:j] if strings else '""' + lines_in(s[i:j]))
            i = j
        elif c == "'":
            if s.startswith("\\", i + 1):
                j = s.find("'", i + 3)  # past the escaped char, so '\'' ends right
                j = n if j < 0 else j + 1
            elif i + 2 < n and s[i + 2] == "'" and s[i + 1] != "\n":
                j = i + 3
            else:
                out.append(c)  # a lifetime or a loop label
                i += 1
                continue
            out.append("' '")
            i = j
        elif c == "_" or c.isalpha():
            m = IDENT.match(s, i)
            if not m:  # a non-ASCII letter: not an identifier start Rust cares about here
                out.append(c)
                i += 1
                continue
            word, j = m.group(0), m.end()
            raw = RAW_OPEN.match(s, j) if word in ("r", "br", "cr") else None
            if raw:
                close = '"' + raw.group(1)
                k = s.find(close, raw.end())
                k = n if k < 0 else k + len(close)
                out.append(s[i:k] if strings else word + '""' + lines_in(s[i:k]))
                i = k
            else:
                out.append(word)  # b"..", c"..", b'.' go on as a string or char
                i = j
        else:
            out.append(c)
            i += 1
    return "".join(out)


# Known cases the shared blanker must get right; check_rust_code runs them
# before any rule that reads its output.
RUST_CODE_CASES = [
    ("let q = '\"'; let v = \"x\"; fn after() {}",
     "let q = ' '; let v = \"\"; fn after() {}"),
    ("w.replace('\"', \"\\\\\\\"\")\nfn after() {}",  # src/reap_timer.rs hid launchd_plist
     "w.replace(' ', \"\")\nfn after() {}"),
    ("s.replace('\\'', r\"'\\''\"); fn after() {}", "s.replace(' ', r\"\"); fn after() {}"),
    ("x.push('\\\\'); fn after() {}", "x.push(' '); fn after() {}"),
    ("b'\"' => out.push(b'\"'), b\"a\\\"b\" fn after", "b' ' => out.push(b' '), b\"\" fn after"),
    ("fn f<'a>(x: &'a str) -> &'a str { 'outer: loop {} }",
     "fn f<'a>(x: &'a str) -> &'a str { 'outer: loop {} }"),
    ("r#\"a \" // b\"# fn after", "r\"\" fn after"),
    ("a /* x /* y */ z */ b // c\nfn after", "a   b \nfn after"),
    ("\"one\ntwo\" fn after", "\"\"\n fn after"),
    ("'\\u{1F600}' fn after", "' ' fn after"),
]


def check_rust_code():
    for src, want in RUST_CODE_CASES:
        got = rust_code(src)
        if got != want:
            errors.append(f"scripts/precheck.py: rust_code({src!r}) gave {got!r}, not {want!r}; "
                          "the source blanker the checks share is broken")



MUSL_DEPRECATED = re.compile(r"\blibc::(time_t|suseconds_t)\b")


def check_musl_aliases():
    """libc deprecates the `time_t` and `suseconds_t` aliases on musl,
    since musl 1.2 widened both to 64 bits, so naming either fails musl
    clippy under -D warnings (round 225: src/info.rs). Write `as _` and
    let the libc call or struct field give the width."""
    for top in ("src", "tests"):
        for path in sorted((ROOT / top).rglob("*.rs")):
            rel = path.relative_to(ROOT).as_posix()
            text = rust_code(path.read_text(encoding="utf-8"))
            for m in MUSL_DEPRECATED.finditer(text):
                line = text.count("\n", 0, m.start()) + 1
                errors.append(f"{rel}:{line}: names libc::{m.group(1)}, which libc deprecates on musl; "
                              "cast with `as _` so the call or field gives the width")

# libc functions whose listed parameters (0-based) are `*const`: a
# `&mut x` there trips clippy::unnecessary_mut_passed, and clippy does not
# run on the box (round 228: `&mut ws` into openpty's winsize). Add a
# function here when code starts passing it a pointer; check the libc
# source for each parameter's mutability, not only that the function
# exists.
LIBC_CONST_PARAMS = {
    "openpty": {3, 4},
    "forkpty": {2, 3},
    "sigaction": {1},
    "pthread_sigmask": {1},
    "sigprocmask": {1},
    "tcsetattr": {2},
    "setrlimit": {1},
    "utimensat": {2},
    "futimens": {1},
    "nanosleep": {0},
}
LIBC_CALL = re.compile(r"\blibc::(\w+)\s*\(")


def call_args(text, start):
    """The top-level arguments of the call whose `(` ends at `start`."""
    args, depth, cur, i = [], 1, "", start
    while i < len(text) and depth:
        ch = text[i]
        if ch in "([{":
            depth += 1
        elif ch in ")]}":
            depth -= 1
            if not depth:
                break
        if ch == "," and depth == 1:
            args.append(cur)
            cur = ""
        else:
            cur += ch
        i += 1
    args.append(cur)
    return [a.strip() for a in args]


def check_libc_const_args():
    """A `*const` libc parameter takes `&x`, not `&mut x`."""
    for top in ("src", "tests"):
        for path in sorted((ROOT / top).rglob("*.rs")):
            rel = path.relative_to(ROOT).as_posix()
            text = rust_code(path.read_text(encoding="utf-8"))
            for m in LIBC_CALL.finditer(text):
                const = LIBC_CONST_PARAMS.get(m.group(1))
                if not const:
                    continue
                for k, arg in enumerate(call_args(text, m.end())):
                    if k in const and re.match(r"&\s*mut\b", arg):
                        line = text.count("\n", 0, m.start()) + 1
                        errors.append(f"{rel}:{line}: passes `{arg}` to libc::{m.group(1)} parameter {k}, "
                                      "which is *const; pass `&x` (clippy::unnecessary_mut_passed)")


# Calls that write file bytes in the cross-device copy (round 230).
COPY_WRITE = re.compile(r"\bio::copy\(|\.write_all_at\(|\bfs::copy\(")


def check_copy_progress():
    """Each write in the trashdir copy counts its bytes with
    `interrupt::copied` on the same line or the two after, so the
    explorer's progress line sees every copy path."""
    path = ROOT / "src" / "trashdir.rs"
    text = rust_code(path.read_text(encoding="utf-8"))
    cut = re.search(r"#\[cfg\(test\)\]\s*mod tests\b", text)
    lines = text[:cut.start() if cut else len(text)].split("\n")
    for k, line in enumerate(lines):
        if COPY_WRITE.search(line) and not any("interrupt::copied" in l for l in lines[k:k + 3]):
            errors.append(f"src/trashdir.rs:{k + 1}: a copy write without interrupt::copied after it; "
                          "count the bytes so the explorer's progress line moves")


def check_fn_params(limit=7):
    """clippy::too_many_arguments fires above 7 parameters (self excluded)."""
    sig = re.compile(r"\bfn\s+(\w+)\s*(<[^>{]*>)?\s*\(")
    for path in sorted((ROOT / "src").rglob("*.rs")) + sorted((ROOT / "tests").glob("*.rs")):
        text = rust_code(path.read_text(encoding="utf-8"))
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


# A struct that gains a field breaks every literal and pattern of it that
# names its fields one by one and has no `..`. The box cannot compile, and
# round 260 sent a RowCtx field that broke two literals in tui-only tests.
STRUCT_DEF = re.compile(r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?\s+)?struct\s+(\w+)(?:<[^{;()]*>)?\s*\{")
TYPE_BEFORE = re.compile(r"(?:\bstruct|\bimpl|\bfor|\benum|->)\s*"
                         r"(?:&(?:'\w+\s+)?(?:mut\s+)?)?(?:[\w:]*::)?$")
FIELD_DEF = re.compile(r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?\s+)?(\w+)\s*:(?!:)")
STRUCT_FIELD_CASES = [
    ("plain", "struct A {\n    x: u8,\n    pub y: Vec<u8>,\n}\n", {"A": {"x", "y"}}),
    ("generic", "pub(crate) struct B<'a> {\n    /// doc: not a field\n    z: &'a str,\n}\n",
     {"B": {"z"}}),
    ("nested", "struct C {\n    f: fn(a: u8),\n    g: Option<D>,\n}\n", {"C": {"f", "g"}}),
    ("tuple", "struct T(u8);\nstruct U;\n", {}),
    ("twice", "struct E {\n    a: u8,\n}\nmod m {\n    struct E {\n        b: u8,\n    }\n}\n",
     {"E": None}),
]
LITERAL_DRIFT_CASES = [
    ("listed", "let c = Ctx {\n    a: 1,\n    b,\n};\n", []),
    ("missing", "let c = Ctx {\n    a: 1,\n};\n", [1]),
    ("filled", "let c = Ctx {\n    a: 1,\n    ..Ctx::plain()\n};\n", []),
    ("pattern rest", "let Ctx { a, .. } = c;\n", []),
    ("pattern", "let &Ctx { a } = c;\n", [1]),
    ("path", "x(m::Ctx { a: S { b: 1 } });\n", [1]),
    ("declarations", "struct Ctx {\n    a: u8,\n}\nimpl Ctx {\n}\nimpl T for Ctx {\n}\n"
     "fn f() -> Ctx {\n    g()\n}\n", []),
    ("comment", "// Ctx { a } here\n", []),
    ("ref return", "fn f(&self) -> &Ctx {\n    &self.c\n}\nfn g(&mut self) -> &'a mut Ctx {\n"
     "    &mut self.c\n}\n", []),
    ("other name", "let c = MyCtx {\n    a: 1,\n};\n", []),
]


def brace_body(text, open_at):
    """The text between the `{` at `open_at` and its match, with nested
    braces and their contents left out."""
    depth, out = 0, []
    for c in text[open_at:]:
        if c == "{":
            depth += 1
        elif c == "}":
            depth -= 1
            if depth == 0:
                break
        elif depth == 1:
            out.append(c)
    return "".join(out)


def struct_fields(text):
    """{name: field names} of the structs with braces in `text`; a name
    defined twice maps to None, as the literals cannot be told apart."""
    out = {}
    for m in STRUCT_DEF.finditer(text):
        fields = set(FIELD_DEF.findall(brace_body(text, m.end() - 1)))
        out[m.group(1)] = None if m.group(1) in out else fields
    return out


def literal_drift_lines(text, name, fields):
    """1-based lines of `name { ... }` literals or patterns that name their
    fields one by one, with no `..`, and leave out one of `fields`."""
    hits = []
    for m in re.finditer(rf"(?<![\w])(?:\w+::)*{name}\s*\{{", text):
        head = text[text.rfind("\n", 0, m.start()) + 1:m.start()]
        if head.lstrip().startswith("//") or TYPE_BEFORE.search(head):
            continue
        body = brace_body(text, m.end() - 1)
        if ".." in body:
            continue
        if any(not re.search(rf"(?<![\w.]){f}\b", body) for f in fields):
            hits.append(text.count("\n", 0, m.start()) + 1)
    return hits


def check_struct_field_drift(base):
    """For each struct in src that gains fields in the range, every literal
    or pattern of it in src or tests names the new fields or has a `..`.
    The box cannot compile, so this catches what cargo would refuse
    (E0063, E0027). STRUCT_FIELD_CASES and LITERAL_DRIFT_CASES test it."""
    for what, text, want in STRUCT_FIELD_CASES:
        if struct_fields(text) != want:
            errors.append(f"scripts/precheck.py: struct_fields gets STRUCT_FIELD_CASES '{what}' "
                          "wrong; the field-drift check is broken")
    for what, text, want in LITERAL_DRIFT_CASES:
        if literal_drift_lines(text, "Ctx", {"a", "b"}) != want:
            errors.append(f"scripts/precheck.py: literal_drift_lines gets LITERAL_DRIFT_CASES "
                          f"'{what}' wrong; the field-drift check is broken")
    if not base:
        return
    changed = subprocess.run(["git", "diff", "--name-only", base, "--", "src"], cwd=ROOT,
                             capture_output=True, text=True).stdout.split()
    files = sorted((ROOT / "src").rglob("*.rs")) + sorted((ROOT / "tests").rglob("*.rs"))
    texts = {p.relative_to(ROOT).as_posix(): p.read_text(encoding="utf-8") for p in files}
    now = {}
    for rel, text in texts.items():
        if rel.startswith("src/"):
            for name, fields in struct_fields(text).items():
                now[name] = None if name in now else fields
    added = {}
    for rel in changed:
        if not rel.endswith(".rs") or rel not in texts:
            continue
        old = subprocess.run(["git", "show", f"{base}:{rel}"], cwd=ROOT,
                             capture_output=True, text=True)
        if old.returncode != 0:
            continue
        for name, fields in struct_fields(old.stdout).items():
            if fields is not None and now.get(name) and now[name] - fields:
                added[name] = now[name] - fields
    for name, new in sorted(added.items()):
        for rel, text in texts.items():
            for line in literal_drift_lines(text, name, new):
                errors.append(f"{rel}:{line}: {name} gained {', '.join(sorted(new))} in this range, "
                              "but this literal or pattern names neither it nor a `..`; cargo "
                              "would refuse it (E0063 or E0027)")


# A conventional-commit prefix: `feat(du): ...`, `fix: ...`, `ci!: ...`.
# Subjects here are plain sentences that start with a capital (round 209).
TYPED_SUBJECT = re.compile(r"^[a-z][a-z0-9-]*(?:\([^)]*\))?!?:\s")


def subject_error(subject):
    if TYPED_SUBJECT.match(subject):
        return (f"commit subject '{subject}' starts with a type prefix; write a plain "
                "sentence instead, as in 'Read zstd-compressed ncdu exports in du -f'")
    return None


def check_commit_subjects(base):
    """Every commit after BASE has a plain-sentence subject."""
    if not base:
        return
    log = subprocess.run(["git", "log", "--no-merges", "--format=%s", f"{base}..HEAD"],
                         cwd=ROOT, capture_output=True, text=True)
    if log.returncode != 0:
        warnings.append(f"git log {base}..HEAD failed; commit subjects not checked")
        return
    for subject in log.stdout.splitlines():
        e = subject_error(subject)
        if e:
            errors.append(e)


def commit_msg_hook(path):
    """`--commit-msg FILE`, the prek commit-msg hook: check the subject of
    the message git is about to commit, skipping its comment lines."""
    text = Path(path).read_text(encoding="utf-8", errors="replace")
    lines = (l for l in text.splitlines() if not l.startswith("#"))
    e = subject_error(next((l for l in lines if l.strip()), ""))
    if e:
        print("error:", e)
        return 1
    return 0


def codespell_hook():
    """The codespell version, arguments and the global exclude that
    prek.toml gives the pre-commit hook, or None when there is no hook."""
    import tomllib
    cfg = tomllib.loads((ROOT / "prek.toml").read_text(encoding="utf-8"))
    exclude = re.compile(cfg.get("exclude", r"(?!)"))
    for repo in cfg.get("repos", []):
        for hook in repo.get("hooks", []) if "codespell" in str(repo.get("repo", "")) else []:
            if hook.get("id") == "codespell":
                return str(repo.get("rev", "")).lstrip("v"), hook.get("args", []), exclude
    return None


def check_codespell(base):
    """Under `--base`, codespell on the files changed since BASE, with the
    version and arguments of the pre-commit hook in prek.toml (round 233:
    the hook flagged a word in a test after every box check had passed).
    codespell comes from PATH, or at the hook's version through uvx; with
    neither, or when it cannot start, a note says nothing was checked."""
    if not base:
        return
    hook = codespell_hook()
    if hook is None:
        errors.append("precheck codespell check found no codespell hook in prek.toml; "
                      "the rule needs updating")
        return
    rev, args, exclude = hook
    skips = [g for a in args if a.startswith("--skip=") for g in a[len("--skip="):].split(",")]
    git = lambda *a: subprocess.run(["git", *a], cwd=ROOT, capture_output=True,
                                    text=True).stdout.splitlines()
    names = set(git("diff", "--name-only", "--diff-filter=ACMR", base))
    names |= set(git("ls-files", "--others", "--exclude-standard"))
    files = sorted(n for n in names if (ROOT / n).is_file() and not exclude.search(n)
                   and not any(fnmatch.fnmatch(n, g) or fnmatch.fnmatch(Path(n).name, g)
                               for g in skips))
    if not files:
        return
    if shutil.which("codespell"):
        cmd = ["codespell"]
    elif shutil.which("uvx"):
        cmd = ["uvx", f"codespell=={rev}" if rev else "codespell"]
    else:
        print("note: codespell and uvx not found; codespell check skipped (pre-commit still runs it)")
        return
    try:
        r = subprocess.run(cmd + args + ["--", *files], cwd=ROOT, capture_output=True,
                           text=True, timeout=600)
    except (OSError, subprocess.TimeoutExpired) as e:
        print(f"note: codespell did not run ({e}); codespell check skipped")
        return
    found = [l.strip() for l in r.stdout.splitlines() if "==>" in l]
    for line in found:
        errors.append(f"codespell: {line}; fix the word, or add it to the hook's -L list "
                      "in prek.toml when it is a real name")
    if r.returncode != 0 and not found:
        print(f"note: codespell did not run (exit {r.returncode}): "
              f"{(r.stderr or r.stdout).strip()[:300]}")


# Each string literal in Rust source, comments skipped: a raw string
# (r"..", r#".."#, br"..") or a plain one (b".." keeps its b as code).
LITERAL = re.compile(r'(?<!\w)b?r(#*)"(.*?)"\1|"((?:\\.|[^"\\])*)"', re.S)
FORMAT_ARG = re.compile(r"\{[^{}]*\}")


def rust_literals(text):
    """(line, body) of each string literal in `text`."""
    code = rust_code(text, strings=True)
    for m in LITERAL.finditer(code):
        body = m.group(2) if m.group(2) is not None else m.group(3)
        yield code.count("\n", 0, m.start()) + 1, body


def literal_parts(body):
    """The fixed text of a literal: a format string splits at each `{..}`,
    so `"{n} could not be listed"` gives `" could not be listed"`."""
    return [s for s in FORMAT_ARG.split(body) if s]


def split_literals(rel, text):
    """(product, test) literals of one file, each as (where, body). All of
    tests/ is test code; in src, the `#[cfg(test)]` modules are."""
    tests = None if rel.startswith("tests/") else test_mod_lines(text.splitlines())
    product, test = [], []
    for line, body in rust_literals(text):
        side = test if tests is None or line - 1 in tests else product
        side.append((f"{rel}:{line}", body))
    return product, test


def stale_literals(base_product, head_product, test_lits):
    """The test literals that only gone product text could match: each is
    part of a string src had at the base and part of none it has now. A
    string that moved, or that lives on inside another, is not gone."""
    head = {p for _, b in head_product for p in literal_parts(b)}
    gone = {p for _, b in base_product for p in literal_parts(b)} - head
    if not gone:
        return []
    gone_text, head_text = "\0".join(sorted(gone)), "\0".join(sorted(head))
    return [(where, part) for where, body in test_lits for part in literal_parts(body)
            if len(part) >= 6 and re.search(r"[A-Za-z]{3}", part)
            and part in gone_text and part not in head_text]


# Known cases for stale_literals: (src at the base, src now, test strings,
# the parts it must report). Round 237 moved a writer and got 73 warnings
# for "rtrash" and "snapshot", which other src strings still held.
STALE_CASES = [
    # A message moved to another file: the same string is still in src.
    (["could not be listed"], ["could not be listed"], ["could not be listed"], []),
    # A dropped fallback name lives on inside a longer message.
    (["snapshot", "an older snapshot or a gdu export"], ["an older snapshot or a gdu export"],
     ["snapshot"], []),
    # A test string longer than the gone one asserts something else.
    (["rtrash"], [], ["rtrash du"], []),
    # A reworded message the test still asserts, matched through formats.
    (["{n} records could not be listed"], ["{n} records were skipped"],
     ["could not be listed", "{} records were skipped"], ["could not be listed"]),
    # Only fixed text counts: a test that names the argument alone is fine.
    (["cannot scan {path}: {e}"], ["cannot read {path}: {e}"], ["{path}"], []),
]


def check_stale_cases():
    for base, head, tests, want in STALE_CASES:
        lit = lambda xs: [("case", x) for x in xs]
        got = [part for _, part in stale_literals(lit(base), lit(head), lit(tests))]
        if got != want:
            errors.append(f"scripts/precheck.py: stale_literals({base!r}, {head!r}, {tests!r}) "
                          f"gave {got!r}, not {want!r}; the stale-string check is broken")


# A diff hunk can add lines between an unchanged outer attribute and the
# item it was on. The new lines then take the attribute, and that item
# loses it. Round 240 put a new test under an existing
# `#[cfg(all(unix, feature = "tui"))]`, and the old test, now built
# everywhere, failed to find pick_rows on Windows and without features.
HUNK = re.compile(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@")


def attr_depth(text):
    """How many `[` stay open at the end of `text` (strings and comments
    are not told apart, which attributes rarely need)."""
    return text.count("[") - text.count("]")


def harmless_run(run):
    """Added lines that leave the item below with its attributes: only
    attributes, comments and blank lines."""
    depth = 0
    for text in run:
        t = text.strip()
        if depth > 0:
            depth += attr_depth(t)
            continue
        if not t or t.startswith("//"):
            continue
        if t.startswith("#[") and not t.startswith("#!["):
            depth = attr_depth(t)
            continue
        return False
    return True


def split_attribute_hits(diff, mutant=None):
    """(file, line, attribute) for each run of added lines that a diff
    puts between an unchanged outer attribute and its item. A run of
    attributes or comments alone is fine, and so is a run that ends with
    the very attributes above it: git drew a correct insertion one item
    early. `mutant` breaks one rule on purpose, for the self-test."""
    opens = ("#[", "#![") if mutant == "inner" else ("#[",)
    hits = []
    path, line = None, 0
    stack, opened, depth = [], [], 0
    run, run_stack = [], []

    def flush():
        nonlocal run, run_stack
        if run and run_stack:
            attrs = [t.strip() for _, t in run_stack if t.strip()]
            tail = [t.strip() for t in run if t.strip()][-len(attrs):]
            shifted = mutant != "no-shift" and tail == attrs
            harmless = mutant != "any-run" and harmless_run(run)
            if not shifted and not harmless:
                at, text = next((n, t) for n, t in reversed(run_stack)
                                if t.strip().startswith(opens))
                hits.append((path, at, text.strip()))
        run, run_stack = [], []

    for raw in diff.splitlines():
        if raw.startswith("+++ "):
            flush()
            path = raw[6:] if raw.startswith("+++ b/") else raw[4:]
            stack, opened, depth = [], [], 0
            continue
        if raw.startswith("--- ") or raw.startswith("diff ") or raw.startswith("index "):
            continue
        m = HUNK.match(raw)
        if m:
            flush()
            line, stack, opened, depth = int(m.group(1)), [], [], 0
            continue
        if raw.startswith("\\"):
            continue
        kind, text = raw[:1], raw[1:]
        if kind == "+":
            if not run:
                run_stack = list(stack) if not opened and any(
                    t.strip().startswith(opens) for _, t in stack) else []
            run.append(text)
            line += 1
            continue
        flush()
        if kind == "-":
            stack, opened, depth = [], [], 0
            continue
        if kind != " ":
            continue
        t = text.strip()
        if opened:
            opened.append((line, text))
            depth += attr_depth(t)
            if depth <= 0:
                stack += opened
                opened = []
        elif t.startswith(opens):
            d = attr_depth(t)
            if d > 0 and mutant != "single-line":
                opened, depth = [(line, text)], d
            elif t.endswith("]"):
                stack.append((line, text))
            else:
                stack = []
        elif stack and (not t or t.startswith("//")):
            stack.append((line, text))
        else:
            stack = []
        line += 1
    flush()
    return hits


def hunk(*lines, at=10):
    """A one-file diff of `lines`, each with its ' ', '+' or '-' mark."""
    return "\n".join(["+++ b/src/x.rs", f"@@ -{at},9 +{at},9 @@", *lines]) + "\n"


# Known cases for split_attribute_hits: (what, diff, the lines it flags).
SPLIT_CASES = [
    ("round 240: a test added under an existing cfg",
     hunk("     #[cfg(all(unix, feature = \"tui\"))]", "+    #[test]", "+    fn new() {}",
          "+", "     #[test]", "     fn old() {}"), [10]),
    ("a test added above the attribute",
     hunk("     }", "+    #[test]", "+    fn new() {}", "+", "     #[cfg(unix)]",
          "     #[test]"), []),
    ("git draws a correct insertion one item early",
     hunk("     #[cfg(unix)]", "+    #[test]", "+    fn new() {}", "+    ",
          "+    #[cfg(unix)]", "     #[test]", "     fn old() {}"), []),
    ("an attribute added to the item",
     hunk("     #[cfg(unix)]", "+    #[allow(dead_code)]", "     fn old() {}"), []),
    ("a cfg over several lines",
     hunk(" #[cfg(any(", "     unix,", "     windows", " ))]", "+fn new() {}", " fn old() {}"),
     [10]),
    ("the item itself replaced",
     hunk(" #[cfg(unix)]", "-fn old() {}", "+fn new() {}"), []),
    ("an inner attribute covers the whole module",
     hunk(" #![cfg(unix)]", "+use std::fs;", " use std::io;"), []),
    ("an attribute and its item on one line",
     hunk(" #[cfg(unix)] use std::fs;", "+use std::io;"), []),
    ("a doc comment between the cfg and the item",
     hunk(" #[cfg(unix)]", " /// Old.", "+fn new() {}", " fn old() {}"), [10]),
    ("cfg_attr",
     hunk(" #[cfg_attr(test, allow(dead_code))]", "+fn new() {}", " fn old() {}"), [10]),
]
SPLIT_MUTANTS = ["no-shift", "any-run", "single-line", "inner"]


def check_split_cases():
    """The rule meets every case, and each broken matcher misses one."""
    def failures(mutant):
        return [what for what, diff, want in SPLIT_CASES
                if [n for _, n, _ in split_attribute_hits(diff, mutant)] != want]
    for what in failures(None):
        errors.append(f"scripts/precheck.py: split_attribute_hits gets SPLIT_CASES '{what}' "
                      "wrong; the attribute check is broken")
    for mutant in SPLIT_MUTANTS:
        if not failures(mutant):
            errors.append(f"scripts/precheck.py: SPLIT_CASES no longer catch the '{mutant}' "
                          "mutant of split_attribute_hits; add a case that does")


def check_split_attributes(base):
    """Under `--base`, lines a diff adds between an unchanged attribute
    and the item it was on (round 240)."""
    if not base:
        return
    diff = subprocess.run(["git", "diff", base, "--", "*.rs"], cwd=ROOT,
                          capture_output=True, text=True).stdout
    for path, line, attr in split_attribute_hits(diff):
        errors.append(f"{path}:{line}: lines added between the unchanged `{attr}` and the "
                      "item it was on; they take the attribute and that item loses it. "
                      "Put the new lines above the attribute.")


def base_rust_files(base):
    """{path: text} of every .rs file under src and tests at `base`, or
    None when git cannot read it."""
    ls = subprocess.run(["git", "ls-tree", "-r", "--name-only", base, "--", "src", "tests"],
                        cwd=ROOT, capture_output=True, text=True)
    if ls.returncode:
        return None
    names = [n for n in ls.stdout.splitlines() if n.endswith(".rs")]
    cat = subprocess.run(["git", "cat-file", "--batch"], cwd=ROOT, capture_output=True,
                         input="".join(f"{base}:{n}\n" for n in names).encode())
    out, at, files = cat.stdout, 0, {}
    for n in names:
        end = out.index(b"\n", at)
        size = int(out[at:end].split()[2])
        files[n] = out[end + 1:end + 1 + size].decode("utf-8", "replace")
        at = end + 2 + size
    return files


def check_removed_literals(base):
    """A test that asserts a message src no longer prints is a stale assert.
    It compares whole string literals at the base with the tree now, so a
    message that moved or that another string still holds does not count;
    only test strings that match nothing src prints now are reported."""
    if not base:
        return
    files = base_rust_files(base)
    if files is None:
        return
    base_product = [lit for rel, t in files.items() if rel.startswith("src/")
                    for lit in split_literals(rel, t)[0]]
    head_product, test_lits = [], []
    for p in sorted(list((ROOT / "src").rglob("*.rs")) + list((ROOT / "tests").rglob("*.rs"))):
        rel = p.relative_to(ROOT).as_posix()
        product, test = split_literals(rel, p.read_text(encoding="utf-8"))
        head_product += product if rel.startswith("src/") else []
        test_lits += test
    for where, part in stale_literals(base_product, head_product, test_lits):
        warnings.append(f"{where}: asserts \"{part}\", which src printed at {base} and no "
                        "longer does; update the assert or the example "
                        "(tests/windows_mount.rs runs only in CI)")


REMOVAL = re.compile(r"(?<!permanent_guard::)\b(remove_any_path|remove_path|remove_any|remove_forcing|"
                     r"wipe_two_parallel|wipe_children_parallel|wipe_children_except|purge_all|"
                     r"remove_dir_all|remove_dir|remove_file|remove_record)\s*\(")
ANY_FN = re.compile(r"^(\s*)(?:pub(?:\([^)]*\))?\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?"
                    r"(?:extern\s+\"[^\"]*\"\s+)?fn\s+(\w+)")

# Every call to a removal primitive outside permanent_guard, by (file under
# src, enclosing fn): how many calls the fn makes and why it may skip
# permanent_guard::{remove, remove_record}. A new call, in a new fn or in
# a listed one, fails until it goes through the guard or the entry is
# updated with its reason (round 194: two Windows restores hand-rolled
# gate() plus remove_forcing).
PRIMITIVE = "the removal primitive itself, which permanent_guard calls"
RECORD = ("the record of an item whose payload permanent_guard::remove just took: "
          "the item counts once against the cap")
OWN = "a file rtrash wrote itself (cache, temp or link), not user data"
MOVE = "a move: the source goes only after the whole copy is in place"
BATCH = "a batch the caller counted up front with permanent_guard::gate_many"
PERMANENT_ALLOW = {
    ("du_dupes.rs", "clone_file"): (2, OWN + ": the clone target it just created"),
    ("du_dupes.rs", "link_one"): (2, OWN + ": the .rtrash-link temp beside the duplicate"),
    ("empty.rs", "empty_one"): (1, RECORD),
    ("empty.rs", "full_empty_wipe"): (1, BATCH),
    ("empty_tui.rs", "permanently_remove_entries"): (1, RECORD),
    ("fastdelete.rs", "remove_dir_tree"): (2, PRIMITIVE),
    ("file_replace.rs", "replace_with"): (2, OWN + ": the .NAME.PID.tmp it writes beside the file it replaces"),
    ("fastdelete.rs", "remove_path"): (2, PRIMITIVE),
    ("fastdelete.rs", "wipe_children_parallel"): (1, PRIMITIVE),
    ("fastdelete.rs", "wipe_pair"): (1, PRIMITIVE),
    ("finder_total.rs", "prune_stale"): (1, OWN),
    ("reap_timer.rs", "remove"): (1, OWN + ": the timer unit files it installed"),
    ("restore.rs", "restore_one_to"): (1, "the record of the restored item"),
    ("retention.rs", "remove_entry"): (1, RECORD),
    ("rm.rs", "run_inner"): (1, RECORD),
    ("setup.rs", "link_multicall"): (1, OWN),
    ("setup.rs", "run_setup"): (2, OWN),
    ("trash_total.rs", "forget"): (1, OWN),
    ("trashdir.rs", "directorysizes_rewrite"): (1, OWN),
    ("trashdir.rs", "remove_any"): (2, PRIMITIVE + ", and the move's source removal"),
    ("trashdir.rs", "remove_any_path"): (1, PRIMITIVE),
    ("trashdir.rs", "remove_record"): (1, PRIMITIVE),
    ("trashdir.rs", "move_by_copy"): (2, MOVE + "; and the part it copied, when the copy fails"),
    ("trashdir.rs", "reserve_in_place"): (1, "a put taking back the record it could not write"),
    ("trashdir.rs", "reserve_record"): (2, OWN + ": the .rtrash-put temporary it wrote the record into"),
    ("trashdir.rs", "trash_move_locked"): (1, "a put undoing its own record when the move fails"),
    ("tui_term.rs", "tempfile_unlinked"): (1, OWN),
    ("win_recycle.rs", "copy_tree"): (2, OWN + ": the part copy it just made, when the copy fails"),
    ("win_recycle.rs", "empty_run"): (1, BATCH),
    ("win_recycle.rs", "prune_after_put"): (1, BATCH),
    ("win_recycle.rs", "make_junction"): (1, OWN + ": the empty folder it just made, when the "
                                          "reparse data does not take"),
    ("win_recycle.rs", "purge_oldest_on"): (1, BATCH),
    ("win_recycle.rs", "remove_retry"): (3, PRIMITIVE + " (under remove_forcing)"),
    ("win_recycle.rs", "test_bin_put"): (1, OWN + ": the $I record it just wrote, when the move fails"),
    ("win_recycle.rs", "restore_payload"): (3, MOVE + "; then the $I record of the restored item"),
    ("win_recycle.rs", "rm_run"): (1, BATCH),
    ("wsl_bin.rs", "remove_entry"): (1, RECORD),
}


def test_mod_lines(lines):
    """Line numbers inside `#[cfg(test)] mod x { ... }` blocks."""
    test_lines, k = set(), 0
    while k < len(lines):
        m = re.match(r"(\s*)(pub )?mod \w+ \{", lines[k + 1]) if k + 1 < len(lines) else None
        if lines[k].strip() == "#[cfg(test)]" and m:
            close = m.group(1) + "}"
            j = k + 1
            while j < len(lines) and lines[j].rstrip() != close:
                j += 1
            test_lines.update(range(k, j + 1))
            k = j
        k += 1
    return test_lines


def check_permanent_gate():
    """Outside permanent_guard, a removal primitive is called only through
    permanent_guard::{remove, remove_record} or from a fn PERMANENT_ALLOW
    names, with its count and reason."""
    seen, first = {}, {}
    for path in sorted((ROOT / "src").rglob("*.rs")):
        if path.name == "permanent_guard.rs":
            continue
        rel = path.relative_to(ROOT / "src").as_posix()
        text = path.read_text(encoding="utf-8")
        lines, codes = text.splitlines(), rust_code(text).splitlines()
        test_lines, stack = test_mod_lines(lines), []
        for k, line in enumerate(lines):
            if stack and line.rstrip() == stack[-1][0] + "}":
                stack.pop()
            code = codes[k] if k < len(codes) else ""
            m = ANY_FN.match(code)
            fn = m.group(2) if m else (stack[-1][1] if stack else "<top>")
            if m:
                code = code[m.end():]  # the name defined is not a call; a one-line body is
                if not code.rstrip().endswith((";", "}")):
                    stack.append((m.group(1), fn))
            if k in test_lines:
                continue
            n = len(REMOVAL.findall(code))
            if n:
                key = (rel, fn)
                seen[key] = seen.get(key, 0) + n
                first.setdefault(key, k + 1)
    for key in sorted(set(seen) | set(PERMANENT_ALLOW)):
        got, (want, _) = seen.get(key, 0), PERMANENT_ALLOW.get(key, (0, ""))
        if got == want:
            continue
        rel, fn = key
        where = f"src/{rel}:{first[key]}" if key in first else f"src/{rel}"
        if want == 0:
            errors.append(f"{where}: {fn} calls a removal primitive that skips permanent_guard; "
                          "call permanent_guard::remove or remove_record, or list it in "
                          "PERMANENT_ALLOW with a reason")
        else:
            errors.append(f"{where}: {fn} makes {got} removal primitive call(s), PERMANENT_ALLOW "
                          f"lists {want}; route a new one through permanent_guard::remove or "
                          "remove_record, or update the entry and its reason")


SEAM_READ = re.compile(r'\b(?:var|var_os)\s*\(\s*"(RTRASH_TEST_\w*)"')
SEAM_CONST = re.compile(r'\b(?:const|static)\s+\w+\s*:[^=;]*=\s*"(RTRASH_TEST_\w*)"')
SCOPE = re.compile(r"^(\s*)(?:pub(?:\([^)]*\))?\s+)?(?:mod\s+\w+\s*\{|impl\b[^;]*\{\s*$)")
DEBUG_CFG = re.compile(r"#\[cfg\((?:all\()?(?:[^\]]*,\s*)?debug_assertions\b")


def item_attrs(lines, k):
    """The `#[...]` attributes right above line k, past doc comments."""
    attrs, j = [], k - 1
    while j >= 0 and lines[j].strip().startswith(("#[", "///", "//")):
        if lines[j].strip().startswith("#["):
            attrs.append(lines[j].strip())
        j -= 1
    return attrs


def check_test_seams():
    """Every read of an RTRASH_TEST_* variable outside the tests is debug-only:
    its fn (or a mod or impl around it) has #[cfg(debug_assertions)], or
    cfg!(debug_assertions) appears in the fn at or before the read (round
    195; RTRASH_TEST_BIN_DIR points status --fix at any folder)."""
    for path in sorted((ROOT / "src").rglob("*.rs")):
        rel = path.relative_to(ROOT)
        text = path.read_text(encoding="utf-8")
        lines = text.splitlines()
        code = rust_code(text, strings=True).splitlines()
        code += [""] * (len(lines) - len(code))
        test_lines, stack, scope_at = test_mod_lines(lines), [], []
        for k, line in enumerate(code):
            if stack and line.rstrip() == stack[-1][0] + "}":
                stack.pop()
            gated = any(DEBUG_CFG.search(a) for a in item_attrs(lines, k))
            here = list(stack)
            m, sc = ANY_FN.match(line), SCOPE.match(line)
            if m or sc:
                item = ((m or sc).group(1), "fn" if m else "scope", k, gated)
                here.append(item)
                if not (m and line.rstrip().endswith((";", "}"))):
                    stack.append(item)
            scope_at.append(here)
        text = "\n".join(code)
        found = [(m.start(), m.group(1), "read") for m in SEAM_READ.finditer(text)]
        found += [(m.start(), m.group(1), "const") for m in SEAM_CONST.finditer(text)]
        for at, name, kind in sorted(found):
            k = text.count("\n", 0, at)
            if k in test_lines:
                continue
            if kind == "const":
                errors.append(f"{rel}:{k + 1}: {name} is named through a const; read a test seam "
                              "by its literal name so precheck can see its debug gate")
                continue
            scopes = scope_at[k]
            if any(g for _, _, _, g in scopes):
                continue
            fns = [start for _, kind_, start, _ in scopes if kind_ == "fn"]
            if fns and any("cfg!(debug_assertions)" in l for l in code[fns[-1]:k + 1]):
                continue
            errors.append(f"{rel}:{k + 1}: {name} is read without a debug gate; put "
                          "`cfg!(debug_assertions)` before it in the fn or `#[cfg(debug_assertions)]` "
                          "on the fn, so a release build never honours it")


OS_TRASH = re.compile(r"\btrash::(\{[^}]*\b)?delete(_all)?\b")


BIN_ASKED_CASES = [
    ("asked", "pub fn send(p: &Path) {\n    if bin_cannot_take(p).is_some() {}\n"
     "    trash::delete(p);\n}\n", []),
    ("not asked", "pub fn send(p: &Path) {\n    check(p)?;\n    trash::delete(p);\n}\n", [3]),
    ("asked after", "fn send(p: &Path) {\n    trash::delete(p);\n    bin_cannot_take(p);\n}\n",
     [2]),
    ("other fn", "fn ask(p: &Path) {\n    bin_cannot_take(p);\n}\n"
     "fn send(p: &Path) {\n    trash::delete(p);\n}\n", [5]),
]


def unasked_bin_lines(text):
    """1-based lines of trash::delete calls in a top-level fn that has not
    called bin_cannot_take before them."""
    hits, start, asked = [], 0, False
    for n, line in enumerate(text.split("\n")):
        if re.match(r"(?:pub(?:\([^)]*\))?\s+)?fn\s", line):
            start, asked = n, False
        if "bin_cannot_take(" in line:
            asked = True
        if OS_TRASH.search(line) and not (n >= start and asked):
            hits.append(n + 1)
    return hits


def check_os_trash_scoped():
    """Every hand-off to the OS trash goes through allow_roots::trash_delete,
    which checks RTRASH_ALLOW_ROOTS first (round 182: the Windows du browser
    called trash::delete directly and skipped the scope). There it asks
    win_recycle::bin_cannot_take first (round 261): the shell deletes for
    good, without a word, an item the Recycle Bin cannot take, so put, the
    du browser and reap --trash would lose it. BIN_ASKED_CASES tests the
    matcher."""
    for what, text, want in BIN_ASKED_CASES:
        if unasked_bin_lines(text) != want:
            errors.append(f"scripts/precheck.py: unasked_bin_lines gets BIN_ASKED_CASES '{what}' "
                          "wrong; the Recycle Bin check is broken")
    for path in sorted((ROOT / "src").rglob("*.rs")):
        if path.name == "allow_roots.rs":
            code = rust_code(path.read_text(encoding="utf-8"))
            for line in unasked_bin_lines(code):
                errors.append(f"{path.relative_to(ROOT)}:{line}: trash::delete before "
                              "win_recycle::bin_cannot_take in its fn; the shell would delete "
                              "an item the Recycle Bin cannot take for good")
            continue
        for k, line in enumerate(rust_code(path.read_text(encoding="utf-8")).splitlines()):
            if OS_TRASH.search(line):
                errors.append(f"{path.relative_to(ROOT)}:{k + 1}: trash::delete outside allow_roots; "
                              "call crate::allow_roots::trash_delete so the scope is checked")

ITEM_FN = re.compile(r"^(\s{0,4})(?:pub(?:\([^)]*\))?\s+)?fn\s+(\w+)")


def test_fns(text, raw):
    """Top-level fns and impl methods of a test file: name, own cfgs, test
    flag and line range. A body ends at the first `}` line at its indent."""
    lines, fns, attrs = text.splitlines(), [], []
    raw_lines = raw.splitlines()
    impl_cfgs, impl_trait, impl_close = frozenset(), False, None
    k = 0
    while k < len(lines):
        line = lines[k]
        if impl_close is not None and line.rstrip() == impl_close:
            impl_cfgs, impl_trait, impl_close = frozenset(), False, None
            k += 1
            continue
        a = re.match(r"^\s{0,4}#\[(.*)\]\s*$", raw_lines[k] if k < len(raw_lines) else line)
        if a:
            attrs.append(a.group(1).strip())
            k += 1
            continue
        im = re.match(r"^impl\b(.*)\{\s*$", line)
        if im:
            impl_cfgs = frozenset(re.sub(r"\s+", "", x) for x in attrs if x.startswith("cfg("))
            impl_trait = " for " in im.group(1)
            impl_close, attrs = "}", []
            k += 1
            continue
        m = ITEM_FN.match(line)
        if m and impl_close is not None and impl_trait:
            m = None  # a trait method is called through the trait, not by name
        if m:
            close = m.group(1) + "}"
            end = k
            while end < len(lines) and lines[end].rstrip() != close and not lines[end].rstrip().endswith("{}"):
                end += 1
            cfgs = frozenset(re.sub(r"\s+", "", x) for x in attrs if x.startswith("cfg("))
            if impl_close is not None:
                cfgs = cfgs | impl_cfgs
            fns.append({"name": m.group(2), "cfgs": cfgs, "test": "test" in attrs,
                        "start": k, "end": end})
            attrs = []
            k = end + 1
            continue
        if line.strip() and not line.lstrip().startswith("//"):
            attrs = []
        k += 1
    return lines, fns


def check_cfg_helpers():
    """A test helper used only under a cfg it lacks is dead code on the other
    targets, and cross-clippy -D warnings fails there (181b: outside_lines)."""
    for path in sorted((ROOT / "tests").glob("*.rs")):
        raw = path.read_text(encoding="utf-8")
        lines, fns = test_fns(rust_code(raw), raw)
        owner = {}
        for i, f in enumerate(fns):
            for n in range(f["start"], f["end"] + 1):
                owner[n] = i
        callers = defaultdict(list)  # helper index -> caller indexes (None: outside any fn)
        index = {}
        for i, f in enumerate(fns):
            index.setdefault(f["name"], []).append(i)
        for name, ids in index.items():
            if len(ids) != 1:
                continue  # a name defined twice (per-cfg variants) is not judged
            i = ids[0]
            pat = re.compile(rf"\b{re.escape(name)}\b")
            for n, line in enumerate(lines):
                if n == fns[i]["start"] or not pat.search(line):
                    continue
                callers[i].append(owner.get(n))
        eff = [f["cfgs"] if (f["cfgs"] or f["test"]) else None for f in fns]
        for _ in range(len(fns)):
            changed = False
            for i, f in enumerate(fns):
                if f["cfgs"] or f["test"] or not callers[i]:
                    continue
                known = []
                for c in callers[i]:
                    if c is None or c == i:
                        known.append(frozenset() if c is None else None)
                    else:
                        known.append(eff[c])
                known = [k for k in known if k is not None]
                if not known:
                    continue
                new = frozenset.intersection(*known)
                if eff[i] != new:
                    eff[i], changed = new, True
            if not changed:
                break
        # A file-level #![cfg(...)] already gates every fn in the file.
        file_cfgs = frozenset(re.sub(r"\s+", "", m) for m in re.findall(r"(?m)^#!\[(cfg\(.*\))\]\s*$", raw))
        for i, f in enumerate(fns):
            if f["cfgs"] or f["test"] or not eff[i] or not (eff[i] - file_cfgs):
                continue
            gate = " ".join(f"#[{c}]" for c in sorted(eff[i] - file_cfgs))
            errors.append(f"{path.relative_to(ROOT)}:{f['start'] + 1}: fn {f['name']} is used only under {gate} "
                          "and is dead code elsewhere (cross-clippy fails); give it the same cfg")


def check_refusal_helpers():
    """A test that expects a refusal to change nothing goes through
    common::assert_refused_unchanged, which snapshots just before the call
    (181c compared against a stale listing). Flag a hand-rolled copy: a fn
    or closure named *unchanged* that does not call it, or the scope
    refusal text spelled out instead of common::OUTSIDE."""
    for path in sorted((ROOT / "tests").glob("*.rs")):
        rel = path.relative_to(ROOT)
        raw = path.read_text(encoding="utf-8")
        text = rust_code(raw)
        for m in re.finditer(r"(?m)^[ \t]*(?:let\s+(\w*unchanged\w*)\s*=\s*(?:move\s*)?\||fn\s+(\w*unchanged\w*)\b)", text):
            name = m.group(1) or m.group(2)
            line = text.count("\n", 0, m.start()) + 1
            open_at = text.find("{", m.end())
            depth, k = 0, open_at
            while 0 <= k < len(text):
                depth += {"{": 1, "}": -1}.get(text[k], 0)
                if depth == 0:
                    break
                k += 1
            if "common::assert_refused_unchanged(" not in text[open_at:k]:
                errors.append(f"{rel}:{line}: {name} compares before and after by hand; "
                              "call common::assert_refused_unchanged, which takes the snapshot itself")
        for n, l in enumerate(raw.splitlines(), 1):
            if '"is outside RTRASH_ALLOW_ROOTS"' in l and not l.lstrip().startswith("//"):
                errors.append(f"{rel}:{n}: spells out the scope refusal text; "
                              "use common::OUTSIDE or common::outside_lines")


def method_chain(text, at):
    """The `.name(...)` calls that follow position `at`, as one string."""
    i, n = at, len(text)
    while True:
        m = re.compile(r"\s*\.\s*\w+(?:::<[^()]*>)?\s*\(").match(text, i)
        if not m:
            return text[at:i]
        depth, i = 1, m.end()
        while depth and i < n:
            depth += {"(": 1, ")": -1}.get(text[i], 0)
            i += 1


def check_du_rows():
    """A test compares du table rows through common::du_row, which writes
    the AGE cell as `AGE`: a second can tick between two runs, and round
    218 failed when `0s` became `1s`. Flag a row put together by hand from
    split_whitespace() and join, and a row taken with .lines().find() that
    an assert_eq!/assert_ne! compares whole."""
    bound = re.compile(r"\blet\s+(\w+)\s*=\s*[^;|]*?\.lines\(\)\s*\.find\(")
    for path in sorted((ROOT / "tests").glob("*.rs")):
        rel = path.relative_to(ROOT).as_posix()
        text = rust_code(path.read_text(encoding="utf-8"))
        line_of = lambda k: text.count("\n", 0, k) + 1
        for m in re.finditer(r"\.split_whitespace\(\)", text):
            if ".join(" in method_chain(text, m.end()):
                errors.append(f"{rel}:{line_of(m.start())}: joins a row's cells by hand; "
                              "compare du rows through common::du_row, which masks the AGE cell")
        for m in bound.finditer(text):
            name = m.group(1)
            end = re.compile(r"\n(?:#\[|fn |pub fn )").search(text, m.end())
            scope = text[m.end():end.start() if end else len(text)]
            whole = re.compile(rf"assert_(?:eq|ne)!\(\s*(?:{name}\s*,|[^;]*?,\s*{name}\s*[,)])")
            hit = whole.search(scope)
            if hit:
                errors.append(f"{rel}:{line_of(m.end() + hit.start())}: compares the row `{name}` whole; "
                              "take it with common::du_row, which masks the AGE cell")


PAYLOAD_GONE = re.compile(r"\bpermanent_guard::remove\(")
RECORD_GONE = re.compile(r"\bremove_file\(|\bremove_record\(")


def check_record_sections():
    """A fn that reserves a .trashinfo (create_new on a join_info path)
    opens interrupt::critical() too, so a Ctrl-C between the record and
    the payload waits until both are whole (round 220). So does a fn that
    removes a payload and then its record (round 222), so a Ctrl-C never
    leaves a record whose payload is gone."""
    starts = re.compile(r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?\s+)?fn\s+\w+")
    for path in sorted((ROOT / "src").rglob("*.rs")):
        rel = path.relative_to(ROOT).as_posix()
        text = rust_code(path.read_text(encoding="utf-8"))
        cuts = [m.start() for m in starts.finditer(text)] + [len(text)]
        for a, b in zip(cuts, cuts[1:]):
            body = text[a:b]
            if "interrupt::critical()" in body:
                continue
            line = text.count("\n", 0, a) + 1
            if "create_new(true)" in body and "join_info(" in body:
                errors.append(f"{rel}:{line}: reserves a trash record outside interrupt::critical(); "
                              "open the section first so Ctrl-C cannot split the record from its payload")
            elif PAYLOAD_GONE.search(body) and RECORD_GONE.search(body):
                errors.append(f"{rel}:{line}: removes a payload and its record outside interrupt::critical(); "
                              "open the section once the payload is gone so Ctrl-C waits for the record")



# Each fn outside the tests that takes a trash lock, with what it reads
# again under it: a look taken before the lock is stale by the time the
# lock is held (round 244). A new site needs an entry saying so.
LOCK_SITES = {
    ("src/empty.rs", "empty_dangling"): "reads the dangling records again; only counted ones go",
    ("src/empty.rs", "run_inner"): "empty_one scans under the lock and takes only what "
                                   "trashdir::Seen counted before it (round 245)",
    ("src/list.rs", "relock"): "reads the record again; only a Locked entry is acted on",
    ("src/retention.rs", "prune"): "locks every root before it lists and sizes the items; "
                                   "Prune.only spares what an empty did not count",
    ("src/trash_check.rs", "run"): "scans again and keeps what changed (tests::AFTER_SCAN)",
    ("src/trashdir.rs", "trash_move"): "takes the name and writes the record under the lock",
    ("src/trashdir.rs", "record_dir_size"): "writes only for the record it sized "
                                            "(tests::AFTER_WALK)",
    ("src/trashdir.rs", "write_back_with"): "directorysizes_rewrite reads the cache again",
}
# A fn that removes a payload and its record acts on a listed item. It
# takes the item through list::Entry::relock, unless it is listed here.
LOCKED_REMOVERS = {
    ("src/empty.rs", "empty_one"): "runs with the lock held, scans under it and checks "
                                   "each item against trashdir::Seen",
    ("src/retention.rs", "remove_entry"): "prune lists the items under the locks it holds",
    ("src/wsl_bin.rs", "remove_entry"): "the Windows Recycle Bin has no trash lock; "
                                        "BinEntry::recheck reads the record again first",
}
TAKES_LOCK = re.compile(r"\bTrashLock::(?:try_)?acquire\(")

# The Finder Trash and the Recycle Bin have no trash lock. A fn that
# removes or restores one of their items checks it again just before:
# finder_shared::Entry::checked, win_recycle::recheck_item (rechecked) or
# recheck_bin_item (BinEntry::recheck). Else it is listed here with why.
SCAN_ACTORS = {
    ("src/finder.rs", "move_back_inner"): "removes the restore destination under --force; "
                                          "the item itself comes from a checked caller",
    ("src/finder_shared.rs", "undo"): "acts on the put log, not on a scan: it restores what "
                                      "sits under each logged name when it runs",
    ("src/win_recycle.rs", "check_bins"): "status --check --fix removes the $I records it "
                                          "just found dangling or bad, with no wait between",
    ("src/win_recycle.rs", "prune_after_put"): "the budget trim of a put purges right after "
                                               "its own listing, with no question between",
    ("src/win_recycle.rs", "purge_oldest_on"): "reap --purge-trash purges right after its own "
                                               "listing, with no question between",
}
# Removal or restore primitives of those trashes, wherever they are
# called, and permanent removal inside their modules.
SCAN_ACT = re.compile(r"(?<!fn )\b(?:move_back|purge_all|restore_payload)\s*\(")
SCAN_FILES = {"src/finder.rs", "src/finder_shared.rs", "src/win_recycle.rs", "src/wsl_bin.rs"}
SCAN_REMOVE = re.compile(r"\bpermanent_guard::remove\s*\(")
SCAN_RECHECK = re.compile(r"\.checked\(\)|\brecheck\w*\s*\(")


def check_lock_rechecks():
    """A fn that takes a trash lock is listed in LOCK_SITES with what it
    reads again under it, and a fn that removes a payload and its record
    calls list::Entry::relock or is listed in LOCKED_REMOVERS (round 244:
    rm, restore and the empty browser acted on a scan taken before the
    lock, so an item whose name a put gave to another was hit instead)."""
    starts = re.compile(r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?\s+)?fn\s+(\w+)")
    sites, removers = set(), set()
    for path in sorted((ROOT / "src").rglob("*.rs")):
        rel = path.relative_to(ROOT).as_posix()
        text = rust_code(path.read_text(encoding="utf-8"))
        skip = test_mod_lines(text.splitlines())
        found = list(starts.finditer(text))
        cuts = [m.start() for m in found] + [len(text)]
        for m, a, b in zip(found, cuts, cuts[1:]):
            line = text.count("\n", 0, a) + 1
            body, key = text[a:b], (rel, m.group(1))
            if line - 1 in skip:
                continue
            if TAKES_LOCK.search(body):
                sites.add(key)
                if key not in LOCK_SITES:
                    errors.append(f"{rel}:{line}: {key[1]} takes the trash lock; read again under "
                                  "it what it acts on, and list it in LOCK_SITES saying what")
            if PAYLOAD_GONE.search(body) and RECORD_GONE.search(body) and ".relock(" not in body:
                removers.add(key)
                if key not in LOCKED_REMOVERS:
                    errors.append(f"{rel}:{line}: {key[1]} removes a payload and its record "
                                  "without list::Entry::relock; check the item again under the "
                                  "trash lock, or list it in LOCKED_REMOVERS with a reason")
    for rel, fn in sorted(set(LOCK_SITES) - sites):
        errors.append(f"LOCK_SITES lists {fn} in {rel}, which takes no trash lock; drop the entry")
    for rel, fn in sorted(set(LOCKED_REMOVERS) - removers):
        errors.append(f"LOCKED_REMOVERS lists {fn} in {rel}, which removes no listed item "
                      "without relock; drop the entry")


# The browsers hold stderr while they own the terminal, so what an action
# there says must reach the screen. A fn in these files may print only when
# it runs before the browser starts or after it has closed.
BROWSER_FILES = ("src/du_tui.rs", "src/empty_tui.rs", "src/put_tui.rs", "src/restore_tui.rs")
BROWSER_PRINTERS = {
    ("src/du_tui.rs", "explorer_roots"): "checks the roots before the explorer starts",
    ("src/du_tui.rs", "run"): "before enter and after leave",
    ("src/du_tui.rs", "run_snapshot"): "an unbrowsable snapshot, before enter",
    ("src/du_tui.rs", "drive"): "before enter and after leave",
    ("src/empty_tui.rs", "run"): "before enter, and the held reasons after leave",
    ("src/put_tui.rs", "run"): "before enter and after leave",
    ("src/put_tui.rs", "run_rows"): "before enter and after leave",
    ("src/restore_tui.rs", "run"): "before enter, and the held log after leave",
    ("src/restore_tui.rs", "restore_single"): "one match restores without a browser",
}
BROWSER_PRINT = re.compile(r"\be?print(?:ln)?!")


def check_browser_prints():
    """A browser action shows what it says in the browser (round 248: the
    empty browser printed why an entry stayed onto held stderr). Only the
    fns in BROWSER_PRINTERS print; stale entries are errors."""
    starts = re.compile(r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?\s+)?fn\s+(\w+)")
    printers = set()
    for rel in BROWSER_FILES:
        text = rust_code((ROOT / rel).read_text(encoding="utf-8"))
        skip = test_mod_lines(text.splitlines())
        found = list(starts.finditer(text))
        cuts = [m.start() for m in found] + [len(text)]
        for m, a, b in zip(found, cuts, cuts[1:]):
            line = text.count("\n", 0, a) + 1
            if line - 1 in skip or not BROWSER_PRINT.search(text[a:b]):
                continue
            key = (rel, m.group(1))
            printers.add(key)
            if key not in BROWSER_PRINTERS:
                errors.append(f"{rel}:{line}: {key[1]} prints while a browser may own the "
                              "terminal; put the text in the status line or a held log, or "
                              "list it in BROWSER_PRINTERS with a reason")
    for rel, fn in sorted(set(BROWSER_PRINTERS) - printers):
        errors.append(f"BROWSER_PRINTERS lists {fn} in {rel}, which prints nothing; drop "
                      "the entry")


def check_put_says():
    """put_one writes its messages through `say!`, never `eprintln!`, so the
    du explorer's delete, which listens with put::heard, shows why a row
    stayed (round 247: stderr is held while the explorer owns the
    terminal, and a failed row only counted)."""
    rel = "src/put.rs"
    text = rust_code((ROOT / rel).read_text(encoding="utf-8"))
    m = re.search(r"(?m)^pub\(crate\) fn put_one\(", text)
    if not m:
        errors.append(f"{rel}: put_one not found; update check_put_says")
        return
    end = re.search(r"(?m)^}", text[m.start():])
    body = text[m.start():m.start() + (end.end() if end else len(text))]
    for k in re.finditer(r"\beprintln!", body):
        line = text.count("\n", 0, m.start() + k.start()) + 1
        errors.append(f"{rel}:{line}: put_one writes with eprintln!; use say! so the du "
                      "explorer can show the message (put::heard)")


def check_scan_rechecks():
    """A fn that removes or restores a Finder Trash or Recycle Bin item
    checks it again just before, or is listed in SCAN_ACTORS with a reason
    (round 246: rm, restore and empty there acted on a scan made before a
    question, and Finder gives a freed name to the next item)."""
    starts = re.compile(r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?\s+)?fn\s+(\w+)")
    actors = set()
    for path in sorted((ROOT / "src").rglob("*.rs")):
        rel = path.relative_to(ROOT).as_posix()
        text = rust_code(path.read_text(encoding="utf-8"))
        skip = test_mod_lines(text.splitlines())
        found = list(starts.finditer(text))
        cuts = [m.start() for m in found] + [len(text)]
        for m, a, b in zip(found, cuts, cuts[1:]):
            line = text.count("\n", 0, a) + 1
            body, key = text[a:b], (rel, m.group(1))
            if line - 1 in skip:
                continue
            acts = SCAN_ACT.search(body) or (rel in SCAN_FILES and SCAN_REMOVE.search(body))
            if not acts or SCAN_RECHECK.search(body):
                continue
            actors.add(key)
            if key not in SCAN_ACTORS:
                errors.append(f"{rel}:{line}: {key[1]} removes or restores a Finder Trash or "
                              "Recycle Bin item without checking it again; use "
                              "finder_shared::Entry::checked or win_recycle::recheck_item, or "
                              "list it in SCAN_ACTORS with a reason")
    for rel, fn in sorted(set(SCAN_ACTORS) - actors):
        errors.append(f"SCAN_ACTORS lists {fn} in {rel}, which acts on no Finder Trash or "
                      "Recycle Bin item without a check; drop the entry")


DOT_BY_NAME = re.compile(r'Some\(\s*"\.\.?"\s*\)|OsStr::new\(\s*"\.\.?"\s*\)')


def check_dot_operands():
    """`Path::file_name()` never gives "." or "..": it reads `d/.` as `d`
    and gives None for `./` and `d/..`. put compared it with "." and so
    trashed d for `d/.` (round 224). A `.`/`..` test goes through
    safety::is_dot_operand, which reads the operand as typed."""
    for path in sorted((ROOT / "src").rglob("*.rs")):
        rel = path.relative_to(ROOT).as_posix()
        text = rust_code(path.read_text(encoding="utf-8"), strings=True)
        for line in sorted({text.count("\n", 0, m.start()) + 1 for m in DOT_BY_NAME.finditer(text)}):
            errors.append(f"{rel}:{line}: compares a path name with '.' or '..', which file_name() never "
                          "gives; use safety::is_dot_operand on the operand as typed")


def check_copy_then_remove():
    """A move across devices copies the tree and then removes the
    original. Only trashdir::move_by_copy does both (copy_recursive, then
    remove_any): it takes back a partial copy, and it keeps a whole copy
    when the removal fails partway (trash-cli #429, round 234). Every fn
    that calls it asks tree_removable first, so a tree this user cannot
    wholly remove is refused before the copy starts, not left half removed
    after it (trash-cli #427, round 225)."""
    starts = re.compile(r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?\s+)?fn\s+(\w+)")
    callers = 0
    for path in sorted((ROOT / "src").rglob("*.rs")):
        rel = path.relative_to(ROOT).as_posix()
        text = rust_code(path.read_text(encoding="utf-8"))
        skip = test_mod_lines(text.splitlines())
        found = list(starts.finditer(text))
        cuts = [m.start() for m in found] + [len(text)]
        for m, a, b in zip(found, cuts, cuts[1:]):
            line = text.count("\n", 0, a) + 1
            if line - 1 in skip or m.group(1) == "move_by_copy":
                continue
            body = text[a:b]
            if "copy_recursive(" in body and "remove_any(" in body:
                errors.append(f"{rel}:{line}: {m.group(1)} copies with copy_recursive and removes the "
                              "original with remove_any; call trashdir::move_by_copy instead, which "
                              "takes back a partial copy and keeps a whole one")
            move = body.find("move_by_copy(")
            if move < 0:
                continue
            callers += 1
            check = body.find("tree_removable(")
            if check < 0 or check > move:
                errors.append(f"{rel}:{line}: {m.group(1)} calls move_by_copy without tree_removable "
                              "before it; check that the whole tree can go first")
    if not callers:
        errors.append("precheck copy-then-remove check found no caller of move_by_copy; "
                      "the rule no longer matches src and needs updating")


WHOLE_WRITE = re.compile(r"\bfs::write\s*\(|\bFile::create\s*\(|\.truncate\s*\(\s*true\s*\)")


def check_whole_file_writes():
    """A file rtrash writes whole goes through src/file_replace.rs, which
    writes a temporary beside it, syncs it and renames it over the file,
    so a failed write never leaves a cut-off file in place (dua 2.46 kept
    a flame graph when its export failed; round 237). Outside that module
    and outside tests, fs::write, File::create and .truncate(true) are
    refused. Appends and create_new reservations are not whole writes."""
    hits = 0
    for path in sorted((ROOT / "src").rglob("*.rs")):
        rel = path.relative_to(ROOT).as_posix()
        text = rust_code(path.read_text(encoding="utf-8"))
        found = list(WHOLE_WRITE.finditer(text))
        hits += len(found)
        if rel == "src/file_replace.rs" or not found:
            continue
        skip = test_mod_lines(text.splitlines())
        for m in found:
            line = text.count("\n", 0, m.start()) + 1
            if line - 1 in skip:
                continue
            errors.append(f"{rel}:{line}: writes a file whole in place; use file_replace::write_whole "
                          "(a file the user asked for) or write_own (a file rtrash keeps), which "
                          "write a temporary and rename it, so a failed write leaves the old file")
    if not hits:
        errors.append("precheck whole-file-write check matched nothing in src; "
                      "the rule no longer matches src and needs updating")


def check_volume_trash_owner():
    """A fn outside the test modules that names this user's volume trash,
    `.Trash-{uid}`, asks `user_trash_problem` (or `ensure_user_trash`,
    which does) before it uses the directory, so a symlink, a directory
    another user planted, or one other users can write to is never put
    into, listed or emptied (trash-cli #433, round 226)."""
    starts = re.compile(r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?\s+)?fn\s+(\w+)")
    named = 0
    for path in sorted((ROOT / "src").rglob("*.rs")):
        rel = path.relative_to(ROOT).as_posix()
        text = rust_code(path.read_text(encoding="utf-8"), strings=True)
        skip = test_mod_lines(text.splitlines())
        found = list(starts.finditer(text))
        cuts = [m.start() for m in found] + [len(text)]
        for m, a, b in zip(found, cuts, cuts[1:]):
            line = text.count("\n", 0, a) + 1
            body = text[a:b]
            if line - 1 in skip or '".Trash-{' not in body:
                continue
            named += 1
            if "user_trash_problem(" not in body and "ensure_user_trash(" not in body:
                errors.append(f"{rel}:{line}: {m.group(1)} names a `.Trash-{{uid}}` volume trash without "
                              "user_trash_problem or ensure_user_trash; refuse a symlink, another "
                              "user's directory and one others can write to before using it")
    if not named:
        errors.append("precheck volume-trash owner check found no fn naming `.Trash-{uid}`; "
                      "the rule no longer matches src and needs updating")



SIZES_FNS = {("src/trashdir.rs", "load"), ("src/trashdir.rs", "directorysizes_rewrite")}


def check_directorysizes_paths():
    """Outside the test modules, only DirSizes::load and
    directorysizes_rewrite name the `directorysizes` file. Every read then
    shares one parser, which matches names by their decoded bytes, and
    every update shares one writer (round 233: three hand-rolled updates
    matched a line by its last space-separated word as written, and missed
    lines other tools encoded another way)."""
    starts = re.compile(r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?\s+)?fn\s+(\w+)")
    seen = set()
    for path in sorted((ROOT / "src").rglob("*.rs")):
        rel = path.relative_to(ROOT).as_posix()
        text = rust_code(path.read_text(encoding="utf-8"), strings=True)
        skip = test_mod_lines(text.splitlines())
        found = list(starts.finditer(text))
        cuts = [m.start() for m in found] + [len(text)]
        for m, a, b in zip(found, cuts, cuts[1:]):
            line = text.count("\n", 0, a) + 1
            if line - 1 in skip or '"directorysizes"' not in text[a:b]:
                continue
            key = (rel, m.group(1))
            seen.add(key)
            if key not in SIZES_FNS:
                errors.append(f"{rel}:{line}: {m.group(1)} names the directorysizes file; read it "
                              "through trashdir::read_size_lines in DirSizes::load and change it "
                              "through trashdir::directorysizes_rewrite")
    for rel, fn in sorted(SIZES_FNS - seen):
        errors.append(f"precheck directorysizes check found no {fn} in {rel} naming the file; "
                      "the rule no longer matches src and needs updating")


KEY_READ = re.compile(r"\bevent::(read|poll)\s*\(")


def check_key_reads():
    """A browser reads keys through `tui_term::next_key` only, so a key
    every browser shares (Ctrl-L redraw, Ctrl-Z suspend) is handled in
    one place and none of the four event loops can miss it (round 228).
    `event::read` and `event::poll` appear in src/tui_term.rs alone."""
    for path in sorted((ROOT / "src").rglob("*.rs")):
        rel = path.relative_to(ROOT).as_posix()
        if rel == "src/tui_term.rs":
            continue
        text = rust_code(path.read_text(encoding="utf-8"))
        for m in KEY_READ.finditer(text):
            line = text.count("\n", 0, m.start()) + 1
            errors.append(f"{rel}:{line}: calls event::{m.group(1)} directly; read keys with "
                          "tui_term::next_key so the keys every browser shares are handled")


# A prompt reads its answer through src/answer.rs (round 251): stdin read
# line by line, or a yes matched by hand, anywhere else is an error.
PROMPT_READ = re.compile(r"stdin\(\)\s*(?:\.lock\(\)\s*)?\.(?:read_line|lines)\b")
YES_MATCH = re.compile(r'"(?:y|Y|yes|Yes|YES)"\s*\|\s*"(?:y|Y|yes|Yes|YES)"'
                       r"|Some\(b'[yY]'\)")


def check_prompt_answers():
    """Every prompt (empty, rm, restore, put -i, the wsl and budget guards)
    reads its answer with answer::ask or answer::read_reply, so a yes means
    the same everywhere: y or yes in any case, and anything else, a piped n
    or the end of input included, is a no (trash-cli #333). src/answer.rs
    alone reads stdin by the line or spells out what a yes is."""
    for path in sorted((ROOT / "src").rglob("*.rs")):
        rel = path.relative_to(ROOT).as_posix()
        if rel == "src/answer.rs":
            continue
        raw = path.read_text(encoding="utf-8")
        skip = test_mod_lines(raw.splitlines())
        for pat, keep, what in ((PROMPT_READ, False, "reads stdin by the line"),
                                (YES_MATCH, True, "matches a yes by hand")):
            text = rust_code(raw, strings=keep)
            for m in pat.finditer(text):
                line = text.count("\n", 0, m.start()) + 1
                if line - 1 in skip:
                    continue
                errors.append(f"{rel}:{line}: {what}; read prompt answers with answer::ask "
                              "or answer::read_reply so every prompt parses them alike")


# Clippy's empty_line_after_doc_comments fails CI on a `///` comment
# that a blank line parts from its item, and only a build shows it
# (round 251: rm_ask kept the doc of a removed fn above a blank line).
DOC_GAP_CASES = [
    ("gap", "/// a\n\nfn f() {}\n", [1]),
    ("no gap", "/// a\nfn f() {}\n", []),
    ("indented gap", "    /// a\n    /// b\n    \n    fn f() {}\n", [2]),
    ("inner doc", "//! m\n\nuse x;\n", []),
    ("plain comment", "// a\n\nfn f() {}\n", []),
    ("four slashes", "//// banner\n\nfn f() {}\n", []),
    ("in a string", 'let s = "\n/// a\n\n";\n', []),
    ("last line", "fn f() {}\n/// a\n", []),
]


def doc_gap_lines(text):
    """Lines of `text` holding a `///` doc comment with a blank line right
    after it and code further down. A `///` inside a string literal is not a comment: the code
    with comments gone (strings kept) still has it."""
    raw = text.split("\n")
    code = rust_code(text, strings=True).split("\n")
    last = max((k for k, line in enumerate(raw) if line.strip()), default=0)
    hits = []
    for n, line in enumerate(raw[:last]):
        t = line.strip()
        doc = t.startswith("///") and not t.startswith("////")
        if doc and not code[n].strip() and not raw[n + 1].strip():
            hits.append(n + 1)
    return hits


def check_doc_gaps():
    """No `///` doc comment in src/ or tests/ sits above a blank line, and
    the matcher meets every DOC_GAP_CASES case."""
    for what, text, want in DOC_GAP_CASES:
        if doc_gap_lines(text) != want:
            errors.append(f"scripts/precheck.py: doc_gap_lines gets DOC_GAP_CASES '{what}' "
                          "wrong; the doc-gap check is broken")
    for top in ("src", "tests"):
        for path in sorted((ROOT / top).rglob("*.rs")):
            rel = path.relative_to(ROOT).as_posix()
            for line in doc_gap_lines(path.read_text(encoding="utf-8")):
                errors.append(f"{rel}:{line}: a blank line follows this doc comment "
                              "(clippy::empty_line_after_doc_comments); remove the gap, or "
                              "make it a // comment if it documents nothing")


# A size sum in the du code saturates (round 252, ncdu #278): a plain
# `+`, `+=` or `.sum()` wraps in release and panics in debug when an
# export or a corrupt stat brings a size near u64::MAX.
SIZE_NAME = (r"(?:disk|apparent|unique|reclaim|length|bytes|dsize|asize|disk_all|base|freed"
             r"|size|loaded_bytes|referenced|left|raw|own|total|cum_dsize)")
SIZE_ADD = re.compile(rf"\b{SIZE_NAME}\s*\+=|\b{SIZE_NAME}\s*\+\s*[\w(]"
                      rf"|[\w)]\s*\+\s*(?:\w+\.)*{SIZE_NAME}\b(?!\s*\()")
SIZE_MUL = re.compile(r"\*\s*512\b")
# `.sum` of anything but lengths in memory (`.len()`), which a usize holds.
BARE_SUM = re.compile(r"(?<!\.len\(\)\))(?<!\s)\s*\.sum\b(?:::<\w+>)?\(\)")
SIZE_SUM_CASES = [
    ("plus-equals", "t.bytes += n;\n", [1]),
    ("plain plus", "let d = a.disk + b.disk;\n", [1]),
    ("plus a size", "let d = 4096 + f.dsize;\n", [1]),
    ("saturating", "t.bytes = t.bytes.saturating_add(n);\n", []),
    ("count", "n.items += 1;\n", []),
    ("index", "let at = off + len;\n", []),
    ("sum", "let d: u64 = rows.iter().map(|r| r.disk).sum();\n", [1]),
    ("turbofish sum", "let d = v.iter().copied().sum::<u64>();\n", [1]),
    ("chained sum", "let d = v\n    .iter()\n    .map(|r| r.disk)\n    .sum();\n", [4]),
    ("len sum", "let n: usize = g.iter().map(|g| g.extras.len()).sum();\n", []),
    ("chained len sum",
     "let n: usize = g\n    .iter()\n    .map(|g| g.extras.len())\n    .sum();\n", []),
    ("size_sum", "let d = du::size_sum(rows.iter().map(|r| r.disk));\n", []),
    ("blocks", "let d = m.blocks() * 512;\n", [1]),
    ("blocks saturating", "let d = m.blocks().saturating_mul(512);\n", []),
    ("comment", "// t.bytes += n;\n", []),
]


def size_sum_lines(text):
    """Lines of `text` that add, sum or scale a size without saturating."""
    code = rust_code(text)
    hits = set()
    for pat in (SIZE_ADD, SIZE_MUL, BARE_SUM):
        for m in pat.finditer(code):
            hits.add(code.count("\n", 0, m.end()) + 1)
    return sorted(hits)


def check_size_sums():
    """Outside tests, src/du*.rs and src/du_fs/ add sizes with
    du::size_sum or saturating_add, and scale blocks with saturating_mul,
    as coreutils du stops at the top; SIZE_SUM_CASES tests the matcher."""
    for what, text, want in SIZE_SUM_CASES:
        if size_sum_lines(text) != want:
            errors.append(f"scripts/precheck.py: size_sum_lines gets SIZE_SUM_CASES '{what}' "
                          "wrong; the size-sum check is broken")
    paths = sorted((ROOT / "src").glob("du*.rs")) + sorted((ROOT / "src" / "du_fs").glob("*.rs"))
    for path in paths:
        rel = path.relative_to(ROOT).as_posix()
        raw = path.read_text(encoding="utf-8")
        skip = test_mod_lines(raw.splitlines())
        for line in size_sum_lines(raw):
            if line - 1 not in skip:
                errors.append(f"{rel}:{line}: adds, sums or scales a size without saturating; "
                              "use du::size_sum, saturating_add or saturating_mul (ncdu #278)")


# Every SIZE option reads its value with filters::parse_size (round 255,
# dust #629), so 5mb, 5MB and 5MiB mean one size everywhere. A unit letter
# matched by hand, a B or iB suffix stripped or compared, or a lookup
# string of unit letters anywhere else is an error. util::parse_block_size
# is the one exception: block sizes follow coreutils, where KB is 1000.
SIZE_UNIT = r"(?:[KMGTP](?:i?[Bb])?|[kmgtp]i?[Bb]|[Ii]?[Bb])"
SIZE_ARG_PATS = [
    re.compile(r"(?<![A-Za-z(])b?'[KMGTP]'\s*(?:\|\s*b?'[A-Za-z]'\s*)*=>"),
    re.compile(r'"' + SIZE_UNIT + r'"\s*(?:\|\s*"[A-Za-z]{1,3}"\s*)*=>'),
    re.compile(r"(?:strip_suffix|ends_with|eq_ignore_ascii_case|trim_end_matches)\(\s*"
               r'(?:"(?:[KMGTPkmgtp]?i?[Bb]|[KMGTPkmgtp])"|b?\'[KMGTPkmgtpBb]\')'),
    re.compile(r'"(?:KMG|kmg)'),
]
SIZE_ARG_CASES = [
    ("unit arm", "        'M' => 1 << 20,\n", [1]),
    ("unit arms joined", "        'K' | 'k' => 1024,\n", [1]),
    ("byte unit arm", "        b'G' => 1 << 30,\n", [1]),
    ("suffix arm", '        "MiB" | "MB" => 1 << 20,\n', [1]),
    ("suffix stripped", '    let t = t.strip_suffix("iB").unwrap_or(t);\n', [1]),
    ("byte suffix", "    if v.ends_with('B') {\n", [1]),
    ("letter lookup", '    let p = "KMGTPE".find(c)?;\n', [1]),
    ("key char", "        KeyCode::Char('G') => self.last(),\n", []),
    ("minutes", '        "m" | "min" => 60,\n', []),
    ("escape", "        't' => out.push('\\t'),\n", []),
    ("parse_size call", "    let n = crate::filters::parse_size(v)?;\n", []),
    ("comment", "    // 'M' => 1 << 20,\n", []),
]


def size_arg_lines(text, skip=()):
    """Lines of `text` (0-based numbers in `skip` left out) that parse a
    size unit by hand. Comment lines are left out."""
    hits = []
    for n, line in enumerate(text.split("\n")):
        if n in skip or line.lstrip().startswith("//"):
            continue
        if any(p.search(line) for p in SIZE_ARG_PATS):
            hits.append(n + 1)
    return hits


def fn_lines(lines, name):
    """0-based line numbers of the top-level fn `name`, through its
    closing brace at the start of a line."""
    for k, line in enumerate(lines):
        if re.match(r"pub(?:\([a-z]+\))? fn " + name + r"\b|fn " + name + r"\b", line):
            j = k
            while j < len(lines) and lines[j] != "}":
                j += 1
            return set(range(k, j + 1))
    return set()


def check_size_args():
    """Outside tests, only src/filters.rs reads a size unit (parse_size,
    for every SIZE option and variable), and util::parse_block_size reads
    the coreutils block sizes; SIZE_ARG_CASES tests the matcher."""
    for what, text, want in SIZE_ARG_CASES:
        if size_arg_lines(text) != want:
            errors.append(f"scripts/precheck.py: size_arg_lines gets SIZE_ARG_CASES '{what}' "
                          "wrong; the size-argument check is broken")
    for path in sorted((ROOT / "src").rglob("*.rs")):
        rel = path.relative_to(ROOT).as_posix()
        if rel == "src/filters.rs":
            continue
        raw = path.read_text(encoding="utf-8")
        lines = raw.split("\n")
        skip = test_mod_lines(lines)
        if rel == "src/util.rs":
            skip |= fn_lines(lines, "parse_block_size")
        for line in size_arg_lines(raw, skip):
            errors.append(f"{rel}:{line}: reads a size unit by hand; parse SIZE values with "
                          "filters::parse_size so every option takes the same units (dust #629)")


REPORT_LIT = re.compile(r"(?<!\w)Report \{")
REPORT_DECL = re.compile(r"(?:\bstruct|\bimpl|\bfor|->)\s*(?:[\w:]*::)?$")
REPORT_FILL_CASES = [
    ("named", "let r = Report {\n    roots,\n    partial: false,\n};\n", []),
    ("filled", "let r = du::Report {\n    roots,\n    ..old.clone()\n};\n", [3]),
    ("nested fill", "let r = Report {\n    opts: ScanOpts {\n        ..o\n    },\n};\n", []),
    ("struct", "pub struct Report {\n    ..\n}\n", []),
    ("return type", "fn f() -> du::Report {\n    ..x\n}\n", []),
    ("crate path", "Ok(crate::du::Report {\n    ..r\n})\n", [2]),
]


def report_fill_lines(text):
    """Lines of `..` fills directly inside a `Report { .. }` literal."""
    hits = []
    for m in REPORT_LIT.finditer(text):
        if REPORT_DECL.search(text[text.rfind("\n", 0, m.start()) + 1:m.start()]):
            continue
        depth, i = 0, m.end() - 1
        while i < len(text):
            c = text[i]
            if c == "{":
                depth += 1
            elif c == "}":
                depth -= 1
                if depth == 0:
                    break
            elif c == "\n" and depth == 1 and text[i + 1:].lstrip(" ").startswith(".."):
                hits.append(text.count("\n", 0, i) + 2)
            i += 1
    return hits


def check_report_literals():
    """A du::Report built by hand names every field: a `..other` fill would
    copy a field such as partial or unread from one part into a joined or
    converted report, and a field added later would pass unnoticed.
    REPORT_FILL_CASES tests the matcher."""
    for what, text, want in REPORT_FILL_CASES:
        if report_fill_lines(text) != want:
            errors.append(f"scripts/precheck.py: report_fill_lines gets REPORT_FILL_CASES '{what}' "
                          "wrong; the report-literal check is broken")
    for path in sorted((ROOT / "src").rglob("*.rs")):
        rel = path.relative_to(ROOT).as_posix()
        for line in report_fill_lines(path.read_text(encoding="utf-8")):
            errors.append(f"{rel}:{line}: fills a du::Report from another with `..`; name every "
                          "field so a new one must be set here too")


# Which functions of src/du_tui.rs may call each walk. Every walk the
# explorer starts goes through App::walk, App::quick or App::spawn_with,
# which refuse while it browses a snapshot (du -f FILE -i).
EXPLORER_WALKS = {
    "scan_any": {"walk", "spawn_with", "new_in"},
    "quick_any": {"quick", "start"},
    "scan_roots": {"scan_any"},
    "scan_view": {"scan_any", "scan_roots"},
    "quick_roots": {"quick_any"},
    "quick_view": {"quick_any"},
    "du::scan": {"scan_view"},
}
WALK_CALL = re.compile(r"(?<![\w:])(du::scan|scan_any|quick_any|scan_roots|scan_view|"
                       r"quick_roots|quick_view)\(")
EXPLORER_WALK_CASES = [
    ("gated", "    fn walk(&self) {\n        scan_any(d, r)\n    }\n", []),
    ("ungated", "    fn go(&mut self) {\n        let v = scan_any(d, r);\n    }\n", [2]),
    ("quick", "    fn back(&mut self) {\n        quick_view(d)\n    }\n", [2]),
    ("definition", "fn scan_view(dir: &Path) -> V {\n    du::scan(&[d], &o)\n}\n", []),
    ("comment", "    fn go(&mut self) {\n        // scan_any(d) is not called\n    }\n", []),
]


def explorer_walk_lines(text, skip=()):
    """Lines that call a walk from a function EXPLORER_WALKS does not
    allow it in. Comment lines and 0-based numbers in `skip` are left out."""
    hits, owner, end = [], None, None
    lines = text.split("\n")
    for n, line in enumerate(lines):
        m = re.match(r"(\s*)(?:pub(?:\([a-z]+\))? )?fn (\w+)", line)
        if m and (owner is None or len(m.group(1)) <= len(end) - 1):
            owner, end = m.group(2), m.group(1) + "}"
            continue
        if owner is not None and line == end:
            owner = None
            continue
        if n in skip or line.lstrip().startswith("//"):
            continue
        for call in WALK_CALL.findall(line):
            if owner not in EXPLORER_WALKS[call]:
                hits.append(n + 1)
    return hits


def check_explorer_walks():
    """The explorer walks only through App::walk, App::quick and
    App::spawn_with (and the scanners those call), so browsing a snapshot
    walks nothing; EXPLORER_WALK_CASES tests the matcher."""
    for what, text, want in EXPLORER_WALK_CASES:
        if explorer_walk_lines(text) != want:
            errors.append(f"scripts/precheck.py: explorer_walk_lines gets EXPLORER_WALK_CASES '{what}' "
                          "wrong; the explorer-walk check is broken")
    path = ROOT / "src" / "du_tui.rs"
    raw = path.read_text(encoding="utf-8")
    for line in explorer_walk_lines(raw, test_mod_lines(raw.split("\n"))):
        errors.append(f"src/du_tui.rs:{line}: walks outside App::walk, App::quick and "
                      "App::spawn_with; go through them so du -f FILE -i never scans (ncdu #247)")


# The explorer moves the totals of views and rows only in these fns, so a
# change carries every total up the chain at once (struct Totals).
CHAIN_TOTAL_FNS = {"carry", "put_row"}
CHAIN_TOTAL = re.compile(r"\.(?:disk|apparent|items)\s*(?:[-+*]=|=(?!=))|"
                         r"&mut\s+[\w.()\[\]&]+\.(?:disk|apparent|items)\b")
CHAIN_TOTAL_CASES = [
    ("carry", "fn carry(v: &mut View) {\n    v.disk = shift(v.disk, a, b);\n}\n", []),
    ("by hand", "    fn drop_row(&mut self) {\n        v.disk -= it.disk;\n    }\n", [2]),
    ("set", "    fn apply_fresh(&mut self) {\n        row.items = n;\n    }\n", [2]),
    ("closure", "fn adjust(v: &mut View) {\n    apply(&mut v.apparent, d);\n}\n", [2]),
    ("compare", "    fn f(&self) {\n        if row.disk == 0 {}\n    }\n", []),
    ("literal", "    fn f() -> Item {\n        Item { disk: 0, items: 1 }\n    }\n", []),
]


def chain_total_lines(text, skip=()):
    """Lines that change a view's or row's disk, apparent or items field
    outside CHAIN_TOTAL_FNS. Comment lines and 0-based `skip` are left out."""
    hits, owner, end = [], None, None
    for n, line in enumerate(text.split("\n")):
        m = re.match(r"(\s*)(?:pub(?:\([a-z]+\))? )?fn (\w+)", line)
        if m and (owner is None or len(m.group(1)) <= len(end) - 1):
            owner, end = m.group(2), m.group(1) + "}"
            continue
        if owner is not None and line == end:
            owner = None
            continue
        if n in skip or line.lstrip().startswith("//"):
            continue
        if owner not in CHAIN_TOTAL_FNS and CHAIN_TOTAL.search(line):
            hits.append(n + 1)
    return hits


def check_chain_totals():
    """In src/du_tui.rs the sizes and item counts of views and rows change
    only in carry and put_row, which move every total of struct Totals at
    once (round 255: deletes moved sizes but not item counts; round 257: a
    rescan moved the apparent size only the way disk use went).
    CHAIN_TOTAL_CASES tests the matcher."""
    for what, text, want in CHAIN_TOTAL_CASES:
        if chain_total_lines(text) != want:
            errors.append(f"scripts/precheck.py: chain_total_lines gets CHAIN_TOTAL_CASES '{what}' "
                          "wrong; the chain-total check is broken")
    raw = (ROOT / "src" / "du_tui.rs").read_text(encoding="utf-8")
    for line in chain_total_lines(raw, test_mod_lines(raw.split("\n"))):
        errors.append(f"src/du_tui.rs:{line}: changes a view or row total by hand; go through "
                      "carry or put_row so every field of Totals moves together")


# Functions that hand out every mount point, or every user's home, which
# may sit on a network share whose server is gone (glib #605). A fn that
# calls one looks at what it gets through mount_probe::probe or
# probe_typed, which give up after RTRASH_MOUNT_WAIT, or is listed here with
# the reason it needs no probe.
MOUNT_LISTERS = re.compile(r"(?<![\w])(mount_points|mounts_from_table|user_homes)\(")
MOUNT_PROBE = re.compile(r"(?<![\w])probe(?:_typed)?\(")
MOUNT_NAMES_ONLY = {
    ("src/list.rs", "run"): "list --volumes prints the mount points and looks at none",
    ("src/du_dev.rs", "devices"): "it hands the mounts to measure, which probes each",
}
MOUNT_PROBE_CASES = [
    ("probed", "fn all() {\n    for mp in mount_points() {\n        probe(&mp, f);\n    }\n}\n",
     []),
    ("bare", "fn all() {\n    for mp in mount_points() {\n        mp.is_dir();\n    }\n}\n",
     [("all", 2)]),
    ("homes", "fn h() {\n    for h in user_homes() {\n        h.exists();\n    }\n}\n",
     [("h", 2)]),
    ("definition", "fn mount_points() -> Vec<PathBuf> {\n    Vec::new()\n}\n", []),
    ("comment", "fn f() {\n    // mount_points() is not called\n}\n", []),
]


def unprobed_mount_lines(text, skip=()):
    """(fn, line) pairs that call a mount lister in a fn that never calls
    probe or probe_typed. Comment lines and 0-based `skip` are left out."""
    calls, probed, owner, end = [], set(), None, None
    for n, line in enumerate(text.split("\n")):
        m = re.match(r"(\s*)(?:pub(?:\([a-z]+\))? )?fn (\w+)", line)
        if m and (owner is None or len(m.group(1)) <= len(end) - 1):
            owner, end = m.group(2), m.group(1) + "}"
            continue
        if owner is not None and line == end:
            owner = None
            continue
        if n in skip or line.lstrip().startswith("//"):
            continue
        if MOUNT_PROBE.search(line):
            probed.add(owner)
        if MOUNT_LISTERS.search(line):
            calls.append((owner, n + 1))
    return [(fn, n) for fn, n in calls if fn not in probed]


def check_mount_probes():
    """Every look at the mounts, or at every user's home, goes through
    mount_probe::probe or probe_typed, so a dead network share cannot hang
    list, empty, status or du --devices (glib #605). MOUNT_PROBE_CASES tests
    the matcher."""
    for what, text, want in MOUNT_PROBE_CASES:
        if unprobed_mount_lines(text) != want:
            errors.append(f"scripts/precheck.py: unprobed_mount_lines gets MOUNT_PROBE_CASES "
                          f"'{what}' wrong; the mount-probe check is broken")
    for path in sorted((ROOT / "src").rglob("*.rs")):
        rel = path.relative_to(ROOT).as_posix()
        raw = path.read_text(encoding="utf-8")
        for fn, line in unprobed_mount_lines(raw, test_mod_lines(raw.split("\n"))):
            if (rel, fn) not in MOUNT_NAMES_ONLY:
                errors.append(f"{rel}:{line}: {fn} lists the mounts or homes without "
                              "mount_probe::probe; a dead network share would hang it (glib "
                              "#605), so probe each or add it to MOUNT_NAMES_ONLY with a reason")


# A fn that reads a directory and calls itself grows the stack once per
# level, so a deep tree can overflow it: du --dupes walked that way on
# rayon's 2 MiB stacks (round 260; pdu #243, diskus #27). Each one left has
# a bound on its depth, named here.
BOUNDED_WALKS = {
    ("src/fastdelete.rs", "usage_in"): "USAGE_PATH_DEPTH levels, then deep_usage by descriptor",
    ("src/list.rs", "walk"): "the --depth the user asked for",
    ("src/win_recycle.rs", "write_tree_below"): "the --depth the user asked for",
    ("src/reap_profiles.rs", "walk"): "one level per component of the profile's pattern",
    ("src/reap_profiles.rs", "star"): "max levels for each ** of the pattern",
}
DIR_READ = re.compile(r"(?<![\w])(?:read_dir|readdir|read_dirfd_entries|Dir::open|openat_dir)\(")
RECURSIVE_WALK_CASES = [
    ("recursive", "fn w(d: &Path) {\n    for e in fs::read_dir(d) {\n        w(&e);\n    }\n}\n",
     [("w", 1)]),
    ("by method", "    fn w(&self) {\n        let r = read_dir(p);\n        self.w();\n    }\n",
     [("w", 1)]),
    ("looped", "fn w(d: &Path) {\n    let mut todo = vec![d];\n"
     "    while let Some(d) = todo.pop() {\n        fs::read_dir(d);\n    }\n}\n", []),
    ("no read", "fn f(n: u32) -> u32 {\n    f(n - 1)\n}\n", []),
    ("other fn", "fn walk_dir(d: &Path) {\n    read_dir(d);\n    my_walk_dir(d);\n}\n", []),
    ("comment", "fn w(d: &Path) {\n    read_dir(d);\n    // w(d) once did this\n}\n", []),
]


def recursive_walk_lines(text, skip=()):
    """(fn, line) of each fn that both reads a directory and calls itself,
    by name or through self or Self, nested fns apart. Comment lines and
    0-based `skip` are left out."""
    stack, seen = [], {}
    for n, line in enumerate(text.split("\n")):
        m = re.match(r"(\s*)(?:pub(?:\([a-z]+\))? )?(?:unsafe )?fn (\w+)", line)
        if m and not line.rstrip().endswith(";"):
            stack.append((m.group(2), m.group(1) + "}", n + 1))
            seen[stack[-1]] = [False, False]
            continue
        if stack and line == stack[-1][1]:
            stack.pop()
            continue
        if not stack or n in skip or line.lstrip().startswith("//"):
            continue
        key = stack[-1]
        if re.search(rf"(?<![\w])(?:Self::|self\.)?{key[0]}\(", line):
            seen[key][0] = True
        if DIR_READ.search(line):
            seen[key][1] = True
    return [(k[0], k[2]) for k, (calls, reads) in seen.items() if calls and reads]


def check_bounded_walks():
    """No directory walk recurses without a bound on its depth: one that
    reads a directory and calls itself is in BOUNDED_WALKS with its bound,
    or keeps a list of what is left instead. RECURSIVE_WALK_CASES tests the
    matcher."""
    for what, text, want in RECURSIVE_WALK_CASES:
        if recursive_walk_lines(text) != want:
            errors.append(f"scripts/precheck.py: recursive_walk_lines gets RECURSIVE_WALK_CASES "
                          f"'{what}' wrong; the bounded-walk check is broken")
    found = set()
    for path in sorted((ROOT / "src").rglob("*.rs")):
        rel = path.relative_to(ROOT).as_posix()
        raw = path.read_text(encoding="utf-8")
        for fn, line in recursive_walk_lines(raw, test_mod_lines(raw.split("\n"))):
            found.add((rel, fn))
            if (rel, fn) not in BOUNDED_WALKS:
                errors.append(f"{rel}:{line}: {fn} reads a directory and calls itself, so a deep "
                              "tree can overflow the stack; walk with a list of what is left, or "
                              "add it to BOUNDED_WALKS with the bound on its depth")
    for rel, fn in sorted(set(BOUNDED_WALKS) - found):
        errors.append(f"scripts/precheck.py: BOUNDED_WALKS names {rel} {fn}, which no longer "
                      "recurses through directories; remove it")


# `metadata(..).is_err()` takes a path that cannot be reached (EACCES on a
# parent) for a missing one: put said "No such file or directory" for a
# denied file on Windows (trash-cli #377), and empty --dangling took every
# record under a denied files/ for dangling (round 261). util::look and
# util::is_gone tell the two apart.
METADATA_IS_ERR = re.compile(r"\b(?:symlink_)?metadata\((?:[^()]|\([^()]*\))*\)\s*\.is_err\(\)")
METADATA_IS_ERR_CASES = [
    ("method", "if p.symlink_metadata().is_err() {", True),
    ("fn", "if std::fs::symlink_metadata(&p.join(x)).is_err() {", True),
    ("follows", "let gone = fs::metadata(p).is_err();", True),
    ("is_ok", "if p.symlink_metadata().is_ok() {", False),
    ("helper", "if crate::util::is_gone(&p) {", False),
    ("match", "match fs::symlink_metadata(p) {", False),
]


def check_missing_looks():
    """No code outside the tests decides that a path is missing from a
    failed metadata call: it goes through util::look or util::is_gone,
    which take only ENOENT and ENOTDIR for missing. METADATA_IS_ERR_CASES
    tests the matcher."""
    for what, line, want in METADATA_IS_ERR_CASES:
        if bool(METADATA_IS_ERR.search(line)) != want:
            errors.append(f"scripts/precheck.py: METADATA_IS_ERR gets METADATA_IS_ERR_CASES "
                          f"'{what}' wrong; the missing-path check is broken")
    for path in sorted((ROOT / "src").rglob("*.rs")):
        raw = path.read_text(encoding="utf-8")
        skip = test_mod_lines(raw.split("\n"))
        for k, line in enumerate(rust_code(raw).split("\n")):
            if k not in skip and METADATA_IS_ERR.search(line):
                errors.append(f"{path.relative_to(ROOT).as_posix()}:{k + 1}: metadata(..).is_err() "
                              "takes a path that cannot be reached for a missing one; use "
                              "crate::util::is_gone or crate::util::look")


# A du walk decides on an entry before it opens it: an excluded folder is
# never opened or looked into, since opening one is what brings up the
# macOS privacy prompts (dust #546, pdu #134). The cold-cache probe and
# the --dupes walk opened excluded folders until round 262. Each fn in
# src/du*.rs that opens a directory asks the filters in the same fn
# (FILTER_ASKS), or is in UNFILTERED_OPENS with the reason it may open.
DIR_OPEN = re.compile(r"(?<![\w])(?:read_dir|open_child|Dir::open|Dir::open_follow)\(")
FILTER_ASKS = re.compile(r"(?<![\w])(?:excluded|probe_opens)\(")
UNFILTERED_OPENS = {
    ("src/du.rs", "enter"): "opens a child that stat_entries let through, or a PATH as named",
    ("src/du_dupes.rs", "enters"): "read_level asks the filters first; this opens only a "
                                   "directory on another file system, to tell a kernfs",
    ("src/du_tui.rs", "quick_view"): "lists the directory on screen, which the scan entered",
}
UNFILTERED_OPEN_CASES = [
    ("opens unasked", "fn w(d: &Dir) {\n    let c = d.open_child(n);\n}\n", [("w", 2)]),
    ("asks first", "fn w(d: &Dir) {\n    if f.excluded(c, p, n, true) {\n        return;\n    }\n"
                   "    let c = d.open_child(n);\n}\n", []),
    ("by path", "fn w(d: &Path) {\n    for e in fs::read_dir(d) {}\n}\n", [("w", 2)]),
    ("nested apart", "fn w() {\n    fn v() {\n        excluded(x);\n    }\n"
                     "    let d = Dir::open(p);\n}\n", [("w", 5)]),
    ("comment", "fn w() {\n    // read_dir(d) here\n}\n", []),
]


def unfiltered_open_lines(text, skip=()):
    """(fn, line) of each fn that opens a directory and asks no filter, by
    the first open; nested fns apart. Comment lines and 0-based `skip` are
    left out."""
    stack, seen = [], {}
    for n, line in enumerate(text.split("\n")):
        m = re.match(r"(\s*)(?:pub(?:\([a-z]+\))? )?(?:unsafe )?fn (\w+)", line)
        if m and not line.rstrip().endswith(";"):
            stack.append((m.group(2), m.group(1) + "}", n + 1))
            seen[stack[-1]] = [None, False]
            continue
        if stack and line == stack[-1][1]:
            stack.pop()
            continue
        if not stack or n in skip or line.lstrip().startswith("//"):
            continue
        key = stack[-1]
        if DIR_OPEN.search(line) and seen[key][0] is None:
            seen[key][0] = n + 1
        if FILTER_ASKS.search(line):
            seen[key][1] = True
    return [(k[0], first) for k, (first, asks) in seen.items() if first and not asks]


def check_filtered_opens():
    """No du walk opens a directory before the filters decide on it: each
    fn in src/du*.rs that opens one asks the filters, or is in
    UNFILTERED_OPENS. UNFILTERED_OPEN_CASES tests the matcher."""
    for what, text, want in UNFILTERED_OPEN_CASES:
        if unfiltered_open_lines(text) != want:
            errors.append(f"scripts/precheck.py: unfiltered_open_lines gets UNFILTERED_OPEN_CASES "
                          f"'{what}' wrong; the filtered-open check is broken")
    found = set()
    for path in sorted((ROOT / "src").glob("du*.rs")):
        rel = path.relative_to(ROOT).as_posix()
        raw = path.read_text(encoding="utf-8")
        skip = test_mod_lines(raw.split("\n"))
        for fn, line in unfiltered_open_lines(rust_code(raw), skip):
            found.add((rel, fn))
            if (rel, fn) not in UNFILTERED_OPENS:
                errors.append(f"{rel}:{line}: {fn} opens a directory without asking the filters, "
                              "so an excluded folder gets opened (macOS privacy prompts); ask "
                              "Filter::excluded first, or add it to UNFILTERED_OPENS with why")
    for rel, fn in sorted(set(UNFILTERED_OPENS) - found):
        errors.append(f"scripts/precheck.py: UNFILTERED_OPENS names {rel} {fn}, which no longer "
                      "opens a directory unasked; remove it")


# Termux and other Android sandboxes may deny the mount table (trash-cli
# #348). Each read of it had its own fallback until round 262, and one
# sent every volume to `/`. trashdir::read_mount_table is the one reader
# (with the debug seam RTRASH_TEST_NO_MOUNTS), so a test reaches every
# fallback; no other fn names a mount table path.
MOUNT_TABLE_PATH = re.compile(
    r'"/(?:proc/(?:self/)?(?:mounts|mountinfo)|etc/mtab|proc/filesystems)"')
MOUNT_TABLE_READER = ("src/trashdir.rs", "read_mount_table")
MOUNT_TABLE_CASES = [
    ("reader", 'fn read_mount_table() {\n    const T: &str = "/proc/self/mounts";\n}\n', []),
    ("other fn", 'fn f() {\n    fs::read_to_string("/proc/mounts")\n}\n', [("f", 2)]),
    ("mtab", 'fn g() {\n    let t = "/etc/mtab";\n}\n', [("g", 2)]),
    ("filesystems", 'fn h() {\n    open("/proc/filesystems");\n}\n', [("h", 2)]),
    ("not a table", 'fn k() {\n    let p = "/proc/self/fd";\n}\n', []),
]


def mount_table_lines(text, reader="read_mount_table", skip=()):
    """(fn, line) of each mount table path named outside the fn `reader`;
    0-based `skip` lines are left out."""
    stack, out = [], []
    for n, line in enumerate(text.split("\n")):
        m = re.match(r"(\s*)(?:pub(?:\([a-z]+\))? )?(?:const )?(?:unsafe )?fn (\w+)", line)
        if m and not line.rstrip().endswith(";"):
            stack.append((m.group(2), m.group(1) + "}"))
        elif stack and line == stack[-1][1]:
            stack.pop()
        if n in skip or not MOUNT_TABLE_PATH.search(line):
            continue
        fn = stack[-1][0] if stack else "(top level)"
        if fn != reader:
            out.append((fn, n + 1))
    return out


def check_mount_table_reads():
    """Only trashdir::read_mount_table names a mount table path, so every
    reader goes through its error and its debug seam. MOUNT_TABLE_CASES
    tests the matcher."""
    for what, text, want in MOUNT_TABLE_CASES:
        if mount_table_lines(text) != want:
            errors.append(f"scripts/precheck.py: mount_table_lines gets MOUNT_TABLE_CASES "
                          f"'{what}' wrong; the mount-table check is broken")
    for path in sorted((ROOT / "src").rglob("*.rs")):
        rel = path.relative_to(ROOT).as_posix()
        raw = path.read_text(encoding="utf-8")
        skip = test_mod_lines(raw.split("\n"))
        reader = MOUNT_TABLE_READER[1] if rel == MOUNT_TABLE_READER[0] else ""
        for fn, line in mount_table_lines(rust_code(raw, strings=True), reader, skip):
            errors.append(f"{rel}:{line}: {fn} names a mount table path; read it through "
                          "crate::trashdir::read_mount_table, which Termux may deny "
                          "(trash-cli #348) and tests can fail with RTRASH_TEST_NO_MOUNTS")


# A list of reparse tags misses the rest: du took only symlinks and
# junctions for links, while std and the --dupes walk took every name
# surrogate for a link (round 263; diskus #42, gdu #441). Code outside the
# tests tells a link by du_fs::fileid::is_link, the name-surrogate bit,
# and names no IO_REPARSE_TAG_* constant but where one is declared. One fn
# names a tag that stores file data, not a link: fileid::is_wof, which
# tells a Compact file by IO_REPARSE_TAG_WOF (gdu #407; REPARSE_TAG_DATA).
REPARSE_TAG_USE = re.compile(r"\bIO_REPARSE_TAG_\w+")
REPARSE_TAG_DECL = re.compile(r"^\s*(?:pub(?:\([a-z]+\))? )?const IO_REPARSE_TAG_\w+\s*:")
REPARSE_TAG_CASES = [
    ("compare", "    tag == IO_REPARSE_TAG_SYMLINK", True),
    ("match", "    IO_REPARSE_TAG_MOUNT_POINT | IO_REPARSE_TAG_SYMLINK => true,", True),
    ("declared", "    pub const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;", False),
    ("surrogate", "    tag & NAME_SURROGATE != 0", False),
]


REPARSE_TAG_DATA = {"is_wof"}
# A restore to another volume makes each link again (34(i)): std's
# is_symlink, the name-surrogate bit, says that it is a link, and the tag
# says which kind to make. These fns read or write that tag.
REPARSE_TAG_REMAKE = {"link_kind", "junction_data"}


def reparse_tag_line(line):
    """Whether `line` uses an IO_REPARSE_TAG_* constant, not declares it."""
    return bool(REPARSE_TAG_USE.search(line)) and not REPARSE_TAG_DECL.match(line)


def check_reparse_links():
    """Code outside the tests tells a link from its reparse tag only through
    du_fs::fileid::is_link. REPARSE_TAG_CASES tests the matcher."""
    for what, line, want in REPARSE_TAG_CASES:
        if reparse_tag_line(line) != want:
            errors.append(f"scripts/precheck.py: reparse_tag_line gets REPARSE_TAG_CASES "
                          f"'{what}' wrong; the reparse-link check is broken")
    for path in sorted((ROOT / "src").rglob("*.rs")):
        raw = path.read_text(encoding="utf-8")
        skip = test_mod_lines(raw.split("\n"))
        lines = rust_code(raw).split("\n")
        for k, line in enumerate(lines):
            if k in skip or not reparse_tag_line(line):
                continue
            if enclosing_fn(lines, k) not in REPARSE_TAG_DATA | REPARSE_TAG_REMAKE:
                errors.append(f"{path.relative_to(ROOT).as_posix()}:{k + 1}: a reparse tag decides "
                              "here; a list of tags misses the rest, so tell a link by "
                              "du_fs::fileid::is_link (the name-surrogate bit)")


# A trash seen through a bind mount has two paths and one device and inode;
# a set of paths listed it twice (trash-cli #301 and #341, gtrash #20). A fn
# that returns a list of trash roots drops repeats through
# trashdir::RootSet::first, never a set or dedup of its own.
# ROOT_SET_CASES tests the matcher.
ROOT_LIST_FN = re.compile(
    r"^(?:pub(?:\([^)]*\))?\s+)?fn\s+(\w+)[^{]*->\s*Vec<(?:trashdir::)?TrashDir>")
OWN_DEDUP = re.compile(r"\b(?:HashSet|BTreeSet)\b|\.dedup(?:_by\w*)?\(")
ROOT_SET_CASES = [
    ("paths", "pub fn all() -> Vec<TrashDir> {\n    let mut seen = HashSet::new();\n}", 1),
    ("dedup", "fn roots() -> Vec<trashdir::TrashDir> {\n    v.dedup();\n}", 1),
    ("rootset", "pub fn all() -> Vec<TrashDir> {\n    let mut seen = RootSet::default();\n}", 0),
    ("other", "fn names() -> Vec<String> {\n    let mut seen = HashSet::new();\n}", 0),
]


def own_dedup_lines(code):
    """0-based lines where a fn returning trash roots dedups on its own."""
    lines = code.split("\n")
    hits = []
    for line in lines:
        m = ROOT_LIST_FN.match(line)
        if m:
            body = sorted(fn_lines(lines, m.group(1)))
            hits.extend(j for j in body if OWN_DEDUP.search(lines[j]))
    return hits


def check_trash_root_sets():
    """Lists of trash roots drop repeats by device and inode (RootSet)."""
    for what, text, want in ROOT_SET_CASES:
        if len(own_dedup_lines(text)) != want:
            errors.append(f"scripts/precheck.py: own_dedup_lines gets ROOT_SET_CASES '{what}' "
                          "wrong; the trash-root set check is broken")
    for path in sorted((ROOT / "src").rglob("*.rs")):
        for k in own_dedup_lines(rust_code(path.read_text(encoding="utf-8"))):
            errors.append(f"{path.relative_to(ROOT).as_posix()}:{k + 1}: a list of trash roots "
                          "drops repeats on its own; a bind mount shows one trash at two paths, "
                          "so use trashdir::RootSet::first (device and inode)")


# gdu #439: the du walk threads meet the links of a file in any order, so
# the first sighting gave a different folder the file's size from run to
# run, and an ncdu export read back gave it to whichever link it wrote
# first. du::credits_first is the one rule: Ctx::counts_here applies it in
# a scan and du::LinkCredit to a saved tree (du_ncdu::Builder). A link
# dedupe anywhere else in src/du*.rs (first_link, an inode set insert)
# decides by order again. LINK_CREDIT_OK names the deciders and the inode
# sets that credit no folder size. LINK_CREDIT_CASES tests the matcher.
LINK_CREDIT = re.compile(r"\.first_link\s*\(|\bseen\.insert\s*\([^)]*\bino\b"
                         r"|\.(?:insert|entry)\s*\(\s*\([^)]*\bino\b")
LINK_CREDIT_OK = {
    ("src/du.rs", "counts_here"), ("src/du.rs", "first_link"),
    ("src/du.rs", "insert"),  # LinkSet::insert, behind first_link
    ("src/du.rs", "see"),  # LinkClaims::see, the scan's claims
    ("src/du_dupes.rs", "one_per_inode"),  # groups names; no sizes
    ("src/du_ncdu_bin.rs", "leaf"),  # ncdu's own per-directory sizes
}
LINK_CREDIT_CASES = [
    ("ctx", "        let first = ctx.first_link(&m);", True),
    ("set", "        self.seen.first_link(st)", True),
    ("pair", "        if linked && !self.seen.insert((dev, f.ino)) {", True),
    ("follow", "            m.ino == 0 || ctx.seen.insert(m.dev, m.ino)", True),
    ("entry", "        let l = links.entry((st.dev, st.ino)).or_insert(0);", True),
    ("routed", "            ctx.counts_here(&m, || path.join(du_fs::os(name)))", False),
    ("device", "        if size == 0 || !seen.insert(dev) {", False),
]


def check_link_credit():
    """In src/du*.rs, only du::credits_first decides which link counts."""
    for what, line, want in LINK_CREDIT_CASES:
        if bool(LINK_CREDIT.search(line)) != want:
            errors.append(f"scripts/precheck.py: LINK_CREDIT gets LINK_CREDIT_CASES '{what}' "
                          "wrong; the link-credit check is broken")
    for path in sorted((ROOT / "src").glob("du*.rs")):
        rel = path.relative_to(ROOT).as_posix()
        raw = path.read_text(encoding="utf-8")
        skip = test_mod_lines(raw.split("\n"))
        lines = rust_code(raw).split("\n")
        for k, line in enumerate(lines):
            if k in skip or not LINK_CREDIT.search(line):
                continue
            if (rel, enclosing_fn(lines, k)) in LINK_CREDIT_OK:
                continue
            errors.append(f"{rel}:{k + 1}: a hard link counted where it is met first goes to "
                          "whichever path came first; use Ctx::counts_here in a scan or "
                          "du::LinkCredit for a saved tree (du::credits_first)")


# dua #410 and #406: a Win32 call refuses a path of 260 characters or more
# unless it carries the verbatim prefix. util::win_path adds it from
# util::WIN_LONG units on; util::win_text converts a name that is not a
# path. An encode_wide anywhere else in src/ builds a wide path by hand and
# fails on a deep folder. WIN_PATH_OK names the converters and os_bytes,
# which writes WTF-8 for a key, not for Win32. WIN_PATH_CASES tests it.
WIN_WIDE = re.compile(r"\.encode_wide\s*\(")
WIN_PATH_OK = {("src/util.rs", "win_path"), ("src/util.rs", "win_text"),
               ("src/util.rs", "os_bytes")}
WIN_PATH_CASES = [
    ("chain", "    let w: Vec<u16> = p.as_os_str().encode_wide().chain(Some(0)).collect();", True),
    ("split", "                    .encode_wide()", True),
    ("helper", "    let w = crate::util::win_path(path);", False),
]


def check_win_paths():
    """Outside util's converters, nothing in src/ builds a wide string."""
    for what, line, want in WIN_PATH_CASES:
        if bool(WIN_WIDE.search(line)) != want:
            errors.append(f"scripts/precheck.py: WIN_WIDE gets WIN_PATH_CASES '{what}' wrong; "
                          "the Windows path check is broken")
    for path in sorted((ROOT / "src").rglob("*.rs")):
        rel = path.relative_to(ROOT).as_posix()
        raw = path.read_text(encoding="utf-8")
        skip = test_mod_lines(raw.split("\n"))
        lines = rust_code(raw).split("\n")
        for k, line in enumerate(lines):
            if k in skip or not WIN_WIDE.search(line):
                continue
            if (rel, enclosing_fn(lines, k)) in WIN_PATH_OK:
                continue
            errors.append(f"{rel}:{k + 1}: a wide string built by hand has no verbatim prefix, "
                          "so a path of 260 characters or more fails; use util::win_path (or "
                          "util::win_text for a name that is not a path)")


# gdu #407: on Windows the allocation size of a file that NTFS compression
# or Compact (WOF) packed is not its disk use. du_fs::fileid::needs_real_size
# says which files those are; GetCompressedFileSizeW gives their real size.
# A function that reads an allocation size and never asks it counts such a
# file at its full size. ALLOC_CASES tests the matcher. ALLOC_PACKED names
# the fns that read the allocation of a packed stream itself, which is
# the real size: du_fs windows wof_size (34(j)).
ALLOC_PACKED = {("src/du_fs/windows.rs", "wof_size")}
ALLOC_READ = re.compile(r"\bAllocationSize\b|\ballocation_size\s*\)")
ALLOC_CASES = [
    ("std", "    let mut allocated = std_info.AllocationSize.max(0) as u64;", True),
    ("listing", "    let a = i64_at(rec, offset_of!(I, allocation_size));", True),
    ("field", "        pub allocation_size: i64,", False),
]


def enclosing_fn_text(lines, k):
    """Lines from the fn declared at or above 0-based `k` to the next fn."""
    decl = re.compile(r"\s*(?:pub(?:\([^)]*\))?\s+)?(?:unsafe\s+)?fn\s+\w+")
    start = k
    while start > 0 and not decl.match(lines[start]):
        start -= 1
    end = k + 1
    while end < len(lines) and not decl.match(lines[end]):
        end += 1
    return "\n".join(lines[start:end])


def check_compressed_sizes():
    """Each fn in src/ that reads an allocation size asks needs_real_size."""
    for what, line, want in ALLOC_CASES:
        if bool(ALLOC_READ.search(line)) != want:
            errors.append(f"scripts/precheck.py: ALLOC_READ gets ALLOC_CASES '{what}' wrong; "
                          "the compressed-size check is broken")
    for path in sorted((ROOT / "src").rglob("*.rs")):
        rel = path.relative_to(ROOT).as_posix()
        raw = path.read_text(encoding="utf-8")
        skip = test_mod_lines(raw.split("\n"))
        lines = rust_code(raw).split("\n")
        for k, line in enumerate(lines):
            if k in skip or not ALLOC_READ.search(line):
                continue
            if "needs_real_size(" in enclosing_fn_text(lines, k):
                continue
            if (rel, enclosing_fn(lines, k)) in ALLOC_PACKED:
                continue
            errors.append(f"{rel}:{k + 1}: an allocation size read without needs_real_size counts "
                          "a compressed or Compact file at its full size; check "
                          "du_fs::fileid::needs_real_size and use GetCompressedFileSizeW")

# trash-cli #354: a blocking flock hangs without a word behind a long empty
# or a process stuck on a dead mount. The trash lock waits in
# trashdir::wait_for_lock, which names the trash after a second and gives up
# after RTRASH_LOCK_WAIT. A flock, lockf or F_SETLKW call anywhere else in
# src/ may block for good. LOCK_CALL_CASES tests the matcher.
LOCK_CALL = re.compile(r"\blibc::(?:flock|lockf)\s*\(|\bF_SETLKW\b")
LOCK_CALLERS = {"acquire_flags", "wait_for_lock"}
LOCK_CALL_CASES = [
    ("flock", "    let rc = unsafe { libc::flock(fd, libc::LOCK_EX) };", True),
    ("lockf", "    libc::lockf(fd, libc::F_LOCK, 0);", True),
    ("setlkw", "    libc::fcntl(fd, libc::F_SETLKW, &fl);", True),
    ("flag", "    Self::acquire_flags(root, libc::LOCK_EX, at)", False),
]


# 34(j): the Compact test asserted only `disk < 1 << 19`, so a du that
# gave a Compact file a disk use of 0 passed it. In tests/, an assert that
# puts a disk size below a bound also puts it above 0 (`0 < disk && ..`).
# DISK_CEILING_CASES tests the matcher.
DISK_CEILING = re.compile(r"\bassert!\(\s*\w*disk\w*\s*<")
DISK_CEILING_CASES = [
    ("ceiling", "        assert!(disk < 1 << 19, \"{name}: disk {disk}: {j}\");", True),
    ("both", "        assert!(0 < disk && disk < apparent, \"{name}\");", False),
    ("floor", "        assert!(disk > 0, \"{j}\");", False),
]


def check_disk_floors():
    """A test assert that caps a disk size also wants it above 0."""
    for what, line, want in DISK_CEILING_CASES:
        if bool(DISK_CEILING.search(line)) != want:
            errors.append(f"scripts/precheck.py: DISK_CEILING gets DISK_CEILING_CASES '{what}' "
                          "wrong; the disk floor check is broken")
    for path in sorted((ROOT / "tests").rglob("*.rs")):
        lines = rust_code(path.read_text(encoding="utf-8")).split("\n")
        for k, line in enumerate(lines):
            if DISK_CEILING.search(line):
                errors.append(f"{path.relative_to(ROOT).as_posix()}:{k + 1}: a disk size checked "
                              "only against a ceiling passes when du gives 0; assert "
                              "0 < disk && disk < N")


# 34(i): win_recycle::copy_tree walked a folder and handed a junction to
# fs::copy, which copies through a link or fails on one half way. A fn in
# src/ that walks folders (read_dir) and copies files (fs::copy) checks
# is_symlink() and makes a link again instead. LINK_COPY_CASES tests it.
LINK_COPY_CASES = [
    ("through", "fn c(s: &Path) {\n    for e in fs::read_dir(s)? {}\n    fs::copy(&a, &b)?;\n}", 1),
    ("remade", "fn c(s: &Path) {\n    if m.is_symlink() {}\n    fs::read_dir(s)?;\n"
               "    std::fs::copy(&a, &b)?;\n}", 0),
    ("one file", "fn c() {\n    fs::copy(&a, &b)?;\n}", 0),
]


def link_copy_errors(text):
    """The 1-based lines of fns in `text` that walk, copy and skip links."""
    lines = text.split("\n")
    decl = re.compile(r"\s*(?:pub(?:\([^)]*\))?\s+)?(?:unsafe\s+)?fn\s+\w+")
    starts = [k for k, line in enumerate(lines) if decl.match(line)] + [len(lines)]
    bad = []
    for a, b in zip(starts, starts[1:]):
        body = "\n".join(lines[a:b])
        if "read_dir(" in body and re.search(r"\bfs::copy\(", body) \
                and "is_symlink()" not in body:
            bad.append(a + 1)
    return bad


def check_link_copies():
    """A fn that walks folders and copies files makes each link again."""
    for what, text, want in LINK_COPY_CASES:
        if len(link_copy_errors(text)) != want:
            errors.append(f"scripts/precheck.py: link_copy_errors gets LINK_COPY_CASES '{what}' "
                          "wrong; the link copy check is broken")
    for path in sorted((ROOT / "src").rglob("*.rs")):
        raw = path.read_text(encoding="utf-8")
        skip = test_mod_lines(raw.split("\n"))
        for line in link_copy_errors(rust_code(raw)):
            if line - 1 in skip:
                continue
            errors.append(f"{path.relative_to(ROOT).as_posix()}:{line}: this fn walks folders and "
                          "copies files but never checks is_symlink(); fs::copy goes through a "
                          "link, so make the link again (win_recycle::copy_link)")




def enclosing_fn(lines, k):
    """The name of the last fn declared at or above 0-based line `k`."""
    for j in range(k, -1, -1):
        m = re.match(r"\s*(?:pub(?:\([^)]*\))?\s+)?(?:unsafe\s+)?fn\s+(\w+)", lines[j])
        if m:
            return m.group(1)
    return ""


def check_lock_waits():
    """Outside the tests, only the trash lock code calls flock."""
    for what, line, want in LOCK_CALL_CASES:
        if bool(LOCK_CALL.search(line)) != want:
            errors.append(f"scripts/precheck.py: LOCK_CALL gets LOCK_CALL_CASES '{what}' wrong; "
                          "the lock-wait check is broken")
    for path in sorted((ROOT / "src").rglob("*.rs")):
        raw = path.read_text(encoding="utf-8")
        skip = test_mod_lines(raw.split("\n"))
        lines = rust_code(raw).split("\n")
        for k, line in enumerate(lines):
            if k in skip or not LOCK_CALL.search(line) or enclosing_fn(lines, k) in LOCK_CALLERS:
                continue
            errors.append(f"{path.relative_to(ROOT).as_posix()}:{k + 1}: a file lock taken outside "
                          "TrashLock may wait for good; take it through trashdir::TrashLock")


# A path in a message goes through util::err_path (stderr) or
# util::term_path (stdout), which turn control characters into `?` on a
# terminal. A bare `.display()` or `.to_string_lossy()` in a print macro
# let a file name write escape sequences to the terminal (dua #365; 147
# such calls before round 265). PRINT_PATH_CASES tests the matcher.
PRINT_MACRO = re.compile(r"\b(?:e?println|e?print|say)!\s*\(")
RAW_PATH = re.compile(r"\.(?:display|to_string_lossy)\(\)")
PRINT_PATH_CASES = [
    ("display", 'eprintln!("{prog}: cannot read \'{}\': {e}", p.display());', 1),
    ("lossy", 'say!("{prog}: {}", name.to_string_lossy());', 1),
    ("split", 'println!(\n    "{}",\n    root.display()\n);', 1),
    ("routed", 'eprintln!("{prog}: \'{}\'", crate::util::err_path(&p));', 0),
    ("format", 'let s = format!("{}", p.display());', 0),
]


def raw_print_paths(code):
    """0-based lines of print macros whose arguments show a path raw."""
    hits = []
    for m in PRINT_MACRO.finditer(code):
        depth, j = 1, m.end()
        while j < len(code) and depth:
            depth += {"(": 1, ")": -1}.get(code[j], 0)
            j += 1
        if RAW_PATH.search(code[m.start():j]):
            hits.append(code.count("\n", 0, m.start()))
    return hits


def check_message_paths():
    """No print macro outside the tests shows a path raw."""
    for what, text, want in PRINT_PATH_CASES:
        if len(raw_print_paths(rust_code(text))) != want:
            errors.append(f"scripts/precheck.py: raw_print_paths gets PRINT_PATH_CASES '{what}' "
                          "wrong; the message-path check is broken")
    for path in sorted((ROOT / "src").rglob("*.rs")):
        raw = path.read_text(encoding="utf-8")
        skip = test_mod_lines(raw.split("\n"))
        for k in raw_print_paths(rust_code(raw)):
            if k not in skip:
                errors.append(f"{path.relative_to(ROOT).as_posix()}:{k + 1}: a print macro shows a "
                              "path raw; use crate::util::err_path (stderr) or term_path (stdout)")


# Btrfs decides at writeback which blocks it compresses (a range of one
# block goes out plain), so a test that asserts the exact count of encoded
# bytes fails at random: main CI 39154329766 got 16769024 for 2 * 8 MiB.
# ENCODED_EXACT_CASES tests the matcher.
ENCODED_EXACT_CASES = [
    ("exact", 'assert_eq!(\n    num(&j, "\\"shared\\":{", "encoded"),\n    2 * mib8,\n);', 1),
    ("bounded", 'let encoded = num(&j, "x", "encoded");\nassert!(encoded > 14 << 20);', 0),
    ("other", 'assert_eq!(num(&j, "x", "distinct"), mib8);', 0),
]


def exact_encoded_lines(code):
    """0-based lines where an assert_eq! or assert_ne! names "encoded"."""
    hits = []
    for m in re.finditer(r"\bassert_(?:eq|ne)!\(", code):
        depth, j = 1, m.end()
        while j < len(code) and depth:
            depth += {"(": 1, ")": -1}.get(code[j], 0)
            j += 1
        if '"encoded"' in code[m.start():j]:
            hits.append(code.count("\n", 0, m.start()))
    return hits


def check_exact_encoded():
    """No test pins the exact encoded byte count of a scan."""
    for what, text, want in ENCODED_EXACT_CASES:
        if len(exact_encoded_lines(text)) != want:
            errors.append(f"scripts/precheck.py: exact_encoded_lines gets ENCODED_EXACT_CASES "
                          f"'{what}' wrong; the encoded-bytes check is broken")
    for path in sorted((ROOT / "tests").rglob("*.rs")):
        code = rust_code(path.read_text(encoding="utf-8"), strings=True)
        for k in exact_encoded_lines(code):
            errors.append(f"{path.relative_to(ROOT).as_posix()}:{k + 1}: an exact count of "
                          "encoded bytes; btrfs leaves some blocks plain at random, so bound it")


# Every mklink in the tests goes through common::mklink_junction, which
# refuses a '/' in either path. cmd reads a '/' anywhere in an argument as a
# switch: round 263 passed "...\\d/again" and mklink failed with "Invalid
# switch". MKLINK_CASES tests the matcher.
MKLINK = re.compile(r'"mklink"')
MKLINK_CASES = [
    ("args", '        .args(["/C", "mklink", "/J"])', True),
    ("word", '    let tool = "mklink";', True),
    ("other", '        .args(["/C", "dir"])', False),
]


def check_mklink_calls():
    """A test makes a junction only through common::mklink_junction."""
    for what, line, want in MKLINK_CASES:
        if bool(MKLINK.search(line)) != want:
            errors.append(f"scripts/precheck.py: MKLINK gets MKLINK_CASES '{what}' wrong; "
                          "the mklink check is broken")
    for path in sorted((ROOT / "tests").rglob("*.rs")):
        rel = path.relative_to(ROOT).as_posix()
        lines = rust_code(path.read_text(encoding="utf-8"), strings=True).split("\n")
        own = rel == "tests/common/mod.rs"
        allowed = fn_lines(lines, "mklink_junction") if own else set()
        for k, line in enumerate(lines):
            if k not in allowed and MKLINK.search(line):
                errors.append(f"{rel}:{k + 1}: mklink outside mklink_junction; cmd takes a '/' "
                              "in a path for a switch, so make the junction through that helper")


# Bare rtrash runs allowed per test file. Empty since round 188: every test
# goes through common::rtrash or common::rtrash_real.
UNPINNED_LEFT: dict[str, int] = {}
UNPINNED = re.compile(r'Command::new\(\s*(?:bin\(\)|env!\("CARGO_BIN_EXE_rtrash"\)|rtrash)\s*\)')


def check_rtrash_pinned():
    """A test runs rtrash through common::rtrash(home), which pins HOME, the
    XDG folders and the macOS trash choice, or common::rtrash_real(why). A
    bare Command::new of the binary reaches the developer's own trash (on
    macOS the Finder Trash). Files not yet moved over have a fixed allowance."""
    for path in sorted((ROOT / "tests").glob("*.rs")):
        rel = path.relative_to(ROOT).as_posix()
        hits = [n for n, l in enumerate(path.read_text(encoding="utf-8").splitlines(), 1)
                if not l.lstrip().startswith("//") for _ in UNPINNED.findall(l)]
        allowed = UNPINNED_LEFT.get(rel, 0)
        if len(hits) > allowed:
            where = ", ".join(str(n) for n in hits)
            errors.append(f"{rel}: {len(hits)} bare Command::new of rtrash (allowed {allowed}; lines {where}); "
                          "use common::rtrash(home), or common::rtrash_real(Real::...) for the real trash")
        elif len(hits) < allowed:
            errors.append(f"{rel}: {len(hits)} bare Command::new of rtrash, fewer than the {allowed} "
                          "UNPINNED_LEFT allows; lower it in scripts/precheck.py")


def fn_body(text, name):
    """The text of fn `name` from its signature to the first `}` line at
    column 0; empty when there is none."""
    m = re.search(rf"(?m)^(?:pub(?:\([^)]*\))?\s+)?fn\s+{name}\b", text)
    if not m:
        return ""
    end = text.find("\n}\n", m.end())
    return text[m.start():end if end >= 0 else len(text)]


def check_bin_seam():
    """No test fills or reads the system Recycle Bin by accident (round 231:
    a parallel put changed the bin another test's du summary read).
    common::rtrash sets RTRASH_TEST_BIN_DIR, no test removes it, and the
    two places that reach the bin under that pin, the put hand-off and the
    du summary, ask win_recycle::test_bin_dir first. A test that needs the
    real bin uses common::rtrash_real(Real::RecycleBin)."""
    common = (ROOT / "tests" / "common" / "mod.rs").read_text(encoding="utf-8")
    if '.env("RTRASH_TEST_BIN_DIR"' not in fn_body(rust_code(common, strings=True), "rtrash"):
        errors.append("tests/common/mod.rs: rtrash(home) does not set RTRASH_TEST_BIN_DIR; "
                      "Windows runs would reach the system Recycle Bin")
    for path in sorted((ROOT / "tests").rglob("*.rs")):
        text = rust_code(path.read_text(encoding="utf-8"), strings=True)
        for m in re.finditer(r'env_remove\(\s*"RTRASH_TEST_BIN_DIR"', text):
            errors.append(f"{path.relative_to(ROOT).as_posix()}:{text.count(chr(10), 0, m.start()) + 1}: "
                          "removes the scratch bin pin; use common::rtrash_real(Real::RecycleBin) "
                          "for the real bin")
    for rel, fn in (
        ("src/allow_roots.rs", "trash_delete_as"),
        ("src/du.rs", "recycle_bin_on_scanned_drives"),
    ):
        body = fn_body(rust_code((ROOT / rel).read_text(encoding="utf-8")), fn)
        if "test_bin_dir()" not in body:
            errors.append(f"{rel}: {fn} does not ask win_recycle::test_bin_dir() first; "
                          "a test under common::rtrash would reach the system Recycle Bin")


def check_du_json_compares():
    """A test compares two du --json outputs through common::du_scan, which
    cuts off the live trash summary: it counts the trash at that moment,
    and round 231 went red on Windows when a parallel put changed it
    between two runs. Flag a name bound to du --json output (Tree::json,
    a run whose arguments hold "--json", or one that uses the variable of
    a loop over a list holding "--json") that an assert_eq!/assert_ne!
    compares whole. A value a free function takes from the output (a
    field, the listed paths) is not the whole output."""
    for path in sorted((ROOT / "tests").glob("*.rs")):
        rel = path.relative_to(ROOT).as_posix()
        text = rust_code(path.read_text(encoding="utf-8"), strings=True)
        line_of = lambda k: text.count("\n", 0, k) + 1
        loop_vars = set(re.findall(r'\bfor\s+(\w+)\s+in\s+\[[^\]]*"--json"', text))
        run = re.compile(r"\.(?:du|du_env)\s*\(|\.output\s*\(")
        for m in re.finditer(r"\blet\s+(?:mut\s+)?(\w+)\s*=\s*([^;]*);", text):
            name, rhs = m.group(1), m.group(2)
            # A free function around the run (paths_in, field) takes a part.
            if re.match(r"\s*&?(?!common::rtrash|String::from_utf8)[A-Za-z_][\w:]*\s*\(", rhs):
                continue
            from_loop = run.search(rhs) and any(re.search(rf"\b{v}\b", rhs) for v in loop_vars)
            if not (re.search(r"\.json\s*\(", rhs) or (run.search(rhs) and '"--json"' in rhs) or from_loop):
                continue
            end = re.compile(r"\n(?:#\[|fn |pub fn )").search(text, m.end())
            scope = text[m.end():end.start() if end else len(text)]
            whole = re.compile(rf"assert_(?:eq|ne)!\(\s*(?:{name}\s*,|[^;]*?,\s*{name}\s*[,)])")
            hit = whole.search(scope)
            if hit:
                errors.append(f"{rel}:{line_of(m.end() + hit.start())}: compares the du --json output "
                              f"`{name}` whole; compare common::du_scan of both, which leaves out "
                              "the live trash summary")


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
    if "--commit-msg" in sys.argv:
        return commit_msg_hook(sys.argv[sys.argv.index("--commit-msg") + 1])
    base = sys.argv[sys.argv.index("--base") + 1] if "--base" in sys.argv else None
    check_rust_code(); check_stale_cases(); check_split_cases(); check_completions(); check_test_tags(); check_pid_paths(); check_long_sleeps(); check_clock_waits(); check_tight_elapsed()
    check_fn_params(); check_permanent_gate(); check_test_seams(); check_os_trash_scoped(); check_cfg_helpers(); check_refusal_helpers(); check_rtrash_pinned(); check_bin_seam(); check_du_json_compares(); check_du_rows(); check_record_sections(); check_lock_rechecks(); check_scan_rechecks(); check_put_says(); check_browser_prints(); check_dot_operands(); check_copy_then_remove(); check_volume_trash_owner(); check_whole_file_writes(); check_key_reads(); check_prompt_answers(); check_doc_gaps(); check_size_sums(); check_size_args(); check_report_literals(); check_explorer_walks(); check_chain_totals(); check_mount_probes(); check_bounded_walks(); check_missing_looks(); check_filtered_opens(); check_mount_table_reads(); check_reparse_links(); check_mklink_calls(); check_exact_encoded(); check_message_paths(); check_lock_waits(); check_trash_root_sets(); check_link_credit(); check_win_paths(); check_compressed_sizes(); check_disk_floors(); check_link_copies(); check_directorysizes_paths()
    check_long_lines(base); check_struct_field_drift(base); check_split_attributes(base); check_removed_literals(base); check_commit_subjects(base); check_codespell(base)
    check_musl_aliases(); check_libc_const_args(); check_copy_progress()
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
