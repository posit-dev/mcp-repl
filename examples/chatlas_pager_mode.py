"""Use chatlas with mcp-repl pager mode."""

import asyncio

from chatlas import ChatOpenAI


async def main() -> None:
    chat = ChatOpenAI(
        system_prompt=(
            "Use the `repl` MCP tool for Python execution. This mcp-repl "
            "server runs in pager mode. If output opens the pager, send pager "
            "commands through the tool's `input` argument: empty input or "
            "`:next` advances, `:/pattern` searches, and `:q` exits."
        ),
    )

    try:
        await chat.register_mcp_tools_stdio_async(
            name="mcp_repl_pager",
            command="mcp-repl",
            args=[
                "--sandbox",
                "workspace-write",
                "--oversized-output",
                "pager",
                "--interpreter",
                "python",
            ],
            include_tools=["repl"],
        )

        response = await chat.chat_async(
            "Use the repl tool to print 400 numbered lines in the format "
            "record-0001 through record-0400. When pager mode starts, search "
            "the pager for record-0250, then answer with the matching line "
            "and the pager command that found it.",
            echo="none",
        )
        print(await response.get_content())
    finally:
        await chat.cleanup_mcp_tools()


if __name__ == "__main__":
    asyncio.run(main())
