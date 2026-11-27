"""Lexical gold for TypeScript, Go and Java: definitions, call sites and import statements, from the source
text alone (stdlib only, no basemind, no parser).

Python's gold comes from `ast`; these three languages have no parser in the standard library, so every
extractor here is a scanner over the text with comments and string literals blanked, and each one declines
what it cannot read exactly. A name whose any occurrence is ambiguous (a call that might be a declaration, a
call whose receiver starts on an earlier line) is dropped from the candidates, so a task is gold only where
the text is unambiguous; the loss is candidates, never correctness.

What basemind lists (`src/queries/<language>.scm`), and so what the gold must hold:

- symbols: a declaration that is not function-local (`is_function_local` in `src/extract/l1.rs`), at the
  line its node starts, which includes the annotations and decorators above it. A brace scan classifies each
  `{` as a container (class, interface, enum, namespace, record; Go struct and interface) or a block, and a
  line is read only where every enclosing brace is a container and no parenthesis is open.
- references: a call expression, by its callee name, at the line the expression starts. A `new X(...)` is not
  a call. A method call counts only when its receiver is a plain identifier chain on the same line, so the
  expression starts where the name does.
- dependents: the text of an import statement (`import_statement` in TypeScript, `import_declaration` in Go).
  Java has no `imports` section, so Java has no dependents gold.

Known undercounts, which lower a tool's precision and never its recall: Go grouped `const (...)` and `var (...)`
specs, TypeScript interface members and object-literal methods, and Java record components are not listed.
"""

from __future__ import annotations

import bisect
import re
from dataclasses import dataclass

import _edges as e

IDENT = r"[A-Za-z_$][\w$]*"

# The extensions each language's gold reads, as `--ext` takes them.
EXTENSIONS = {"typescript": "ts,tsx,mts,cts", "go": "go", "java": "java"}
LANGUAGES = tuple(EXTENSIONS)
# The structural kinds each language has gold for (Java has no import statements in basemind's index).
KINDS = {
    "typescript": ("symbols", "references", "dependents"),
    "go": ("symbols", "references", "dependents"),
    "java": ("symbols", "references"),
}

# A raw (multi-line) string opener per language; Go's backtick string and Java's text block.
RAW_QUOTE = {"go": "`", "java": '"""'}

# Words that are never a symbol or callee name.
KEYWORDS = frozenset(
    "if else for while do switch case default catch try finally return throw new this super function func "
    "class interface enum struct type const let var import export await async yield typeof void delete in of "
    "instanceof select go defer package synchronized assert require record".split()
)
# A word directly before a call that leaves it a call (anything else there reads as a declaration's type).
CALL_PRECEDERS = frozenset("return throw await yield case else typeof delete in of go defer do assert".split())
# What may follow a line-start `name(...)` that makes it a declaration: a body, a return type, a throws clause.
DECL_RESTS = re.compile(r"[\w$({\[*:]")


def blank_noncode(src: str, lang: str, *, keep_strings: bool = False) -> str:
    """`src` with comments (and string literals, unless `keep_strings`) overwritten by spaces; every newline
    stays, so an offset's line number is the file's."""
    if lang == "typescript":
        text, starts, ends = e._ts_code(src)
        if keep_strings:
            return text
        out = list(text)
        for a, b in zip(starts, ends, strict=True):
            _blank(out, a, b)
        return "".join(out)
    raw = RAW_QUOTE[lang]
    out = list(src)
    n = len(src)
    i = 0
    while i < n:
        if src.startswith("//", i):
            end = src.find("\n", i)
            end = n if end < 0 else end
            _blank(out, i, end)
            i = end
        elif src.startswith("/*", i):
            end = src.find("*/", i + 2)
            end = n if end < 0 else end + 2
            _blank(out, i, end)
            i = end
        elif src.startswith(raw, i):
            end = src.find(raw, i + len(raw))
            end = n if end < 0 else end + len(raw)
            if not keep_strings:
                _blank(out, i, end)
            i = end
        elif src[i] in "\"'":
            quote = src[i]
            j = i + 1
            while j < n and src[j] not in (quote, "\n"):
                j += 2 if src[j] == "\\" else 1
            j = min(j + 1, n)
            if not keep_strings:
                _blank(out, i, j)
            i = j
        else:
            i += 1
    return "".join(out)


def _blank(out: list[str], a: int, b: int) -> None:
    for k in range(a, min(b, len(out))):
        if out[k] != "\n":
            out[k] = " "


Scope = tuple[str, str] | None

