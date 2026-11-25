"""Tests for the TypeScript, Go and Java gold (`_langs.py`, `gen_structural.py`):
`python3 -m unittest discover benchmarks/eval`.

Every expected line is worked out from the fixture sources below, found by the text of its line so the
fixture can be read next to the assertion.
"""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).parent
sys.path.insert(0, str(HERE))

import _langs as lg  # noqa: E402
from test_analysis_generators import git  # noqa: E402

TS = """import { helperAlpha } from "./helpers.js";
import type { Config } from "./config";
export { reexported } from "./reexport";
const required = require("./required");
const lazy = import("./dynamic");
// ghostCall(1) is only a comment
export interface Widget {
  render(): void;
}

export class WidgetBox<T> {
  private items: T[] = [];

  @Memo()
  public addItem(item: T): void {
    const local = computeLocal(item);
    this.items.push(item);
  }

  static create(): WidgetBox<number> {
    return new WidgetBox<number>();
  }
}

export function buildWidget(name: string): Widget {
  const inner = () => 1;
  return makeWidget(name);
}

export const DEFAULT_LIMIT = 10;
export type WidgetId = string;
const text = "fakeCall(1)";
"""

TS_OTHER = 'import { helperAlpha } from "./helpers.js";\nexport const otherValue = 1;\n'

GO = """package demo

import (
	"fmt"
	str "strings"
)

// ghostCall(2) is only a comment
type Walker interface {
	Walk(path string) error
}

type Runner struct {
	name string
}

func (r *Runner) Execute(args []string) error {
	local := trimAll(args)
	fmt.Println(local, "stringCall(3)")
	return r.finish()
}

func Standalone() int {
	return 1
}

const DefaultName = "x"
var counter int
"""

JAVA = """package demo;

import java.util.List;
import static java.util.Objects.requireNonNull;

/** Javadoc ghostCall(1) */
public class Greeter {
    private final String prefix = "x";
    private static final int LIMIT = 3;

    public Greeter(String prefix) {
        this.prefix = requireNonNull(prefix);
    }

    @Override
    public String toString() {
        String text = format(prefix);
        return text;
    }

    List<String> names(int count) {
        return java.util.Collections.nCopies(count, prefix);
    }

    interface Sink {
        void accept(String value);
    }
}
"""

FILES = {"web/widgets.ts": TS, "web/other.ts": TS_OTHER, "svc/main.go": GO, "lib/Greeter.java": JAVA}


def line_of(src: str, needle: str) -> int:
    """The 1-based line of the only line of `src` containing `needle`."""
    hits = [i for i, line in enumerate(src.splitlines(), 1) if needle in line]
    assert len(hits) == 1, (needle, hits)
    return hits[0]


