"""Restart preserves delivery order between MCP recovery and integrator output."""

import sys
import tempfile
from contextlib import closing
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[3]))

from boundaries.client_server.integrations.test_control import ControlClient
from boundaries.client_server.output.test_recovery import cancel_result
from boundaries.client_server.python.test_without_r import selected_environment
from support.allocations import AllocationProfile
from support.checkpoints import FifoCheckpoint, wait_for_checkpoint
from support.client import McpClient
from support.execution import DIRECT, Execution, executions
from support.normalization import code, normalize_recording
from support.records import TranscriptWithCompanions
from support.requirements import NATIVE_FIXTURES, POSIX, requires
from support.suites import run_this_suite


@requires(POSIX, NATIVE_FIXTURES)
@executions(DIRECT)
def test_restart_orders_cancelled_mcp_error_before_worker_notices(
    binary: Path, execution: Execution
) -> TranscriptWithCompanions:
    with tempfile.TemporaryDirectory(
        dir="/tmp", prefix="console-control-"
    ) as directory:
        root = Path(directory)
        (root / "python3").symlink_to(sys.executable)
        endpoint = root / "control.sock"
        with (
            closing(FifoCheckpoint.create(root / "entered")) as entered,
            closing(FifoCheckpoint.create(root / "release")) as release,
            closing(FifoCheckpoint.create(root / "result-reached")) as result_reached,
            closing(FifoCheckpoint.create(root / "result-release")) as result_release,
            closing(AllocationProfile(root)) as profile,
            McpClient(
                binary,
                execution.serve("--control-socket", str(endpoint)),
                {
                    **selected_environment(root),
                    **profile.environment,
                    "MCP_CONSOLE_TEST_RESULT_REACHED": str(result_reached.path),
                    "MCP_CONSOLE_TEST_RESULT_RELEASE": str(result_release.path),
                },
                root,
            ) as client,
        ):
            client.transcript.append(
                {
                    "scenario": "Restart returns a cancelled MCP error before its worker notices while the integrator receives only its own stopped cell.",
                    "fixture": "A Python cell ignores SIGINT and waits at a FIFO. A native checkpoint holds the write of an MCP interrupt response until cancellation returns its delivery to MCP recovery.",
                }
            )
            client.initialize_and_list_tools()
            client.expect(python="sentinel = 0")
            # fmt: python
            source = code(f"""
                import signal

                previous_handler = signal.signal(signal.SIGINT, signal.SIG_IGN)
                print("integrator pending", flush=True)
                with open({str(entered.path)!r}, "wb", buffering=0) as gate:
                    gate.write(b"1")
                with open({str(release.path)!r}, "rb", buffering=0) as gate:
                    gate.read(1)
                signal.signal(signal.SIGINT, previous_handler)
                print("integrator finished")
                """)
            with ControlClient(endpoint) as control:
                # Receiving the MCP setup response can precede its delivery
                # acknowledgment. Queue setup behind that retained owner.
                pending = control.start(
                    op="evaluate",
                    language="python",
                    code=source,
                    busy="wait",
                    wait_ms=30000,
                )
                try:
                    entered.wait(client=client)
                    (recording,) = (root / ".agents/console/sessions").iterdir()

                    def captured() -> Path | None:
                        return next(
                            (
                                path
                                for path in (recording / "capture/sessions").glob(
                                    "*/cell-*.txt"
                                )
                                if path.read_text() == "integrator pending\n"
                            ),
                            None,
                        )

                    wait_for_checkpoint(
                        captured,
                        "integrator output captured before MCP cancellation",
                        root=recording,
                        recursive=True,
                        watch_files=lambda: (recording / "capture/sessions").glob(
                            "*/cell-*.txt"
                        ),
                        client=client,
                    )
                    profile.pause_results(True)
                    interrupted = client.start_send(
                        control="interrupt", python="sentinel = 99"
                    )
                    # Cancellation returns recovery synchronously before the
                    # following ping is read from the same MCP input stream.
                    cancel_result(client, interrupted, result_reached, result_release)
                    profile.pause_results(False)

                    restarted = client.send(control="restart")
                    assert restarted == {
                        "content": [
                            {
                                "type": "text",
                                "text": (
                                    "interrupted evaluation is still active; cell was not run\n"
                                    "[worker stopped: in-memory state lost]\n"
                                    "[starting new worker]\n"
                                    "[idle]"
                                ),
                            }
                        ],
                        "isError": True,
                    }, restarted
                    stopped = control.receive(pending)
                    assert stopped == {
                        "id": pending["request"]["id"],
                        "result": {
                            "execution_error": None,
                            "resultType": "complete",
                            "content": [
                                {
                                    "type": "text",
                                    "text": (
                                        "integrator pending\n"
                                        "[stopped by session restart request before evaluation finished]"
                                    ),
                                }
                            ],
                            "isError": True,
                        },
                    }, stopped
                    client.expect("\n[idle]")
                    client.expect(
                        "replacement ready\n", python="print('replacement ready')"
                    )
                    transcript = normalize_recording(client.finish(), root)
                finally:
                    profile.pause_results(False)
                    result_release.release()
                    release.release()
            return TranscriptWithCompanions(
                transcript,
                {"control.yaml": normalize_recording(control.transcript, root)},
            )


if __name__ == "__main__":
    run_this_suite(__file__)