CONTAINER = {
    "typescript": re.compile(rf"(?<![\w$.])(class|interface|enum|namespace|module)\b(?:\s+({IDENT}))?"),
    "java": re.compile(rf"(?<![\w$.])(class|interface|enum|record)\s+({IDENT})"),
    "go": re.compile(rf"\btype\s+({IDENT})(?:\[[^\]]*\])?\s+(struct|interface)\s*$"),
}


def _container(header: str, lang: str) -> tuple[str, str] | None:
    m = CONTAINER[lang].search(header)
    if m is None:
        return None
    if lang == "go":
        return m.group(2), m.group(1)
    return m.group(1), m.group(2) or ""


def line_scopes(code: str, lang: str) -> list[Scope]:
    """Per line, the innermost container the line starts in: `("top", "")` at file level, `("class", "Foo")`
    in a class body, and None where a block or an open parenthesis makes the line a statement or an
    argument list (a declaration there is function-local, or none)."""
    stack: list[tuple[str, str] | None] = []
    blocks = parens = boundary = 0
    scopes: list[Scope] = [("top", "")]
    for i, ch in enumerate(code):
        if ch == "\n":
            scopes.append(None if parens or blocks else (stack[-1] if stack else ("top", "")))
        elif ch in "([":
            parens += 1
        elif ch in ")]":
            parens = max(parens - 1, 0)
        elif ch == "{":
            entry = None if parens else _container(code[boundary:i], lang)
            stack.append(entry)
            blocks += entry is None
            boundary = i + 1 if not parens else boundary
        elif ch == "}" and stack:
            blocks -= stack.pop() is None
            boundary = i + 1 if not parens else boundary
        elif ch == ";" and not parens:
            boundary = i + 1
    return scopes


@dataclass(frozen=True)
class Rule:
    """A declaration shape: `pattern` matches a line start and names the symbol in group `name`, in a
    container whose kind is in `scopes`; a `constructor` rule keeps only the container's own name."""

    pattern: re.Pattern[str]
    scopes: frozenset[str]
    constructor: bool = False


def _rule(pattern: str, scopes: str, *, constructor: bool = False) -> Rule:
    return Rule(re.compile(pattern), frozenset(scopes.split()), constructor)


_TS_MODS = (
    r"(?:(?:export|default|declare|abstract|public|private|protected|static|readonly|override|async|accessor)\s+)*"
)
_TS_ANNO = r"(?:@[\w.$]+(?:\([^)]*\))?\s+)*"
_JAVA_MODS = (
    r"(?:(?:public|protected|private|static|final|abstract|synchronized|native|strictfp|default|transient|"
    r"volatile|sealed|non-sealed)\s+)*"
)
_JAVA_ANNO = r"(?:@[\w.]+(?:\([^)]*\))?\s+)*"
_JAVA_TYPE = r"[\w.]+(?:<[^;(){}=]*>)?(?:\[\])*"
_JAVA_MEMBER = "class interface enum record"

RULES = {
    "typescript": [
        _rule(
            rf"\s*{_TS_ANNO}{_TS_MODS}(?:function\*?|class|interface|enum|namespace)\s+(?P<name>{IDENT})",
            "top namespace module",
        ),
        _rule(rf"\s*{_TS_MODS}type\s+(?P<name>{IDENT})\s*[<=]", "top namespace module"),
        _rule(rf"\s*{_TS_MODS}(?:const|let|var)\s+(?P<name>{IDENT})\s*[:=;]", "top namespace module"),
        _rule(rf"\s*{_TS_ANNO}{_TS_MODS}(?:get\s+|set\s+)?\*?\s*(?P<name>{IDENT})\s*\??\s*(?:<[^>(]*>)?\(", "class"),
        _rule(rf"\s*{_TS_ANNO}{_TS_MODS}(?P<name>{IDENT})\s*[?!]?\s*(?::[^=;]*)?[=;]", "class"),
    ],
    "go": [
        _rule(rf"func\s+(?:\([^)]*\)\s*)?(?P<name>{IDENT})", "top"),
        _rule(rf"type\s+(?P<name>{IDENT})", "top"),
        _rule(rf"(?:const|var)\s+(?P<name>{IDENT})", "top"),
        _rule(rf"\s+(?P<name>{IDENT})\s*[(\[]", "interface"),
    ],
    "java": [
        _rule(
            rf"\s*{_JAVA_ANNO}{_JAVA_MODS}(?:class|interface|enum|record|@interface)\s+(?P<name>{IDENT})",
            f"top {_JAVA_MEMBER}",
        ),
        _rule(rf"\s*{_JAVA_ANNO}{_JAVA_MODS}(?P<name>[A-Z][\w$]*)\s*\(", _JAVA_MEMBER, constructor=True),
        _rule(rf"\s*{_JAVA_ANNO}{_JAVA_MODS}(?:<[^>]+>\s+)?{_JAVA_TYPE}\s+(?P<name>{IDENT})\s*\(", _JAVA_MEMBER),
        _rule(rf"\s*{_JAVA_ANNO}{_JAVA_MODS}{_JAVA_TYPE}\s+(?P<name>{IDENT})\s*[=;,]", _JAVA_MEMBER),
    ],
}
_ANNOTATION_HEAD = re.compile(r"\s*@[\w.$]+\s*")
# How many lines above a declaration a multi-line annotation may open.
_ANNOTATION_SPAN = 40


