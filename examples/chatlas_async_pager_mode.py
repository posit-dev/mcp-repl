"""Register mcp-repl pager mode with async chatlas MCP tools."""

import asyncio

from chatlas import ChatOpenAI

PROMPT = (
    "Tell me something interesting about the penguins dataset. "
    "Use the REPL tool to do analysis."
)


async def main() -> None:
    chat = ChatOpenAI()

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

        response = await chat.chat_async(PROMPT, echo="none")
        print(await response.get_content())
    finally:
        await chat.cleanup_mcp_tools()


if __name__ == "__main__":
    asyncio.run(main())
