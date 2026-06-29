#!/usr/bin/env -S uv run --script
# /// script
# dependencies = [
#   "chatlas[mcp]",
# ]
# ///

"""Register mcp-repl files mode with async chatlas MCP tools."""

import asyncio

from chatlas import ChatOpenAI

from chatlas_tools import (
    OverflowMode,
    list_directory,
    read_text_file,
    register_tool_repl,
)

PROMPT = (
    "Tell me something interesting about the penguins dataset. "
    "Use the REPL tool to do analysis."
)


async def main() -> None:
    chat = ChatOpenAI()
    chat.register_tool(list_directory)
    chat.register_tool(read_text_file)

    try:
        await register_tool_repl(chat, overflow=OverflowMode.FILES)
        response = await chat.chat_async(PROMPT, echo="none")
        print(await response.get_content())
    finally:
        await chat.cleanup_mcp_tools()


if __name__ == "__main__":
    asyncio.run(main())
