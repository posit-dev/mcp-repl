#!/usr/bin/env -S uv run --script

import os
import sys
import tempfile
from contextlib import closing
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[3]))

from support.allocations import AllocationProfile
from support.client import McpClient
from support.execution import DIRECT
from support.previews import (
    assert_preview,
    cell_text,
    compact_previews,
    normalize_preview_paths,
)
from support.records import Transcript
from support.requirements import NATIVE_FIXTURES, requires
from support.suites import run_this_suite


@requires(NATIVE_FIXTURES)
def test_same_producer_tiny_events_keep_bounded_storage(binary: Path) -> Transcript:
    worker = Path(__file__).resolve().parents[3] / "fixtures/zod"
    with tempfile.TemporaryDirectory() as temporary:
        with (
            closing(AllocationProfile(Path(temporary))) as profile,
            McpClient(
                binary,
                DIRECT.serve("--worker", str(worker)),
                {**os.environ, **profile.environment},
            ) as client,
        ):
            client.initialize_and_list_tools()
            client.send(r="echo ready")
            profile.start()
            result = client.send(r="preview same producer")
            client.request("ping")
            _, largest = profile.stop()
            assert largest <= 128 * 1024, largest
            assert not result["isError"], result
            assert len(result["content"]) == 1, result
            emitted = (
                "preview head\n" + "ab" * 100000 + "\npreview tail: final diagnostic\n"
            )
            assert_preview(result["content"][0]["text"], emitted)
            assert cell_text(client, 2).encode() == emitted.encode()
            assert client.send(session="default")["content"] == [
                {"type": "text", "text": "\n[idle]"}
            ]
            assert client.send(r="echo fresh")["content"] == [
                {"type": "text", "text": "zod: fresh\n"}
            ]
            normalize_preview_paths(client)
            compact_previews(client, "ab")
            return client.finish()


@requires(NATIVE_FIXTURES)
def test_complete_direct_chunks_do_not_add_decoder_copies(binary: Path) -> Transcript:
    fixtures = Path(__file__).resolve().parents[3] / "fixtures"
    with tempfile.TemporaryDirectory() as temporary:
        with (
            closing(
                AllocationProfile(Path(temporary), thread="worker events")
            ) as profile,
            McpClient(
                binary,
                DIRECT.serve(
                    "--worker",
                    str(fixtures / "zod"),
                    "--relay",
                    str(fixtures / "server_relay/scripted_relay.py"),
                ),
                {
                    **os.environ,
                    **profile.environment,
                    "TMPDIR": temporary,
                    "MCP_CONSOLE_TEST_RELAY_SCENARIO": "preview_direct_allocations",
                },
            ) as client,
        ):
            client.initialize_and_list_tools()
            allocations = []
            emitted = "ab" * (8192 * 512) + "\nfinal diagnostic\n"
            for call_id, stream in enumerate(
                ("console_output", "console_output", "stdout"), 1
            ):
                profile.start()
                result = client.send(r=stream)
                client.request("ping")
                allocated, _ = profile.stop()
                if call_id > 1:
                    allocations.append(allocated)
                assert not result["isError"], result
                assert len(result["content"]) == 1, result
                assert_preview(result["content"][0]["text"], emitted)
                assert cell_text(client, call_id).encode() == emitted.encode()
            # Both paths receive the same complete UTF-8 chunks. A direct
            # decoder must not copy another 8 MiB while ingesting them.
            # Only their shared ingestion thread is measured, which excludes
            # concurrent journal/projection work from this comparison.
            assert all(allocations), allocations
            assert allocations[1] < allocations[0] + 1024 * 1024, allocations
            assert client.send(session="default")["content"] == [
                {"type": "text", "text": "\n[idle]"}
            ]
            normalize_preview_paths(client)
            compact_previews(client, "ab")
            return client.finish()


@requires(NATIVE_FIXTURES)
def test_tiny_events_have_bounded_allocation_work(binary: Path) -> Transcript:
    worker = Path(__file__).resolve().parents[3] / "fixtures/zod"
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        with (
            closing(AllocationProfile(root)) as profile,
            McpClient(
                binary,
                DIRECT.serve("--worker", str(worker)),
                {**os.environ, **profile.environment},
            ) as client,
        ):
            client.initialize_and_list_tools()
            client.send(r="echo ready")
            profile.start()
            result = client.send(r="preview many tiny events", timeout_ms=600_000)
            client.request("ping")
            allocated, _ = profile.stop()
            assert allocated < 512 * 1024 * 1024, allocated
            assert not result["isError"], result
            assert [part["type"] for part in result["content"]] == [
                "text",
                "image",
                "text",
            ]
            assert_preview(
                result["content"][0]["text"],
                "preview head\n" + "ab" * 100000 + "\npreview tail: final diagnostic\n",
            )
            assert result["content"][-1]["text"] == "after final image\n"
            normalize_preview_paths(client)
            compact_previews(client, "x", "ab")
            return client.finish()


@requires(NATIVE_FIXTURES)
def test_text_sizing_does_not_copy_images(binary: Path) -> Transcript:
    worker = Path(__file__).resolve().parents[3] / "fixtures/zod"
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        with (
            closing(AllocationProfile(root)) as profile,
            McpClient(
                binary,
                DIRECT.serve("--worker", str(worker)),
                {**os.environ, **profile.environment},
            ) as client,
        ):
            client.initialize_and_list_tools()
            client.send(r="echo ready")
            allocations = []
            for index, scenario in enumerate(
                (
                    "preview allocation image",
                    "preview allocation image",
                    "preview allocation image and text",
                )
            ):
                profile.start()
                result = client.send(r=scenario)
                client.request("ping")
                allocated, _ = profile.stop()
                # Warm the transport's image-sized buffers before comparison.
                if index:
                    allocations.append(allocated)
                assert not result["isError"], result
                image = result["content"][0]
                assert image == {
                    "type": "image",
                    "data": "A" * (8 * 1024 * 1024),
                    "mimeType": "image/png",
                }
                image["data"] = "<image byte-identical to 6 MiB of zero bytes>"
            # Adding 32 KiB of text may allocate its bounded preview and notices,
            # but must not add copies proportional to the unrelated 8 MiB image.
            assert allocations[1] < allocations[0] + 2 * 1024 * 1024, allocations
            assert_preview(
                result["content"][1]["text"],
                "preview head\n" + "x" * 32768 + "\npreview tail\n",
            )
            normalize_preview_paths(client)
            compact_previews(client, "x", "ab")
            return client.finish()


if __name__ == "__main__":
    run_this_suite(__file__)
