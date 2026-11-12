"""Keep unexpected MCP fixture failures diagnosable without exposing credentials."""
import queue
from types import SimpleNamespace
import unittest

from mcp_workflows import Client, PASSWORD


class McpWorkflowClientTests(unittest.TestCase):
    def test_timeout_reports_process_and_reader_state_without_fixture_password(self):
        client = Client.__new__(Client)
        client.lines = queue.Queue()
        client.stderr = bytearray(("diagnostic: " + PASSWORD).encode())
        client.process = SimpleNamespace(pid=123, poll=lambda: None)
        client.readers = [SimpleNamespace(is_alive=lambda: True), SimpleNamespace(is_alive=lambda: False)]
        with self.assertRaises(AssertionError) as caught:
            client.receive(timeout=0)
        text = str(caught.exception)
        self.assertIn("pid=123", text)
        self.assertIn("exit=None", text)
        self.assertIn("readers=[True, False]", text)
        self.assertIn("diagnostic: <redacted>", text)
        self.assertNotIn(PASSWORD, text)

    def test_successful_protocol_frame_still_passes_through(self):
        client = Client.__new__(Client)
        client.lines = queue.Queue()
        client.lines.put(b'{"jsonrpc":"2.0","id":1,"result":{}}\n')
        client.responses = []
        reply = client.receive(timeout=0)
        self.assertEqual(reply, {"jsonrpc": "2.0", "id": 1, "result": {}})
        self.assertEqual(client.responses, [reply])
