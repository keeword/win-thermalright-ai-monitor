"""Read-only, default-user WSL2 probe. All paths and PIDs are Linux identities.

Requires Python 3. Runs under a guest alarm as well as a host deadline. Receives
JSON on stdin and emits one bounded UTF-8 JSON document; never invokes a shell.
"""
import glob
import json
import os
import pwd
import signal
import socket
import struct
import sys
import time

MAX_RECORD = 16 * 1024 * 1024


def compact_record(value):
    """Bound display text without changing event identities or numeric usage."""
    def compact(item, text_limit, array_limit):
        if isinstance(item, str):
            return item[:text_limit]
        if isinstance(item, list):
            return [compact(v, text_limit, array_limit) for v in item[:array_limit]]
        if isinstance(item, dict):
            return {k: compact(v, 65536 if k == "arguments" else text_limit, array_limit) for k, v in item.items()}
        return item
    encoded = json.dumps(compact(value, 1600, 64), ensure_ascii=False, separators=(",", ":")) + "\n"
    if len(encoded.encode("utf-8")) > 65536:
        encoded = json.dumps(compact(value, 256, 8), ensure_ascii=False, separators=(",", ":")) + "\n"
    if len(encoded.encode("utf-8")) > 131072:
        raise ValueError("log record summary exceeds 128 KiB")
    return encoded


def read_chunk(stream, offset, output_budget, scan_budget):
    stream.seek(offset)
    records = []
    transferred = 0
    while stream.tell() - offset < scan_budget and transferred < output_budget:
        position = stream.tell()
        raw = stream.readline(MAX_RECORD + 1)
        if not raw or not raw.endswith(b"\n"):
            if len(raw) > MAX_RECORD:
                raise ValueError("JSONL record exceeds 16 MiB")
            stream.seek(position)
            break
        if len(raw) > MAX_RECORD:
            raise ValueError("JSONL record exceeds 16 MiB")
        # Preserve malformed complete records so the collector reports them,
        # while advancing past them to continue reading subsequent events.
        try:
            encoded = compact_record(json.loads(raw))
        except json.JSONDecodeError:
            encoded = "{\n"
        size = len(encoded.encode("utf-8"))
        if records and transferred + size > output_budget:
            stream.seek(position)
            break
        records.append(encoded)
        transferred += size
    return "".join(records), stream.tell()


def recent_records(stream):
    """Current state is independent of the historical usage cursor."""
    stream.seek(0)
    first = stream.readline(65537)
    metadata = ""
    try:
        if first.endswith(b"\n") and len(first) <= 65536:
            value = json.loads(first)
            if value.get("type") == "session_meta" or value.get("sessionId"):
                metadata = compact_record(value)
    except ValueError:
        pass
    size = os.fstat(stream.fileno()).st_size
    start = max(0, size - 1024 * 1024)
    stream.seek(start)
    if start:
        stream.readline()  # discard the leading partial record
    records = []
    for raw in stream.readlines():
        if not raw.endswith(b"\n"):
            continue
        try:
            records.append(compact_record(json.loads(raw)))
        except ValueError:
            continue
    chosen = []
    remaining = 32768
    for record in reversed(records):
        length = len(record.encode("utf-8"))
        if length > remaining:
            break
        chosen.append(record)
        remaining -= length
    return metadata + "".join(reversed(chosen))


def encode_result(result):
    encoded = json.dumps(result, ensure_ascii=False).encode("utf-8")
    if len(encoded) + 1 > 1048576:
        result["files"] = []
        result["complete"] = False
        result["errors"].append("probe output exceeds 1 MiB")
        encoded = json.dumps(result, ensure_ascii=False).encode("utf-8")
    return encoded + b"\n"


def codex_role(argv):
    if len(argv) > 1 and argv[1] == "app-server":
        return "updater" if "daemon" in argv[2:] or "pid-update-loop" in argv[2:] else "server"
    return "cli"


def claude_registration(path, pid, started):
    try:
        with open(path, "rb") as stream:
            if os.fstat(stream.fileno()).st_uid != os.getuid():
                raise ValueError("Claude registration owner mismatch")
            data = stream.read(65537)
    except FileNotFoundError:
        return None
    if len(data) > 65536:
        raise ValueError("Claude registration exceeds 64 KiB")
    record = json.loads(data)
    if not isinstance(record, dict) or record.get("pid") != pid or record.get("procStart") != str(started):
        raise ValueError("Claude registration process identity mismatch")
    sid = record.get("sessionId")
    if not isinstance(sid, str) or not sid or len(sid) > 128 or not all(c.isascii() and (c.isalnum() or c in "-_") for c in sid):
        raise ValueError("invalid Claude session identity")
    return record