class Scanner(unittest.TestCase):
    def test_typescript_symbols_skip_locals_and_start_at_the_decorator(self) -> None:
        got = lg.symbols(TS, "typescript")
        self.assertEqual(
            got,
            [
                ("required", line_of(TS, "const required")),
                ("lazy", line_of(TS, "const lazy")),
                ("Widget", line_of(TS, "export interface Widget")),
                ("WidgetBox", line_of(TS, "export class WidgetBox")),
                ("items", line_of(TS, "private items")),
                ("addItem", line_of(TS, "@Memo()")),
                ("create", line_of(TS, "static create")),
                ("buildWidget", line_of(TS, "export function buildWidget")),
                ("DEFAULT_LIMIT", line_of(TS, "DEFAULT_LIMIT")),
                ("WidgetId", line_of(TS, "export type WidgetId")),
                ("text", line_of(TS, "const text")),
            ],
        )

    def test_go_symbols_read_the_interface_method_and_skip_struct_fields_and_locals(self) -> None:
        got = lg.symbols(GO, "go")
        self.assertEqual(
            got,
            [
                ("Walker", line_of(GO, "type Walker")),
                ("Walk", line_of(GO, "Walk(path")),
                ("Runner", line_of(GO, "type Runner")),
                ("Execute", line_of(GO, "func (r *Runner) Execute")),
                ("Standalone", line_of(GO, "func Standalone")),
                ("DefaultName", line_of(GO, "const DefaultName")),
                ("counter", line_of(GO, "var counter")),
            ],
        )

    def test_java_symbols_hold_constructors_members_and_annotated_starts(self) -> None:
        got = lg.symbols(JAVA, "java")
        self.assertEqual(
            got,
            [
                ("Greeter", line_of(JAVA, "public class Greeter")),
                ("prefix", line_of(JAVA, "private final String prefix")),
                ("LIMIT", line_of(JAVA, "LIMIT")),
                ("Greeter", line_of(JAVA, "public Greeter(")),
                ("toString", line_of(JAVA, "@Override")),
                ("names", line_of(JAVA, "List<String> names")),
                ("Sink", line_of(JAVA, "interface Sink")),
                ("accept", line_of(JAVA, "void accept")),
            ],
        )

    def test_a_call_site_is_the_line_its_expression_starts_on(self) -> None:
        cases = {
            "typescript": (
                TS,
                {
                    "computeLocal": [line_of(TS, "computeLocal(item)")],
                    "push": [line_of(TS, "this.items.push")],
                    "makeWidget": [line_of(TS, "makeWidget(name)")],
                },
            ),
            "go": (
                GO,
                {
                    "trimAll": [line_of(GO, "trimAll(args)")],
                    "Println": [line_of(GO, "fmt.Println")],
                    "finish": [line_of(GO, "r.finish()")],
                },
            ),
            "java": (
                JAVA,
                {
                    "requireNonNull": [line_of(JAVA, "requireNonNull(prefix)")],
                    "format": [line_of(JAVA, "format(prefix)")],
                    "nCopies": [line_of(JAVA, "nCopies")],
                },
            ),
        }
        for lang, (src, expected) in cases.items():
            with self.subTest(lang=lang):
                sites, _ = lg.call_sites(src, lang)
                self.assertEqual(sites, expected)

    def test_a_name_with_an_ambiguous_occurrence_is_not_a_candidate(self) -> None:
        cases = {
            "typescript": (TS, {"Memo", "WidgetBox", "addItem", "render", "buildWidget", "create"}),
            "go": (GO, {"Walk", "Execute", "Standalone"}),
            "java": (JAVA, {"toString", "accept", "Greeter", "names"}),
        }
        for lang, (src, expected) in cases.items():
            with self.subTest(lang=lang):
                sites, unclear = lg.call_sites(src, lang)
                self.assertTrue(expected <= unclear, unclear)
                self.assertFalse(expected & set(sites))

    def test_comments_and_strings_hold_no_symbols_or_calls(self) -> None:
        for lang, src in {"typescript": TS, "go": GO, "java": JAVA}.items():
            with self.subTest(lang=lang):
                sites, unclear = lg.call_sites(src, lang)
                self.assertFalse({"ghostCall", "fakeCall", "stringCall"} & (set(sites) | unclear))

    def test_a_chain_started_on_an_earlier_line_is_not_a_call_site(self) -> None:
        src = "const a = builder\n  .step(1)\n  .finish();\nplain(2);\n"
        sites, unclear = lg.call_sites(src, "typescript")
        self.assertEqual(sites, {"plain": [4]})
        self.assertEqual(unclear, {"step", "finish"})

    def test_a_multi_line_annotation_belongs_to_the_declaration_start(self) -> None:
        java = 'class A {\n    @SuppressWarnings(\n        "unchecked"\n    )\n    @Deprecated\n    public void processItems() {}\n}\n'
        self.assertEqual(lg.symbols(java, "java"), [("A", 1), ("processItems", 2)])
        ts = "class A {\n  @Input({\n    required: true,\n  })\n  value = 1;\n  other(): void {}\n}\n"
        self.assertEqual(lg.symbols(ts, "typescript"), [("A", 1), ("value", 2), ("other", 6)])

    def test_a_call_whose_receiver_is_on_the_previous_line_is_not_a_call_site(self) -> None:
        src = "object.\n  methodName();\nobject\n  .other();\nplain();\n"
        sites, unclear = lg.call_sites(src, "typescript")
        self.assertEqual(sites, {"plain": [5]})
        self.assertEqual(unclear, {"methodName", "other"})

    def test_a_bodyless_interface_member_is_not_a_call_site(self) -> None:
        cases = {
            "typescript": "interface Sink {\n  flushBuffer();\n}\nfunction run() {\n  flushBuffer();\n}\n",
            "go": "type Sink interface {\n\tFlushBuffer()\n}\nfunc run() {\n\tFlushBuffer()\n}\n",
        }
        for lang, src in cases.items():
            with self.subTest(lang=lang):
                sites, unclear = lg.call_sites(src, lang)
                name = "flushBuffer" if lang == "typescript" else "FlushBuffer"
                self.assertIn(name, unclear)
                self.assertNotIn(2, sites.get(name, []))
        sites, unclear = lg.call_sites("function run() {\n  flushBuffer();\n}\n", "typescript")
        self.assertEqual(sites, {"flushBuffer": [2]})
        self.assertNotIn("flushBuffer", unclear)

    def test_a_qualified_constructor_is_not_a_call_site(self) -> None:
        for lang, src in {
            "typescript": "new makers.WidgetFactory();\nmakers.WidgetBuilder();\n",
            "java": "class A {\n  void f() {\n    new makers.WidgetFactory();\n    makers.WidgetBuilder();\n  }\n}\n",
        }.items():
            with self.subTest(lang=lang):
                sites, unclear = lg.call_sites(src, lang)
                self.assertEqual(sites, {"WidgetBuilder": [2 if lang == "typescript" else 4]})
                self.assertIn("WidgetFactory", unclear)
                self.assertNotIn("WidgetBuilder", unclear)

    def test_go_import_lines_inside_a_raw_string_are_not_imports(self) -> None:
        src = 'package p\n\nimport "real/pkg"\n\nvar text = `\nimport "example.com/phantom"\n`\n'
        found = lg.import_statements(src, "go")
        self.assertEqual([specs for _, specs in found], [["real/pkg"]])

    def test_the_declaration_grep_finds_members_the_scanner_lists(self) -> None:
        cases = [
            ("typescript", "  public addItem(item: T): void {\n", "addItem"),
            ("typescript", "  private items: T[] = [];\n", "items"),
            ("java", "    private final String prefix = \"x\";\n", "prefix"),
            ("java", "    private int count;\n", "count"),
            ("go", "\tWalk(path string) error\n", "Walk"),
            ("typescript", "  const $emit = make();\n", "$emit"),
            ("typescript", "  public $emit(): void {\n", "$emit"),
            ("java", "    private int $count;\n", "$count"),
        ]
        for lang, src, name in cases:
            with self.subTest(lang=lang, name=name), tempfile.TemporaryDirectory() as tmp:
                target = Path(tmp) / "decl.txt"
                target.write_text(src)
                found = subprocess.run(
                    ["grep", "-n", "-E", lg.decl_grep(lang, name), str(target)], capture_output=True, text=True
                )
                self.assertEqual(found.returncode, 0, found.stdout)
                self.assertIn(name, found.stdout)

    def test_typescript_imports_are_import_statements_only(self) -> None:
        found = lg.import_statements(TS, "typescript")
        self.assertEqual([specs for _, specs in found], [["./helpers.js"], ["./config"]])
        self.assertIn("helperAlpha", found[0][0])

    def test_a_multi_line_typescript_import_is_one_statement(self) -> None:
        src = 'import {\n  alpha,\n  beta,\n} from "./pair";\n'
        found = lg.import_statements(src, "typescript")
        self.assertEqual([specs for _, specs in found], [["./pair"]])
        self.assertIn("beta", found[0][0])

    def test_go_imports_read_grouped_and_single_declarations(self) -> None:
        src = GO + 'import "net/http"\n'
        found = lg.import_statements(src, "go")
        self.assertEqual([specs for _, specs in found], [["fmt", "strings"], ["net/http"]])

    def test_java_has_no_import_statements(self) -> None:
        self.assertEqual(lg.import_statements(JAVA, "java"), [])