def _annotation_start(lines: list[str], end: int) -> int | None:
    """The index of the line an annotation or decorator that ends on line `end` starts on, or None when that
    line does not end one. The argument list may span lines: parentheses are matched backwards from the
    closing one."""
    tail = lines[end].rstrip()
    if not tail.endswith(")"):
        return end if _ANNOTATION_HEAD.fullmatch(tail) else None
    depth = 0
    for k in range(end, max(end - _ANNOTATION_SPAN, -1), -1):
        text = tail if k == end else lines[k]
        for pos in range(len(text) - 1, -1, -1):
            depth += (text[pos] == ")") - (text[pos] == "(")
            if text[pos] == "(" and depth == 0:
                return k if _ANNOTATION_HEAD.fullmatch(text[:pos]) else None
    return None


def _decl_start(lines: list[str], idx: int, lang: str) -> int:
    """The index of the first line of the declaration at `idx`: annotation and decorator lines above it, whole
    multi-line ones included, belong to the node."""
    while lang != "go" and idx > 0:
        start = _annotation_start(lines, idx - 1)
        if start is None:
            break
        idx = start
    return idx


def symbols(src: str, lang: str) -> list[tuple[str, int]]:
    """`(name, 1-based line)` of every declaration of `src` that basemind lists as a symbol."""
    code = blank_noncode(src, lang)
    lines = code.split("\n")
    out: list[tuple[str, int]] = []
    for idx, scope in enumerate(line_scopes(code, lang)):
        if scope is None:
            continue
        for rule in RULES[lang]:
            m = rule.pattern.match(lines[idx]) if scope[0] in rule.scopes else None
            name = m.group("name") if m else None
            if name and name not in KEYWORDS and (not rule.constructor or name == scope[1]):
                out.append((name, _decl_start(lines, idx, lang) + 1))
                break
    return out


CALL = re.compile(rf"(?P<name>{IDENT})\s*(?P<generic><[^<>(){{}};=]*>\s*)?\(")


def _prev_index(code: str, start: int) -> int:
    idx = start - 1
    while idx >= 0 and code[idx] in " \t":
        idx -= 1
    return idx


def _after_call(code: str, open_idx: int) -> str | None:
    """The rest of the line after the parenthesis opened at `open_idx` closes; None when it never does."""
    depth = 0
    for j in range(open_idx, min(len(code), open_idx + 4000)):
        if code[j] == "(":
            depth += 1
        elif code[j] == ")":
            depth -= 1
            if depth == 0:
                end = code.find("\n", j)
                return code[j + 1 : len(code) if end < 0 else end].strip()
    return None


def _receiver_starts_here(code: str, dot: int) -> bool:
    """True when the `.` at `dot` follows a plain identifier chain that begins on this line, so the call
    expression starts where the chain does and no earlier line holds its start."""
    j = dot - 1
    if j >= 0 and code[j] == "?":
        j -= 1
    if j < 0 or not (code[j].isalnum() or code[j] in "_$"):
        return False
    while j >= 0 and (code[j].isalnum() or code[j] in "_$."):
        j -= 1
    chain_start = j + 1
    return not code[chain_start:].startswith(".") and (j < 0 or code[j] not in ")]")


def _continues_earlier_line(code: str, idx: int) -> bool:
    """True when the last token before `idx`, looking back across newlines, is a `.`: the name continues a
    member access that began on an earlier line."""
    while idx >= 0 and code[idx] in " \t\r\n":
        idx -= 1
    return idx >= 0 and code[idx] == "."


def _is_new_operand(code: str, dot: int) -> bool:
    """True when the identifier chain that ends at the `.` at `dot` is the operand of a `new` expression."""
    j = dot - 1
    while j >= 0 and (code[j].isalnum() or code[j] in "_$."):
        j -= 1
    while j >= 0 and code[j] in " \t\r\n":
        j -= 1
    end = j + 1
    while j >= 0 and (code[j].isalnum() or code[j] in "_$"):
        j -= 1
    return code[j + 1 : end] == "new"


