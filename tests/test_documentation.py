#!/usr/bin/env python3
"""Offline documentation contract checks; no profile/state/server is opened.

Usage: python3 tests/test_documentation.py target/debug/rdrive
Only --help and --version are executed. Checks the generated reference against
the executable, independently compares its command tree with source enums, and
checks that the credential-free JSON example covers all configuration fields.
"""

import importlib.util
import contextlib
import io
import json
import os
from pathlib import Path
import re
import runpy
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get("RDRIVE_TEST_BINARY", ROOT / "target/debug/rdrive")).resolve()
if __name__ == "__main__" and len(sys.argv) > 1 and not sys.argv[1].startswith("-"):
    BINARY = Path(sys.argv.pop(1)).resolve()
spec = importlib.util.spec_from_file_location("cli_reference", ROOT / "scripts/generate-cli-docs.py")
cli_reference = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cli_reference)


def source_text(path):
    """Remove comments/literals so braces inside help prose are not syntax."""
    return re.sub(
        r'//[^\n]*|/\*[\s\S]*?\*/|"(?:\\.|[^"\\])*"',
        lambda match: '""' if match.group(0).startswith('"') else "",
        path.read_text(encoding="utf-8"),
    )


def balanced_body(text, opening):
    depth = 1
    for index in range(opening + 1, len(text)):
        depth += (text[index] == "{") - (text[index] == "}")
        if not depth:
            return text[opening + 1:index]
    raise AssertionError("unbalanced Rust declaration")


def rust_command_paths(source=None):
    source = source_text(ROOT / "src/main.rs") if source is None else source
    enums = {}
    for match in re.finditer(r"\benum\s+(\w*Command)\s*\{", source):
        body = balanced_body(source, match.end() - 1)
        fragments = []
        start = depth = 0
        for index, character in enumerate(body):
            depth += character in "{(["
            depth -= character in "})]"
            if character == "," and not depth:
                fragments.append(body[start:index].strip())
                start = index + 1
        fragments.append(body[start:].strip())
        variants = []
        for fragment in filter(None, fragments):
            # Attributes belong to the declaration, but are not the variant
            # identifier. A hidden internal helper is deliberately absent from
            # public --help; validate its declaration before excluding it.
            declaration = fragment
            attributes = []
            while declaration.startswith("#["):
                depth, closing = 1, None
                for index in range(2, len(declaration)):
                    depth += (declaration[index] == "[") - (declaration[index] == "]")
                    if depth == 0:
                        closing = index + 1
                        break
                if closing is None:
                    raise AssertionError(f"unrecognized subcommand attribute: {fragment}")
                attributes.append(declaration[:closing])
                declaration = declaration[closing:].lstrip()
            variant = re.match(r"(\w+)\b", declaration)
            if not variant:
                raise AssertionError(f"unrecognized subcommand declaration: {fragment}")
            if any(re.match(r"#\[\s*command\s*\(", attribute)
                   and re.search(r"\bhide\s*=\s*true\b", attribute)
                   for attribute in attributes):
                continue
            name = re.sub(r"(?<!^)(?=[A-Z])", "-", variant.group(1)).lower()
            nested = re.search(r"\bcommand\s*:\s*(?:Option\s*<\s*)?(\w+Command)\b", declaration)
            variants.append((name, nested.group(1) if nested else None))
        enums[match.group(1)] = variants
    result = {()}

    def visit(enum_name, path):
        for name, nested in enums[enum_name]:
            child = (*path, name)
            result.add(child)
            if nested:
                visit(nested, child)

    visit("Command", ())
    return result


def rust_fields(path, name):
    source = source_text(ROOT / path)
    declaration = re.search(rf"\bpub\s+struct\s+{name}\s*\{{", source)
    if not declaration:
        raise AssertionError(f"missing Rust struct {name}")
    return set(re.findall(r"\bpub\s+(\w+)\s*:", balanced_body(source, declaration.end() - 1)))


