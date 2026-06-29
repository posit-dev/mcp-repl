#!/usr/bin/env -S uv run --script
# /// script
# dependencies = [
#   "chatlas",
# ]
# ///

"""Register mcp-repl pager mode with ordinary chatlas tools."""

from chatlas import ChatOpenAI

from chatlas_tools import repl_tools

PROMPT = (
    "Tell me something interesting about the penguins dataset. "
    "Use the REPL tool to do analysis."
)


def main() -> None:
    chat = ChatOpenAI()

    for tool in repl_tools(overflow="pager"):
        chat.register_tool(tool)

    response = chat.chat(PROMPT, echo="none")
    print(response.get_content())


if __name__ == "__main__":
    main()