def _is_call(code: str, m: re.Match[str], *, in_interface: bool = False) -> bool:
    """True when the `name(` at `m` can only be a call expression starting on the name's own line;
    `in_interface` is set when that line starts directly inside an interface body, where a bodyless
    `name(...)` is a member declaration."""
    if m.group("generic"):
        return False
    idx = _prev_index(code, m.start("name"))
    prev = code[idx] if idx >= 0 else "\n"
    if prev == "\n":
        if in_interface or _continues_earlier_line(code, idx):
            return False
        rest = _after_call(code, m.end() - 1)
        return rest is not None and not DECL_RESTS.match(rest)
    if prev == ".":
        return _receiver_starts_here(code, idx) and not _is_new_operand(code, idx)
    if prev.isalnum() or prev in "_$":
        end = idx + 1
        while idx >= 0 and (code[idx].isalnum() or code[idx] in "_$"):
            idx -= 1
        return code[idx + 1 : end] in CALL_PRECEDERS
    if prev == ">":
        return idx > 0 and code[idx - 1] in "=-"
    return prev not in ")]@"


def call_sites(src: str, lang: str) -> tuple[dict[str, list[int]], set[str]]:
    """`name -> 1-based lines` of the calls in `src` and the names with an occurrence that is not plainly a
    call (a declaration, a constructor, a call chained over several lines): such a name is no candidate."""
    code = blank_noncode(src, lang)
    newlines = [i for i, ch in enumerate(code) if ch == "\n"]
    scopes = line_scopes(code, lang)
    sites: dict[str, list[int]] = {}
    unclear: set[str] = set()
    for m in CALL.finditer(code):
        name = m.group("name")
        if name in KEYWORDS:
            continue
        line = bisect.bisect_left(newlines, m.start("name"))
        scope = scopes[line] if line < len(scopes) else None
        if _is_call(code, m, in_interface=scope is not None and scope[0] == "interface"):
            sites.setdefault(name, []).append(line + 1)
        else:
            unclear.add(name)
    return sites, unclear


TS_IMPORT = re.compile(
    r"^[ \t]*import(?![\w$(.])\s*(?P<clause>[^;'\"`]*?)(?P<q>['\"])(?P<spec>[^'\"\n]*)(?P=q)", re.MULTILINE
)
GO_IMPORT = re.compile(r"^import\b(?:\s*\((?P<block>[^)]*)\)|(?P<single>[^\n]*))", re.MULTILINE)
GO_PATH = re.compile(r'"([^"\n]+)"')


def import_statements(src: str, lang: str) -> list[tuple[str, list[str]]]:
    """`(statement text, module specifiers)` for each import statement of `src`."""
    code = blank_noncode(src, lang, keep_strings=True)
    if lang == "typescript":
        return [
            (m.group(0).strip(), [m.group("spec")])
            for m in TS_IMPORT.finditer(code)
            if not m.group("clause").strip() or m.group("clause").rstrip().endswith("from")
        ]
    if lang == "go":
        masked = blank_noncode(src, lang)
        # An `import` keyword inside a raw string is blanked in `masked`; a real one is not.
        return [(m.group(0), GO_PATH.findall(m.group(0))) for m in GO_IMPORT.finditer(code) if masked[m.start()] == "i"]
    return []


def decl_grep(lang: str, name: str) -> str:
    """The `git grep -E` pattern an agent would run to find where `name` is declared."""
    name = re.sub(r"([$.^*+?()\[\]{}|\\])", r"\\\1", name)
    patterns = {
        "typescript": (
            f"(function|class|interface|enum|type|const|let|var)[[:space:]]+[A-Za-z_$0-9]*{name}"
            f"|^[[:space:]]+([a-z]+[[:space:]]+)*[A-Za-z_$0-9]*{name}[A-Za-z_$0-9]*[[:space:]]*[?!]?[(:=<;]"
        ),
        "go": f"^(func|type|var|const)[[:space:]].*{name}|^[[:space:]]+[A-Za-z_0-9]*{name}[A-Za-z_0-9]*[[:space:]]*[([]",
        "java": (
            f"(class|interface|enum|record)[[:space:]]+[A-Za-z_0-9]*{name}"
            f"|[[:space:]][A-Za-z_0-9]*{name}[A-Za-z_0-9]*[[:space:]]*\\("
            f"|[[:space:]][A-Za-z_0-9$]*{name}[A-Za-z_0-9$]*[[:space:]]*[=;,]"
        ),
    }
    return patterns[lang]


def pathspec(lang: str) -> list[str]:
    return ["*." + ext for ext in EXTENSIONS[lang].split(",")]