def claude_transcript_matches(path, sid):
    with open(path, "rb") as stream:
        size = os.fstat(stream.fileno()).st_size
        for offset in (0, max(0, size - 1024 * 1024)):
            stream.seek(offset)
            data = stream.read(1024 * 1024)
            if offset:
                data = data.partition(b"\n")[2]
            for line in data.splitlines(keepends=True):
                if not line.endswith(b"\n"):
                    continue
                try:
                    value = json.loads(line)
                    if isinstance(value, dict) and value.get("sessionId") == sid:
                        return True
                except ValueError:
                    continue
            if size <= 1024 * 1024:
                break
    return False


def claude_session(root, pid, started):
    path = os.path.join(root, "sessions", str(pid) + ".json")
    record = claude_registration(path, pid, started)
    if record is None:
        return None
    sid = record["sessionId"]
    transcripts = glob.glob(os.path.join(root, "projects", "*", sid + ".jsonl"))
    if len(transcripts) > 1:
        raise ValueError("multiple Claude transcript candidates")
    transcript = transcripts[0] if transcripts else None
    if transcript and not claude_transcript_matches(transcript, sid):
        raise ValueError("Claude transcript identity could not be verified")
    current = claude_registration(path, pid, started)
    if current is None or current["sessionId"] != sid:
        raise ValueError("Claude session changed during discovery")
    return sid, transcript, {"busy": "Working", "idle": "Idle"}.get(current.get("status"))


def unix_peers():
    """Read kernel socket identities; never connect to an Agent's API socket."""
    peers = {}
    with socket.socket(socket.AF_NETLINK, socket.SOCK_RAW, 4) as channel:
        channel.settimeout(0.15)
        request = struct.pack("<BBHIIIII", socket.AF_UNIX, 0, 0, 0xffffffff, 0, 4, 0xffffffff, 0xffffffff)
        channel.send(struct.pack("<IHHII", 16 + len(request), 20, 0x301, 1, 0) + request)
        for _ in range(32):
            data = channel.recv(262144)
            offset = 0
            while offset + 16 <= len(data):
                size, kind, _, _, _ = struct.unpack_from("<IHHII", data, offset)
                if size < 16 or offset + size > len(data):
                    raise ValueError("invalid Unix socket snapshot")
                if kind == 3:
                    return peers
                if kind == 2:
                    raise OSError("Unix socket snapshot unavailable")
                body = data[offset + 16:offset + size]
                if len(body) < 16:
                    raise ValueError("invalid Unix socket identity")
                inode = struct.unpack_from("<I", body, 4)[0]
                position = 16
                while position + 4 <= len(body):
                    length, attribute = struct.unpack_from("<HH", body, position)
                    if length < 4 or position + length > len(body):
                        raise ValueError("invalid Unix socket attribute")
                    if attribute == 2 and length >= 8:
                        peers[inode] = struct.unpack_from("<I", body, position + 4)[0]
                    position += (length + 3) & ~3
                offset += (size + 3) & ~3
    raise OSError("Unix socket snapshot exceeds budget")


