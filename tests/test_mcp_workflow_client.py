"""Keep unexpected MCP fixture failures diagnosable without exposing credentials."""
import queue
import subprocess
from types import SimpleNamespace
import unittest
from unittest import mock

from mcp_workflows import Client, PASSWORD, sample_stalled_process


class McpWorkflowClientTests(unittest.TestCase):
    def test_timeout_reports_process_and_reader_state_without_fixture_password(self):
        client = Client.__new__(Client)
        client.lines = queue.Queue()
        client.stderr = bytearray(("diagnostic: " + PASSWORD).encode())
        client.process = SimpleNamespace(pid=123, poll=lambda: None)
        client.readers = [SimpleNamespace(is_alive=lambda: True), SimpleNamespace(is_alive=lambda: False)]
        with mock.patch("mcp_workflows.sample_stalled_process", return_value="\nchild stack: " + PASSWORD), \
                self.assertRaises(AssertionError) as caught:
            client.receive(timeout=0)
        text = str(caught.exception)
        self.assertIn("pid=123", text)
        self.assertIn("exit=None", text)
        self.assertIn("readers=[True, False]", text)
        self.assertIn("diagnostic: <redacted>", text)
        self.assertIn("child stack: <redacted>", text)
        self.assertNotIn(PASSWORD, text)

    def test_only_live_macos_child_is_sampled_with_a_bound(self):
        process = SimpleNamespace(pid=123, poll=lambda: None)
        with mock.patch("mcp_workflows.sys.platform", "darwin"), \
                mock.patch("mcp_workflows.subprocess.run", return_value=SimpleNamespace(stdout="Call graph: fixture")) as run:
            self.assertIn("Call graph: fixture", sample_stalled_process(process))
            self.assertEqual(run.call_args.args[0], ["/usr/bin/sample", "123", "1", "1", "-mayDie"])
            self.assertEqual(run.call_args.kwargs["timeout"], 15)
        for platform, exit_code in [("linux", None), ("darwin", 0)]:
            with mock.patch("mcp_workflows.sys.platform", platform), \
                    mock.patch("mcp_workflows.subprocess.run") as run:
                self.assertEqual(sample_stalled_process(SimpleNamespace(poll=lambda: exit_code)), "")
                run.assert_not_called()

    def test_sampler_failure_preserves_original_timeout_diagnostic(self):
        process = SimpleNamespace(pid=123, poll=lambda: None)
        for error in [OSError("fixture"), subprocess.TimeoutExpired("sample", 15)]:
            with mock.patch("mcp_workflows.sys.platform", "darwin"), \
                    mock.patch("mcp_workflows.subprocess.run", side_effect=error):
                self.assertIn(type(error).__name__, sample_stalled_process(process))

    def test_successful_protocol_frame_still_passes_through(self):
        client = Client.__new__(Client)
        client.lines = queue.Queue()
        client.lines.put(b'{"jsonrpc":"2.0","id":1,"result":{}}\n')
        client.responses = []
        reply = client.receive(timeout=0)
        self.assertEqual(reply, {"jsonrpc": "2.0", "id": 1, "result": {}})
        self.assertEqual(client.responses, [reply])