class DocumentationContract(unittest.TestCase):
    def test_public_documentation_links_exist_without_local_build_artifacts(self):
        documents = [ROOT / name for name in ["README.md", "CONTRIBUTING.md", "SECURITY.md", "CHANGELOG.md", "docs/README.md", "docs/INSTALLATION.md", "docs/QUICKSTART.md", "docs/FEATURES.md", "docs/ARCHITECTURE.md", "docs/TROUBLESHOOTING.md", "docs/TESTING.md", "docs/COVERAGE.md", "docs/REPOSITORY.md", "docs/HISTORY.md"]]
        documents += sorted((ROOT / "docs/reference").glob("*.md"))
        for document in documents:
            for destination in re.findall(r'\[[^\]\n]*\]\(([^)]+)\)', document.read_text()):
                destination = destination.strip("<>").split("#", 1)[0]
                if not destination or "://" in destination or destination.startswith("mailto:"):
                    continue
                target = (document.parent / destination).resolve()
                with self.subTest(document=str(document.relative_to(ROOT)), destination=destination):
                    self.assertTrue(target.exists(), destination)
                    relative = target.relative_to(ROOT)
                    self.assertNotIn(relative.parts[0], ["target", "dist", ".runtime"])
                    self.assertFalse(str(relative).startswith("macos/build/"))
                    self.assertFalse(str(relative).endswith(".debug.json"))

    @classmethod
    def setUpClass(cls):
        if not BINARY.is_file():
            raise RuntimeError(f"build rdrive first or pass its path: {BINARY}")
        cls.records = cli_reference.collect(BINARY)
        cls.configuration = (ROOT / "docs/reference/CONFIGURATION.md").read_text(encoding="utf-8")

    def test_every_source_subcommand_has_generated_help(self):
        self.assertEqual(rust_command_paths(), {tuple(path) for path, _ in self.records})

    def test_rust_source_parser_preserves_public_attributes_and_excludes_hidden_helpers(self):
        source = """
            enum Command {
                #[allow(dead_code)] #[command(about="")]
                PublicRead,
                #[command(hide=false)] Visible { command:VisibleCommand },
                #[command(name="", hide = true)]
                StatusProbe { profile:String, command:PrivateCommand },
            }
            enum VisibleCommand { #[command(about="")] Child, }
            enum PrivateCommand { InternalOnly, }
        """
        self.assertEqual(rust_command_paths(source), {
            (), ("public-read",), ("visible",), ("visible", "child"),
        })
        for declaration in ["???", "#[command(hide=true)] ???"]:
            with self.subTest(declaration=declaration):
                with self.assertRaisesRegex(AssertionError, "unrecognized subcommand declaration"):
                    rust_command_paths("enum Command { " + declaration + ", }")

    def test_reference_exactly_matches_the_executable(self):
        actual = (ROOT / "docs/reference/CLI.md").read_text(encoding="utf-8")
        self.assertEqual(actual, cli_reference.render(BINARY), "regenerate CLI.md with scripts/generate-cli-docs.py")
        self.assertEqual(actual, actual.rstrip() + "\n")
        self.assertTrue(all(line == line.rstrip() for line in actual.splitlines()))

    def test_json_example_covers_actual_config_fields(self):
        blocks = re.findall(r"```json\n(.*?)\n```", self.configuration, re.DOTALL)
        self.assertEqual(len(blocks), 1)
        example = json.loads(blocks[0])
        declarations = [
            ("src/config.rs", "Config", example),
            ("src/config.rs", "CachePolicy", example["cache"]),
            ("src/config.rs", "IndexPolicy", example["index"]),
            ("src/filtering.rs", "FilterConfig", example["filters"]),
            ("src/filtering.rs", "ProfileFilter", example["filters"]["profiles"]["dav"]),
            ("src/policies.rs", "PolicyConfig", example["policies"]),
            ("src/policies.rs", "VersionPolicy", example["policies"]["versions"]),
            ("src/policies.rs", "ProfilePolicy", example["policies"]["profiles"]["dav"]),
            ("src/history.rs", "HistoryConfig", example["history"]),
            ("src/history.rs", "HistoryPolicy", example["history"]["default"]),
            ("src/history.rs", "HistoryPolicy", example["history"]["profiles"]["dav"]),
            ("src/checkpoints.rs", "Policy", example["checkpoints"]),
        ]
        declarations.extend(("src/config.rs", "Profile", value) for value in example["profiles"].values())
        for path, name, value in declarations:
            with self.subTest(struct=name):
                fields = rust_fields(path, name)
                self.assertEqual(set(value), fields, f"JSON example must cover every {name} field")
                for field in fields:
                    self.assertIn(f"`{field}`", self.configuration, f"{name}.{field} needs a prose reference")
        for profile in example["profiles"].values():
            self.assertIsNone(profile["username"])
            self.assertIsNone(profile["password_env"])
            self.assertIn("example.invalid", profile["url"])
            self.assertNotIn("@", profile["url"])
        self.assertNotIn('"password"', blocks[0])
        self.assertNotIn('"token"', blocks[0])

    def test_native_controls_have_a_parameter_reference(self):
        native = (ROOT / "macos/Companion/main.swift").read_text(encoding="utf-8")
        reference = (ROOT / "docs/reference/CLI.md").read_text(encoding="utf-8")
        options = set(re.findall(r'"(--[a-z-]+)"', native))
        self.assertGreater(len(options), 15)
        for option in options:
            with self.subTest(option=option):
                self.assertIn(option, reference)

    def test_real_generator_entrypoint(self):
        result = subprocess.run(
            [sys.executable, str(ROOT / "scripts/generate-cli-docs.py"), "--binary", str(BINARY), "--check"],
            text=True, capture_output=True, timeout=30,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("CLI reference matches", result.stdout)

    def test_main_guard_uses_the_same_contract(self):
        arguments = ["generate-cli-docs.py", "--binary", str(BINARY), "--check"]
        with mock.patch.object(sys, "argv", arguments), contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaises(SystemExit) as result:
                runpy.run_path(str(ROOT / "scripts/generate-cli-docs.py"), run_name="__main__")
        self.assertEqual(result.exception.code, 0)


class GeneratorFailureContract(unittest.TestCase):
    def test_complete_mcp_schemas_are_generated_checked_and_bound_to_both_catalogs(self):
        with tempfile.TemporaryDirectory(prefix="rdrive-mcp-docs-") as directory:
            output = Path(directory) / "MCP.md"
            self.assertEqual(self.invoke_main("--binary", str(BINARY), "--mcp", "--output", str(output))[0], 0)
            text = output.read_text()
            tools = json.loads(cli_reference.invoke(BINARY, ["--json", "mcp", "tools", "--allow-write"]))["tools"]
            blocks = re.findall(r'```json\n(.*?)\n```', text, re.DOTALL)
            self.assertEqual([json.loads(block) for block in blocks], [tool["inputSchema"] for tool in tools])
            self.assertEqual(re.findall(r'^## `([^`]+)`', text, re.MULTILINE), [tool["name"] for tool in tools])
            self.assertEqual(self.invoke_main("--binary", str(BINARY), "--mcp", "--output", str(output), "--check")[0], 0)
            output.write_text("stale catalog")
            self.assertEqual(self.invoke_main("--binary", str(BINARY), "--mcp", "--output", str(output), "--check")[0], 1)
            self.assertEqual(self.invoke_main("--binary", str(BINARY), "--mcp", "--check")[0], 0)

    def test_malformed_duplicate_or_inconsistent_mcp_catalogs_fail(self):
        tool = {"name": "test", "description": "test fixture", "annotations": {"readOnlyHint": True}, "inputSchema": {"type": "object"}}
        for all_tools, readonly in [("invalid", '{"tools":[]}'), ('{}', '{"tools":[]}'), ('{"tools":[]}', '{"tools":[]}'), (json.dumps({"tools": [tool, tool]}), json.dumps({"tools": [tool]})), (json.dumps({"tools": [tool]}), '{"tools":[]}')]:
            with self.subTest(all_tools=all_tools), mock.patch.object(cli_reference, "invoke", side_effect=["rdrive fixture", all_tools, readonly]):
                with self.assertRaises(SystemExit) as failure:
                    self.invoke_main("--binary", str(BINARY), "--mcp")
                self.assertEqual(failure.exception.code, 2)

    def invoke_main(self, *arguments):
        output, error = io.StringIO(), io.StringIO()
        with mock.patch.object(sys, "argv", ["generate-cli-docs.py", *arguments]), contextlib.redirect_stdout(output), contextlib.redirect_stderr(error):
            result = cli_reference.main()
        return result, output.getvalue(), error.getvalue()

    def test_generation_check_and_drift_are_reproducible(self):
        with tempfile.TemporaryDirectory(prefix="rdrive-docs-") as directory:
            destination = Path(directory) / "nested" / "CLI.md"
            arguments = ("--binary", str(BINARY), "--output", str(destination))
            result, output, error = self.invoke_main(*arguments)
            self.assertEqual(result, 0)
            self.assertIn("Generated", output)
            self.assertEqual(error, "")
            result, output, error = self.invoke_main(*arguments, "--check")
            self.assertEqual(result, 0)
            self.assertIn("CLI reference matches", output)
            self.assertEqual(error, "")
            destination.write_text("stale reference\n", encoding="utf-8")
            result, _, error = self.invoke_main(*arguments, "--check")
            self.assertEqual(result, 1)
            self.assertIn("out of date", error)
            self.assertEqual(destination.read_text(encoding="utf-8"), "stale reference\n")
            destination.unlink()
            result, _, error = self.invoke_main(*arguments, "--check")
            self.assertEqual(result, 1)
            self.assertFalse(destination.exists())

    def test_missing_binary_and_unknown_generator_option_fail(self):
        with tempfile.TemporaryDirectory(prefix="rdrive-docs-") as directory:
            for arguments in [("--binary", str(Path(directory) / "missing")), ("--unknown",)]:
                with self.subTest(arguments=arguments):
                    with self.assertRaises(SystemExit) as failure:
                        self.invoke_main(*arguments)
                    self.assertEqual(failure.exception.code, 2)
        with self.assertRaises(SystemExit) as help_exit:
            self.invoke_main("--help")
        self.assertEqual(help_exit.exception.code, 0)

    def test_unexpected_help_results_fail_without_a_document(self):
        with tempfile.TemporaryDirectory(prefix="rdrive-docs-") as directory:
            binary = Path(directory) / "fake-rdrive"
            output = Path(directory) / "CLI.md"
            cases = [
                "import sys; sys.stderr.write('unknown help\\n'); sys.exit(2)",
                "import sys; sys.stderr.write('unexpected warning\\n')",
                "print('unrecognized help format')",
            ]
            for body in cases:
                with self.subTest(body=body):
                    binary.write_text(f"#!{sys.executable}\n{body}\n", encoding="utf-8")
                    binary.chmod(0o700)
                    with self.assertRaises(SystemExit) as failure:
                        self.invoke_main("--binary", str(binary), "--output", str(output))
                    self.assertEqual(failure.exception.code, 2)
                    self.assertFalse(output.exists())

    def test_unreadable_and_timed_out_binary_fail(self):
        for failure in [OSError("cannot execute fixture"), subprocess.TimeoutExpired("rdrive --help", 20)]:
            with self.subTest(failure=type(failure).__name__):
                with mock.patch.object(cli_reference.subprocess, "run", side_effect=failure):
                    with self.assertRaises(SystemExit) as exit_status:
                        self.invoke_main("--binary", str(BINARY))
                    self.assertEqual(exit_status.exception.code, 2)


if __name__ == "__main__":
    unittest.main()