def probe(request):
    result = dict(origin_id="", user=pwd.getpwuid(os.getuid()).pw_name,
                  complete=True, instances=[], files=[], logs=[], errors=[], backfilling=False)
    origin = "wsl:" + request["registration"] + ":" + str(os.getuid())
    result["origin_id"] = origin
    boot = open("/proc/sys/kernel/random/boot_id").read().strip()
    home = os.path.expanduser("~")
    paths = {}
    active = set()
    now = int(time.time())

    def fail(error):
        result["complete"] = False
        result["errors"].append(str(error)[:300])

    def identity(pid):
        raw = open("/proc/%s/stat" % pid).read()
        return int(raw[raw.rfind(")") + 2:].split()[19])

    try:
        processes = {}
        launchers = set()
        roles = {}
        sockets = {}
        for directory in glob.glob("/proc/[0-9]*"):
            try:
                if os.stat(directory).st_uid != os.getuid():
                    continue
                argv = open(directory + "/cmdline", "rb").read().decode("utf-8", "replace").split("\0")
                name = os.path.basename(argv[0])
                args = " ".join(argv[:4])
                kind = "Codex" if name == "codex" or (name in ("node", "nodejs") and "@openai/codex" in args) else "Claude" if name == "claude" or (name in ("node", "nodejs") and ("claude-code" in args or "claude.js" in args)) else None
                if not kind:
                    continue
                role = codex_role(argv) if kind == "Codex" else "cli"
                if role == "updater":
                    continue
                pid = int(directory.rsplit("/", 1)[1])
                stat = open(directory + "/stat").read()
                fields = stat[stat.rfind(")") + 2:].split()
                processes[pid] = (kind, int(fields[1]), int(fields[19]))
                roles[pid] = role
                if name in ("node", "nodejs"):
                    launchers.add(pid)
            except FileNotFoundError:
                continue  # exited during discovery
            except (OSError, ValueError) as error:
                fail(error)
        roots = {(os.path.join(os.environ.get("CODEX_HOME", os.path.join(home, ".codex")), "sessions"), "Codex"),
                 (os.path.join(os.environ.get("CLAUDE_CONFIG_DIR", os.path.join(home, ".claude")), "projects"), "Claude"),
                 (os.path.join(home, ".cursor/projects"), "Cursor")}
        for pid, (kind, parent, started) in processes.items():
            if pid in launchers and any(p == pid and k == kind for k, p, _ in processes.values()):
                continue  # actual CLI child owns this invocation
            key = dict(origin_id=origin, boot_id=boot, pid=pid, process_started_at=started)
            matches = {}
            error = None
            auxiliary = False
            sockets[pid] = set()
            native_work_state = None
            env = {}
            try:
                env = dict(item.split("=", 1) for item in open("/proc/%s/environ" % pid, "rb").read().decode("utf-8", "replace").split("\0") if "=" in item)
            except (OSError, ValueError) as exc:
                error = str(exc)
                fail(exc)
            if kind == "Codex":
                try:
                    if env.get("CODEX_HOME"):
                        roots.add((os.path.join(env["CODEX_HOME"], "sessions"), "Codex"))
                    for fd in glob.glob("/proc/%s/fd/*" % pid):
                        try:
                            path = os.readlink(fd)
                            if path.startswith("socket:[") and path.endswith("]"):
                                sockets[pid].add(int(path[8:-1]))
                                continue
                            if not os.path.basename(path).startswith("rollout-") or not path.endswith(".jsonl"):
                                continue
                            # Independently open; never read through the process's fd.
                            with open(path, "rb") as stream:
                                held = os.stat(fd)
                                opened = os.fstat(stream.fileno())
                                if (held.st_dev, held.st_ino) != (opened.st_dev, opened.st_ino):
                                    fail("file identity changed")
                                    continue
                                first = stream.readline(65537)
                            if len(first) > 65536 or not first.endswith(b"\n"):
                                fail("invalid rollout metadata")
                                continue
                            meta = json.loads(first)
                            payload = meta.get("payload", {})
                            source = payload.get("source")
                            if source == "subagent" or isinstance(source, dict) and "subagent" in source:
                                paths[path] = kind  # usage still counted
                                auxiliary = True
                                continue
                            if meta.get("type") == "session_meta" and payload.get("id"):
                                matches[payload["id"]] = path
                                paths[path] = kind
                                active.add(path)
                        except FileNotFoundError:
                            continue
                        except (OSError, ValueError) as exc:
                            error = str(exc)
                            fail(exc)
                except (OSError, ValueError) as exc:
                    error = str(exc)
                    fail(exc)
            sid = next(iter(matches)) if len(matches) == 1 else None
            if kind == "Claude":
                try:
                    root = env.get("CLAUDE_CONFIG_DIR") or os.environ.get("CLAUDE_CONFIG_DIR") or os.path.join(home, ".claude")
                    if not os.path.isabs(root):
                        root = os.path.join(os.readlink("/proc/%s/cwd" % pid), root)
                    roots.add((os.path.join(root, "projects"), "Claude"))
                    native = claude_session(root, pid, started)
                    if native:
                        sid, transcript, native_work_state = native
                        if transcript:
                            paths[transcript] = kind
                            active.add(transcript)
                except (OSError, ValueError) as exc:
                    error = "Claude 原生会话登记：" + str(exc)
                    fail(error)
            if auxiliary and not matches:
                continue
            try:
                current_identity = identity(pid)
            except FileNotFoundError:
                continue
            if current_identity != started:
                fail("process identity changed: " + str(pid))
                continue
            # New Codex versions share a server across several CLI clients.
            # Every held, identity-checked main rollout is a separate session.
            shared = sorted(matches) if roles[pid] == "server" else []
            if shared:
                sid = None
            session = lambda sid: dict(origin_id=origin, agent_kind=kind, native_session_id=sid)
            result["instances"].append(dict(instance_key=key, agent_kind=kind,
                session_key=session(sid) if sid else None, shared_session_keys=[session(s) for s in shared],
                native_work_state=native_work_state, last_verified_at=now, open_state="Open",
                error=error if sid or shared else error or ("Claude 尚未提供有效的原生会话登记" if kind == "Claude" else "会话身份尚未关联或有多个候选")))
        linked_servers = {i["instance_key"]["pid"] for i in result["instances"]
                          if roles[i["instance_key"]["pid"]] == "server"
                          and (i["session_key"] or i["shared_session_keys"])}
        if linked_servers:
            try:
                peers = unix_peers()
                server_sockets = set().union(*(sockets[p] for p in linked_servers))
                # Suppress only frontends proven connected to an identified server.
                # If socket discovery fails, keep them unconfirmed.
                result["instances"] = [i for i in result["instances"]
                    if roles[i["instance_key"]["pid"]] != "cli" or i["agent_kind"] != "Codex"
                    or i["session_key"] or i["shared_session_keys"]
                    or not any(peers.get(s) in server_sockets for s in sockets[i["instance_key"]["pid"]])]
            except (OSError, ValueError, AttributeError) as exc:
                fail("CLI connection verification failed: " + str(exc))
        candidates = []
        for root, kind in roots:
            if not os.path.exists(root):
                continue
            for directory, _, names in os.walk(root, onerror=fail):
                for name in names:
                    if not name.endswith(".jsonl"):
                        continue
                    path = os.path.join(directory, name)
                    relative = os.path.relpath(path, root)
                    if kind == "Claude" and len(relative.split(os.sep)) != 2:
                        continue
                    if kind == "Cursor" and "agent-transcripts" not in relative.split(os.sep):
                        continue
                    try:
                        modified = os.stat(path).st_mtime
                        candidates.append((modified, path, kind))
                    except OSError as exc:
                        fail(exc)
        candidates.sort(reverse=True)
        for modified, path, kind in candidates:
            if modified >= request["day_start"]:
                paths[path] = kind
        # Advance every file, with small chunks and a rotating start for fairness.
        ordered = sorted(paths)
        if ordered:
            cursor = int(request.get("cursor", 0)) % len(ordered)
            ordered = ordered[cursor:] + ordered[:cursor]
        # Bootstrap identified sessions before historical usage backfill. After
        # the first chunk, resume the rotating queue so history cannot starve.
        ordered.sort(key=lambda path: not (path in active and not request.get("offsets", {}).get(path, ["", 0])[1]))
        size_budget = 0
        scanned = 0
        for path in ordered:
            try:
                with open(path, "rb") as stream:
                    stat = os.fstat(stream.fileno())
                    file_id = "%s:%s" % (stat.st_dev, stat.st_ino)
                    previous = request.get("offsets", {}).get(path, ["", 0])
                    offset = previous[1] if previous[0] == file_id and previous[1] <= stat.st_size else 0
                    data, next_offset = "", offset
                    if size_budget < 262144 and scanned < 8 * 1024 * 1024:
                        data, next_offset = read_chunk(stream, offset, 262144 - size_budget,
                                                      min(2 * 1024 * 1024, 8 * 1024 * 1024 - scanned))
                    recent = recent_records(stream) if path in active else ""
                if not data and not recent and offset < stat.st_size:
                    result["backfilling"] = True
                    continue
                result["files"].append(dict(path=path, kind=paths[path], file_id=file_id,
                    size=stat.st_size, offset=offset, data=data, next_offset=next_offset, recent_data=recent))
                size_budget += len(data.encode("utf-8"))
                scanned += next_offset - offset
                if next_offset < stat.st_size:
                    result["backfilling"] = True
            except (OSError, ValueError) as exc:
                fail(exc)
    except (TimeoutError, OSError) as exc:
        fail(exc)
    return result


if __name__ == "__main__":
    def timeout(*_):
        raise TimeoutError("guest probe budget exceeded")
    signal.signal(signal.SIGALRM, timeout)
    signal.setitimer(signal.ITIMER_REAL, 1.6)
    request = json.load(sys.stdin)
    result = probe(request)
    signal.setitimer(signal.ITIMER_REAL, 0)
    sys.stdout.buffer.write(encode_result(result))
