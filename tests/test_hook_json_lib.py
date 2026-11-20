"""Exercise json-lib.sh, the hooks' JSON reader and writer, with no jq on PATH."""

import json
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LIBRARY = ROOT / "plugin/.ai-rulez/hooks/json-lib.sh"


class HookJsonLibraryTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.bin = Path(temporary.name)
        (self.bin / "awk").symlink_to(shutil.which("awk"))

    def shell(self, script: str, stdin: str = "", *args: str) -> str:
        return subprocess.run(
            ["/bin/bash", "-c", f'. "{LIBRARY}"; {script}', "json-lib", *args],
            input=stdin,
            env={**os.environ, "PATH": str(self.bin)},
            capture_output=True,
            text=True,
            timeout=15,
            check=True,
        ).stdout

    def get(self, document: str, path: str, kind: str = "") -> str:
        return self.shell('json_get "$(json_fields)" "$1" "$2"', document, path, kind)

    def length(self, document: str, path: str) -> str:
        return self.shell('printf %s "$(json_length "$(json_fields)" "$1")"', document, path)

    def test_values_are_decoded_at_their_path(self) -> None:
        document = json.dumps(
            {
                "tool_input": {"command": 'cd x\ngrep "a\\\\b"\ttab', "description": 'say ,"command":"no"'},
                "unicode": "café \U0001f600 bell \u0007 slash /",
                "list": [1, -2.5e3, True, None, {"deep": "yes"}],
                "nul": "a\u0000b",
            }
        )
        cases = (
            ("tool_input.command", "", 'cd x\ngrep "a\\\\b"\ttab'),
            ("tool_input.description", "string", 'say ,"command":"no"'),
            ("unicode", "", "café \U0001f600 bell \u0007 slash /"),
            ("list.1", "number", "-2500.0"),
            ("list.2", "boolean", "true"),
            ("list.3", "null", ""),
            ("list.4.deep", "", "yes"),
            ("nul", "", "ab"),
            ("tool_input.command", "number", ""),
            ("missing", "", ""),
        )
        for path, kind, expected in cases:
            with self.subTest(path=path, kind=kind):
                self.assertEqual(self.get(document, path, kind), expected)

    def test_a_dot_inside_a_key_is_not_a_path_separator(self) -> None:
        document = '{"statusLine.command":"echo ready","statusLine":{"command":"basemind statusline"}}'
        self.assertEqual(self.get(document, "statusLine.command"), "basemind statusline")
        self.assertEqual(self.shell("json_fields", document).splitlines()[1], "statusLine\\.command\tstring\techo ready")

    def test_a_pretty_document_reads_like_a_compact_one(self) -> None:
        document = {"messages": [{"id": "t-1"}, {"id": "t-2"}], "more": False}
        for text in (json.dumps(document), json.dumps(document, indent=2)):
            with self.subTest(text=text):
                self.assertEqual(self.get(text, "messages.1.id"), "t-2")
                self.assertEqual(self.length(text, "messages"), "2")

    def test_length_counts_only_an_arrays_direct_members(self) -> None:
        cases = {
            '{"messages":[]}': "0",
            '{"messages":[{"id":"a","tags":[1,2,3]},{"id":"b"}]}': "2",
            '{"messages":{"0":"x"}}': "",
            '{"other":[1]}': "",
        }
        for text, expected in cases.items():
            with self.subTest(text=text):
                self.assertEqual(self.length(text, "messages"), expected)

    def test_a_document_that_does_not_parse_lists_nothing(self) -> None:
        raw_controls = tuple(f'{{"a":"raw{chr(code)}byte"}}' for code in range(1, 32))
        malformed = ("", "not JSON", '{"a":1', '{"a":1} trailing', '{"a":"\\x"}', "[1,]")
        for text in malformed + raw_controls:
            with self.subTest(text=text):
                self.assertEqual(self.shell("json_fields", text), "")

    def test_escape_round_trips_every_control_character(self) -> None:
        text = "".join(chr(code) for code in range(1, 32)) + ' quote " backslash \\ café'
        escaped = self.shell('json_escape "$1"', "", text)
        self.assertEqual(json.loads(f'"{escaped}"'), text)


if __name__ == "__main__":
    unittest.main()
