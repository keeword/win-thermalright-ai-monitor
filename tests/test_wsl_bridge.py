"""Guest protocol and native session identity tests; no real WSL or user config writes."""
import contextlib
import importlib.util
import io
import json
import os
import pathlib
import sys
import tempfile
import types
import unittest
from unittest.mock import patch


def load(name, file):
    spec = importlib.util.spec_from_file_location(name, pathlib.Path(__file__).parents[1] / "scripts" / file)
    module = importlib.util.module_from_spec(spec)
    with patch.dict(sys.modules, {"pwd": types.SimpleNamespace(getpwuid=lambda _: types.SimpleNamespace(pw_name="tester"))}):
        spec.loader.exec_module(module)
    return module


guest = load("wsl_guest", "wsl_probe.py")


class GuestTests(unittest.TestCase):
    def test_megabyte_record_advances_physical_cursor_and_keeps_recent_state(self):
        with tempfile.TemporaryDirectory() as folder:
            path = pathlib.Path(folder) / "rollout-main.jsonl"
            rows = [{"timestamp": "2026-10-08T01:00:00Z", "type": "session_meta", "payload": {"id": "large", "cwd": "/project"}},
                    {"timestamp": "2026-10-08T01:01:00Z", "type": "response_item", "payload": {"type": "function_call_output", "output": "大" * 900000}},
                    {"timestamp": "2026-10-08T01:02:00Z", "type": "event_msg", "payload": {"type": "token_count", "info": {"total_token_usage": {"input_tokens": 1234, "output_tokens": 56}}}},
                    {"timestamp": "2026-10-08T01:02:01Z", "type": "event_msg", "payload": {"type": "task_complete"}}]
            path.write_bytes(("\n".join(json.dumps(v, ensure_ascii=False) for v in rows) + "\n").encode())
            request = {"registration": "distro", "day_start": 0, "offsets": {}}
            with self.process(path):
                first = guest.probe(request)
            self.assertTrue(first["complete"])
            self.assertTrue(first["backfilling"])
            file = first["files"][0]
            self.assertGreater(file["next_offset"], 2 * 1024 * 1024)
            self.assertLess(len(file["data"].encode()), 10000)
            self.assertEqual(json.loads(file["recent_data"].splitlines()[-1])["payload"]["type"], "task_complete")
            request["offsets"][str(path)] = [file["file_id"], file["next_offset"]]
            with self.process(path):
                second = guest.probe(request)
            self.assertTrue(second["complete"])
            self.assertFalse(second["backfilling"])
            next_file = second["files"][0]
            self.assertEqual(next_file["next_offset"], path.stat().st_size)
            token = json.loads(next_file["data"].splitlines()[0])
            self.assertEqual(token["payload"]["info"]["total_token_usage"], {"input_tokens": 1234, "output_tokens": 56})

    def test_no_today_logs_means_no_historical_backfill(self):
        with tempfile.TemporaryDirectory() as folder:
            path = pathlib.Path(folder) / "history.jsonl"
            path.write_text("{}\n")
            with self.processes({}), patch.object(guest.os.path, "exists", return_value=True), \
                 patch.object(guest.os, "walk", return_value=[(folder, [], [path.name])]):
                result = guest.probe({"registration": "distro", "day_start": path.stat().st_mtime + 100})
            self.assertTrue(result["complete"])
            self.assertFalse(result["backfilling"])
            self.assertEqual(result["files"], [])

    def test_output_budget_uses_the_actual_utf8_bytes(self):
        result = {"files": [{"data": "大" * 160000}], "errors": [], "complete": True}
        encoded = guest.encode_result(result)
        self.assertLess(len(encoded), 1048576)
        self.assertEqual(json.loads(encoded), result)
        self.assertNotIn(b"\\u5927", encoded)
        oversized = {"files": [{"data": "大" * 400000}], "errors": [], "complete": True}
        truncated = json.loads(guest.encode_result(oversized))
        self.assertFalse(truncated["complete"])
        self.assertEqual(truncated["files"], [])

    @contextlib.contextmanager
    def processes(self, rows, peers=None):
        actual_open, actual_stat, actual_glob = open, os.stat, guest.glob.glob
        files = {"/proc/sys/kernel/random/boot_id": "boot-a\n"}
        links = {}
        for pid, row in rows.items():
            root = "/proc/" + str(pid)
            fields = ["S", str(row.get("parent", 1))] + ["0"] * 17 + [str(row.get("started", 123))]
            files[root + "/stat"] = str(pid) + " (codex) " + " ".join(fields)
            files[root + "/cmdline"] = ("\0".join(row.get("argv", ["codex"])) + "\0").encode()
            files[root + "/environ"] = row.get("environ", b"HOME=/root\0")
            for index, path in enumerate(row.get("fds", [])):
                links[root + "/fd/" + str(index)] = str(path)

        def opened(name, *args, **kwargs):
            if name in files:
                return io.BytesIO(files[name]) if isinstance(files[name], bytes) else io.StringIO(files[name])
            return actual_open(name, *args, **kwargs)

        def stat(name, *args, **kwargs):
            if name in ["/proc/" + str(pid) for pid in rows]:
                return types.SimpleNamespace(st_uid=0)
            return actual_stat(links.get(name, name), *args, **kwargs)

        def glob(pattern):
            if pattern == "/proc/[0-9]*":
                return ["/proc/" + str(pid) for pid in rows]
            if pattern.endswith("/fd/*"):
                return [fd for fd in links if fd.startswith(pattern[:-1])]
            if pattern.endswith(".jsonl"):
                return actual_glob(pattern)
            return []

        with patch("builtins.open", side_effect=opened), patch.object(guest.os, "stat", side_effect=stat), \
             patch.object(guest.os, "getuid", return_value=0, create=True), \
             patch.object(guest.os, "readlink", side_effect=links.__getitem__), \
             patch.object(guest.os.path, "exists", return_value=False), \
             patch.object(guest.glob, "glob", side_effect=glob), \
             patch.object(guest, "unix_peers", side_effect=peers if isinstance(peers, Exception) else None,
                          return_value=peers or {}):
            yield

    def test_shared_server_excludes_auxiliary_updater_and_connected_frontends(self):
        with tempfile.TemporaryDirectory() as folder:
            paths = []
            for sid, source in [("a", "vscode"), ("b", "vscode"), ("aux", {"subagent": {}})]:
                path = pathlib.Path(folder) / ("rollout-" + sid + ".jsonl")
                path.write_text(json.dumps({"type": "session_meta", "payload": {"id": sid, "source": source}}) + "\n")
                paths.append(path)
            rows = {100: {"argv": ["codex", "app-server", "--managed-daemon"], "fds": paths + ["socket:[400]"]},
                    101: {"fds": ["socket:[300]"]},
                    102: {"fds": ["socket:[900]"]},
                    103: {"argv": ["codex", "app-server", "daemon", "pid-update-loop"]}}
            with self.processes(rows, peers={300: 400, 400: 300}):
                result = guest.probe({"registration": "distro", "day_start": 0})
            self.assertTrue(result["complete"])
            self.assertEqual([i["instance_key"]["pid"] for i in result["instances"]], [100, 102])
            self.assertEqual([s["native_session_id"] for s in result["instances"][0]["shared_session_keys"]], ["a", "b"])
            self.assertEqual(result["instances"][0]["error"], None)
            self.assertIsNone(result["instances"][1]["session_key"])
            self.assertEqual({f["path"] for f in result["files"]}, {str(p) for p in paths})

            with self.processes(rows, peers=PermissionError("socket denied")):
                denied = guest.probe({"registration": "distro", "day_start": 0})
            self.assertFalse(denied["complete"])
            self.assertEqual([i["instance_key"]["pid"] for i in denied["instances"]], [100, 101, 102])
            self.assertIn("socket denied", denied["errors"][0])

            # Ordinary CLI holding several logs remains ambiguous.
            with self.processes({100: {"fds": paths}}):
                ambiguous = guest.probe({"registration": "distro", "day_start": 0})
            self.assertIsNone(ambiguous["instances"][0]["session_key"])
            self.assertEqual(ambiguous["instances"][0]["shared_session_keys"], [])

    def test_reused_pid_cannot_associate_the_previous_process_rollout(self):
        with tempfile.TemporaryDirectory() as folder:
            path = pathlib.Path(folder) / "rollout-main.jsonl"
            path.write_text(json.dumps({"type": "session_meta", "payload": {"id": "old"}}) + "\n")
            with self.processes({100: {"fds": [path]}}), patch.object(guest, "probe", wraps=guest.probe):
                # Identity is checked again after independently reading metadata.
                # The stat fixture reports 123, but the final check sees 124.
                actual_open = open
                def final_stat(name, *args, **kwargs):
                    if name == "/proc/100/stat":
                        return io.StringIO("100 (codex) S 1 " + "0 " * 17 + "124")
                    return actual_open(name, *args, **kwargs)
                calls = 0
                def opened(name, *args, **kwargs):
                    nonlocal calls
                    if name == "/proc/100/stat":
                        calls += 1
                        if calls > 1:
                            return final_stat(name, *args, **kwargs)
                    return actual_open(name, *args, **kwargs)
                with patch("builtins.open", side_effect=opened):
                    result = guest.probe({"registration": "distro", "day_start": 0})
            self.assertFalse(result["complete"])
            self.assertEqual(result["instances"], [])
            self.assertIn("process identity changed", result["errors"][0])

    @contextlib.contextmanager
    def process(self, path, denied=False):
        actual_open = open
        actual_stat = os.stat
        fields = ["S", "1"] + ["0"] * 17 + ["123"]
        files = {"/proc/sys/kernel/random/boot_id": "boot-a\n",
                 "/proc/100/stat": "100 (codex) " + " ".join(fields),
                 "/proc/100/cmdline": b"/usr/bin/codex\0",
                 "/proc/100/environ": b"CODEX_HOME=/custom/.codex\0"}

        def opened(name, *args, **kwargs):
            if name in files:
                return io.BytesIO(files[name]) if isinstance(files[name], bytes) else io.StringIO(files[name])
            return actual_open(name, *args, **kwargs)

        def stat(name, *args, **kwargs):
            if name == "/proc/100":
                return types.SimpleNamespace(st_uid=0)
            if name == "/proc/100/fd/7":
                return actual_stat(path)
            return actual_stat(name, *args, **kwargs)

        def glob(pattern):
            if pattern == "/proc/[0-9]*":
                return ["/proc/100"]
            if pattern == "/proc/100/fd/*":
                if denied:
                    raise PermissionError("fd permission denied")
                return ["/proc/100/fd/7"]
            return []

        with patch("builtins.open", side_effect=opened), patch.object(guest.os, "stat", side_effect=stat), \
             patch.object(guest.os, "getuid", return_value=0, create=True), \
             patch.object(guest.os, "readlink", return_value=str(path)), \
             patch.object(guest.os.path, "exists", return_value=False), \
             patch.object(guest.glob, "glob", side_effect=glob):
            yield

    def test_process_identity_metadata_and_incremental_large_line(self):
        with tempfile.TemporaryDirectory() as folder:
            path = pathlib.Path(folder) / "rollout-main.jsonl"
            first = json.dumps({"type": "session_meta", "payload": {"id": "real-session", "source": "cli", "cwd": "/same-project"}}) + "\n"
            path.write_bytes((first + json.dumps({"type": "event_msg", "payload": {"type": "agent_message", "message": "a" * 70000}}) + "\n").encode("utf-8"))
            request = {"registration": "stable-distro", "day_start": 0, "offsets": {}}
            with self.process(path):
                result = guest.probe(request)
            self.assertTrue(result["complete"])
            self.assertFalse(result["backfilling"])
            instance = result["instances"][0]
            self.assertEqual(instance["instance_key"]["process_started_at"], 123)
            self.assertEqual(instance["instance_key"]["boot_id"], "boot-a")
            self.assertEqual(instance["session_key"]["origin_id"], "wsl:stable-distro:0")
            self.assertEqual(instance["session_key"]["native_session_id"], "real-session")
            file = result["files"][0]
            self.assertEqual(len(file["data"].splitlines()), 2)
            self.assertEqual(json.loads(file["data"].splitlines()[0])["payload"]["id"], "real-session")
            self.assertEqual(len(json.loads(file["data"].splitlines()[1])["payload"]["message"]), 1600)
            self.assertEqual(file["next_offset"], path.stat().st_size)
            request["offsets"][str(path)] = [file["file_id"], file["next_offset"]]
            with self.process(path):
                second = guest.probe(request)
            self.assertFalse(second["backfilling"])
            self.assertEqual(second["files"][0]["offset"], path.stat().st_size)
            self.assertEqual(second["files"][0]["data"], "")

    def test_fd_permission_failure_is_partial_and_cannot_borrow_history(self):
        with tempfile.TemporaryDirectory() as folder:
            path = pathlib.Path(folder) / "rollout-main.jsonl"
            path.write_text("{}\n")
            with self.process(path, denied=True):
                result = guest.probe({"registration": "distro", "day_start": 0, "offsets": {}})
            self.assertFalse(result["complete"])
            self.assertIsNone(result["instances"][0]["session_key"])
            self.assertIn("permission denied", result["errors"][0])


    def claude_fixture(self, root, pid=100, sid="claude-a", started=123, status="busy", log=True):
        (root / "sessions").mkdir(parents=True, exist_ok=True)
        record = root / "sessions" / (str(pid) + ".json")
        record.write_text(json.dumps({"pid": pid, "sessionId": sid, "procStart": str(started),
                                      "cwd": "/same-project", "status": status, "updatedAt": 1}))
        if log:
            project = root / "projects" / "same-project"
            project.mkdir(parents=True, exist_ok=True)
            (project / (sid + ".jsonl")).write_text(json.dumps({"sessionId": sid, "type": "assistant",
                    "message": {"id": sid, "role": "assistant", "stop_reason": "end_turn", "content": []}}) + "\n")
        return record

    def test_native_claude_preserves_two_sessions_in_one_directory_without_hooks(self):
        with tempfile.TemporaryDirectory() as folder:
            root = pathlib.Path(folder) / "custom-config"
            self.claude_fixture(root)
            self.claude_fixture(root, pid=200, sid="claude-b", started=456, status="idle")
            env = ("CLAUDE_CONFIG_DIR=" + str(root) + "\0").encode()
            rows = {100: {"argv": ["claude"], "environ": env},
                    200: {"argv": ["claude"], "started": 456, "environ": env}}
            with self.processes(rows):
                result = guest.probe({"registration": "distro", "day_start": time_future(), "offsets": {}})
            self.assertTrue(result["complete"], result["errors"])
            instances = {i["session_key"]["native_session_id"]: i for i in result["instances"]}
            self.assertEqual(set(instances), {"claude-a", "claude-b"})
            self.assertEqual(instances["claude-a"]["native_work_state"], "Working")
            self.assertEqual(instances["claude-b"]["native_work_state"], "Idle")
            self.assertEqual(len(result["files"]), 2)
            self.assertTrue(all(f["recent_data"] for f in result["files"]))
            self.assertNotIn("hooks", result)

    def test_native_claude_rejects_pid_reuse_wrong_pid_and_path_traversal(self):
        with tempfile.TemporaryDirectory() as folder:
            root = pathlib.Path(folder)
            env = ("CLAUDE_CONFIG_DIR=" + str(root) + "\0").encode()
            for invalid in ({"procStart": "999"}, {"pid": 999}, {"sessionId": "../outside"}):
                with self.subTest(invalid=invalid):
                    record = self.claude_fixture(root)
                    data = json.loads(record.read_text())
                    data.update(invalid)
                    record.write_text(json.dumps(data))
                    with self.processes({100: {"argv": ["claude"], "environ": env}}):
                        result = guest.probe({"registration": "distro", "day_start": time_future(), "offsets": {}})
                    self.assertFalse(result["complete"])
                    self.assertIsNone(result["instances"][0]["session_key"])
                    self.assertEqual(result["files"], [])

    def test_native_claude_without_transcript_can_identify_a_fresh_session(self):
        with tempfile.TemporaryDirectory() as folder:
            root = pathlib.Path(folder)
            self.claude_fixture(root, status="future-status", log=False)
            env = ("CLAUDE_CONFIG_DIR=" + str(root) + "\0").encode()
            with self.processes({100: {"argv": ["claude"], "environ": env}}):
                result = guest.probe({"registration": "distro", "day_start": time_future(), "offsets": {}})
            self.assertTrue(result["complete"])
            self.assertEqual(result["instances"][0]["session_key"]["native_session_id"], "claude-a")
            self.assertIsNone(result["instances"][0]["native_work_state"])

    def test_native_claude_does_not_borrow_history_or_a_mismatched_transcript(self):
        with tempfile.TemporaryDirectory() as folder:
            root = pathlib.Path(folder)
            record = self.claude_fixture(root)
            transcript = root / "projects/same-project/claude-a.jsonl"
            transcript.write_text('{"sessionId":"other"}\n')
            env = ("CLAUDE_CONFIG_DIR=" + str(root) + "\0").encode()
            with self.processes({100: {"argv": ["claude"], "environ": env}}):
                mismatch = guest.probe({"registration": "distro", "day_start": time_future(), "offsets": {}})
                record.unlink()
                missing = guest.probe({"registration": "distro", "day_start": time_future(), "offsets": {}})
            self.assertFalse(mismatch["complete"])
            self.assertIsNone(mismatch["instances"][0]["session_key"])
            self.assertTrue(missing["complete"])
            self.assertIsNone(missing["instances"][0]["session_key"])
            self.assertEqual(missing["files"], [])

    def test_native_claude_rechecks_session_identity_after_reading_transcript(self):
        with tempfile.TemporaryDirectory() as folder:
            root = pathlib.Path(folder)
            self.claude_fixture(root)
            records = [{"pid": 100, "procStart": "123", "sessionId": sid, "status": "busy"}
                       for sid in ("claude-a", "claude-b")]
            with patch.object(guest, "claude_registration", side_effect=records):
                with self.assertRaisesRegex(ValueError, "changed during discovery"):
                    guest.claude_session(str(root), 100, 123)

    def test_native_claude_can_find_identity_after_a_large_initial_record(self):
        with tempfile.TemporaryDirectory() as folder:
            root = pathlib.Path(folder)
            self.claude_fixture(root)
            transcript = root / "projects/same-project/claude-a.jsonl"
            transcript.write_bytes(b"x" * (2 * 1024 * 1024) + b'\n{"sessionId":"claude-a"}\n')
            self.assertTrue(guest.claude_transcript_matches(str(transcript), "claude-a"))

    def test_native_claude_rejects_oversized_and_incomplete_registration(self):
        with tempfile.TemporaryDirectory() as folder:
            root = pathlib.Path(folder)
            record = self.claude_fixture(root)
            for data in (b" " * 65537, b'{"pid":100'):
                with self.subTest(size=len(data)):
                    record.write_bytes(data)
                    with patch.object(guest.os, "getuid", return_value=os.stat(record).st_uid, create=True):
                        with self.assertRaises(ValueError):
                            guest.claude_session(str(root), 100, 123)


def time_future():
    return 10 ** 12


if __name__ == "__main__":
    unittest.main()