class Generator(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls._tmp = tempfile.TemporaryDirectory()
        cls.repo = repo = Path(cls._tmp.name) / "repo"
        repo.mkdir()
        git(repo, "init", "-q")
        git(repo, "config", "commit.gpgsign", "false")
        for rel, text in FILES.items():
            (repo / rel).parent.mkdir(parents=True, exist_ok=True)
            (repo / rel).write_text(text)
        git(repo, "add", "-A")
        git(repo, "commit", "-qm", "fixture")

    @classmethod
    def tearDownClass(cls) -> None:
        cls._tmp.cleanup()

    def run_gen(self, *args: str) -> subprocess.CompletedProcess[str]:
        cmd = [sys.executable, str(HERE / "gen_structural.py"), "--repo", str(self.repo), "--seed", "1", "--n", "50"]
        return subprocess.run([*cmd, *args], capture_output=True, text=True, check=False)

    def gen(self, lang: str, kind: str) -> dict[str, dict]:
        proc = self.run_gen("--lang", lang, "--kind", kind)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(proc.stdout, self.run_gen("--lang", lang, "--kind", kind).stdout, "must be deterministic")
        return {json.loads(line)["args"]["name"]: json.loads(line) for line in proc.stdout.splitlines()}

    def test_symbols_gold_is_every_definition_whose_name_contains_the_query(self) -> None:
        tasks = self.gen("typescript", "symbols")
        self.assertEqual(
            tasks["Widget"]["gold"],
            [
                f"web/widgets.ts:{line_of(TS, 'export interface Widget')}",
                f"web/widgets.ts:{line_of(TS, 'export class WidgetBox')}",
                f"web/widgets.ts:{line_of(TS, 'export function buildWidget')}",
                f"web/widgets.ts:{line_of(TS, 'export type WidgetId')}",
            ],
        )
        self.assertEqual(tasks["Widget"]["mode"], "symbols")
        self.assertEqual(tasks["Widget"]["args"], {"name": "Widget", "limit": 200})
        self.assertEqual(tasks["Widget"]["baseline"]["read"], ["web/widgets.ts"])
        self.assertNotIn("local", tasks)

    def test_java_symbols_keep_the_class_and_its_constructor_under_one_name(self) -> None:
        tasks = self.gen("java", "symbols")
        self.assertEqual(
            tasks["Greeter"]["gold"],
            [
                f"lib/Greeter.java:{line_of(JAVA, 'public class Greeter')}",
                f"lib/Greeter.java:{line_of(JAVA, 'public Greeter(')}",
            ],
        )

    def test_references_gold_is_the_unambiguous_call_sites(self) -> None:
        expected = {
            "typescript": {"computeLocal": f"web/widgets.ts:{line_of(TS, 'computeLocal(item)')}"},
            "go": {
                "Println": f"svc/main.go:{line_of(GO, 'fmt.Println')}",
                "trimAll": f"svc/main.go:{line_of(GO, 'trimAll(args)')}",
            },
            "java": {
                "nCopies": f"lib/Greeter.java:{line_of(JAVA, 'nCopies')}",
                "format": f"lib/Greeter.java:{line_of(JAVA, 'format(prefix)')}",
            },
        }
        for lang, names in expected.items():
            with self.subTest(lang=lang):
                tasks = self.gen(lang, "references")
                for name, site in names.items():
                    self.assertEqual(tasks[name]["gold"], [site])
                    self.assertEqual(tasks[name]["mode"], "references")
                self.assertNotIn("Memo", tasks)
                self.assertNotIn("Execute", tasks)
                self.assertNotIn("toString", tasks)

    def test_dependents_gold_is_every_file_whose_import_statement_names_the_module(self) -> None:
        ts = self.gen("typescript", "dependents")
        self.assertEqual(ts["./helpers.js"]["gold"], ["web/other.ts", "web/widgets.ts"])
        self.assertEqual(ts["./helpers.js"]["args"], {"name": "./helpers.js", "relation": "importers"})
        self.assertEqual(ts["./config"]["gold"], ["web/widgets.ts"])
        self.assertNotIn("./reexport", ts)
        self.assertNotIn("./required", ts)
        self.assertNotIn("./dynamic", ts)
        self.assertEqual(self.gen("go", "dependents")["strings"]["gold"], ["svc/main.go"])

    def test_java_has_no_dependents_gold(self) -> None:
        proc = self.run_gen("--lang", "java", "--kind", "dependents")
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("no `dependents` gold for java", proc.stderr)

    def test_the_kinds_a_language_has_gold_for_are_listed(self) -> None:
        listed = {lang: self.run_gen("--lang", lang, "--list-kinds").stdout.split() for lang in lg.LANGUAGES}
        self.assertEqual(
            listed,
            {
                "typescript": ["symbols", "references", "dependents"],
                "go": ["symbols", "references", "dependents"],
                "java": ["symbols", "references"],
            },
        )


if __name__ == "__main__":
    unittest.main()
