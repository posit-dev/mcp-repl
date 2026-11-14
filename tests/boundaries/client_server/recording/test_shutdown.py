#!/usr/bin/env -S uv run --script

import json
import os
import socket
import sys
import tempfile
from collections.abc import Iterator
from contextlib import ExitStack, closing, contextmanager
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[3]))

from boundaries.client_server._harness import (
    TEST_GATED_RESPONSE_SIZE,
    SocketGateMcpClient,
)
from boundaries.client_server.recording.test_process import recording_events
from boundaries.client_server.recording.test_journal import local_discovery
from support.checkpoints import FifoCheckpoint
from support.client import McpClient, stop_client
from support.execution import DIRECT
from support.native import LOADER_VARIABLE, build_interposer, compile_interposer
from support.records import Transcript
from support.requirements import NATIVE_FIXTURES, PROCESS_EVENTS, R, command, requires
from support.suites import run_this_suite


@contextmanager
def blocked_response(
    binary: Path, *, ping: bool = False
) -> Iterator[tuple[SocketGateMcpClient, dict, Path]]:
    with tempfile.TemporaryDirectory() as directory, ExitStack() as resources:
        root = Path(directory)
        gates = {
            name: resources.enter_context(closing(FifoCheckpoint.create(root / name)))
            for name in ("write-reached", "write-release")
        }
        writer = compile_interposer(
            Path(__file__).parents[1] / "server/_response_admission.c",
            root / "response-admission",
        )
        client = SocketGateMcpClient(
            binary,
            DIRECT.serve(
                "--worker", str(Path(__file__).resolve().parents[3] / "fixtures/zod")
            ),
            {
                **os.environ,
                LOADER_VARIABLE: str(writer),
                "MCP_CONSOLE_HOME": str(root / "console-home"),
                "MCP_CONSOLE_TEST_INITIAL_WRITE_REACHED": str(
                    gates["write-reached"].path
                ),
                "MCP_CONSOLE_TEST_INITIAL_WRITE_RELEASE": str(
                    gates["write-release"].path
                ),
            },
            root,
        )
        try:
            client.initialize_and_list_tools()
            marker = "https://invalid.example/" + "x" * TEST_GATED_RESPONSE_SIZE
            if ping:
                client.send(r="echo ready")
                first = client.send_message(
                    {"jsonrpc": "2.0", "id": marker, "method": "ping"}
                )
            else:
                first = client.start_send(requirements={"python": [marker]})
            gates["write-reached"].wait(
                "first response write is incomplete", client=client
            )
            # The long string ID intentionally backpressures an ordinary ping,
            # independently of any console response reservation.
            expected_prefix = f'"id":{json.dumps(first["id"])}'.encode()[:64]
            prefix = client.stdout.stream.recv(256, socket.MSG_PEEK)
            assert expected_prefix in prefix and b"\n" not in prefix, prefix
            yield client, first, root
        finally:
            gates["write-release"].release()
            if client.process.poll() is None:
                stop_client(client)
            client.close_test_stdio()


@requires(NATIVE_FIXTURES)
def test_failed_response_write_is_recorded_before_finalization(
    binary: Path,
) -> Transcript:
    return failed_write_before_finalization(binary, ping=False)


@requires(NATIVE_FIXTURES)
def test_failed_ping_write_is_recorded_before_finalization(binary: Path) -> Transcript:
    return failed_write_before_finalization(binary, ping=True)


def failed_write_before_finalization(binary: Path, *, ping: bool) -> Transcript:
    with blocked_response(binary, ping=ping) as (client, first, root):
        client.close_input_observer()
        client.stdin.close()
        # The native stdout writer remains blocked until after process exit.
        # The real shutdown deadline must cancel its async delivery owner.
        assert client.process.wait(timeout=15) == 0
        assert client.stderr.read() == ""
        (recording,) = (root / "console-home/sessions").iterdir()
        events = recording_events(recording)
        (assembled,) = [
            e
            for e in events
            if e["event"] == "protocol"
            and e["direction"] == "assembled"
            and e["message"].get("id") == first["id"]
        ]
        deliveries = [
            e
            for e in events
            if e["event"] == "delivery" and e["request_id"] == first["id"]
        ]
        assert len(deliveries) == 1, [(e["event"], e["sequence"]) for e in events]
        delivery = deliveries[0]
        assert (
            delivery["call_id"] == (None if ping else 1)
            and delivery["delivered"] is False
        ), delivery
        assert assembled["sequence"] < delivery["sequence"] < events[-1]["sequence"]
        assert events[-1]["event"] == "recording_ended"
        return [
            *client.transcript[:3],
            {"capture": "failed response write precedes finalization"},
        ]


@requires(NATIVE_FIXTURES, PROCESS_EVENTS, R)
@requires(command("rustc"))
def test_startup_failure_journaling_keeps_ping_live(binary: Path) -> Transcript:
    with tempfile.TemporaryDirectory() as directory, ExitStack() as resources:
        root = Path(directory)
        environment, discovery = resources.enter_context(
            local_discovery(root, fail=True)
        )
        observed = resources.enter_context(
            closing(FifoCheckpoint.create(root / "observed"))
        )
        journal_release = resources.enter_context(
            closing(FifoCheckpoint.create(root / "journal-release"))
        )
        environment[LOADER_VARIABLE] += os.pathsep + str(
            build_interposer(root, "recording_observation")
        )
        environment.update(
            MCP_CONSOLE_TEST_OBSERVED_EVENT="startup_failed",
            MCP_CONSOLE_TEST_JOURNAL_OBSERVED=str(observed.path),
            MCP_CONSOLE_TEST_JOURNAL_RELEASE=str(journal_release.path),
        )
        with McpClient(
            binary, DIRECT.serve(), environment, root, response_timeout=2
        ) as client:
            try:
                client.initialize_and_list_tools()
                discovery.wait("discovery ready to fail")
                discovery.release()
                observed.wait("startup failure journal writer stalled", client=client)
                assert client.request("ping")["result"] == {}
            finally:
                discovery.release()
                journal_release.release()
            _, stderr = client.finish_with_standard_error(expected_exit_status=1)
            assert "fixture R discovery failed" in stderr
            assert not (root / ".agents/console/sessions").exists()
            return [
                *client.transcript[:3],
                {
                    "startup_failure_without_send": "journal wait leaves ping live; capture discarded"
                },
            ]


if __name__ == "__main__":
    run_this_suite(__file__)
