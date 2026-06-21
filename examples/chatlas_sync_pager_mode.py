"""Use synchronous chatlas tools with mcp-repl pager mode."""

from chatlas import ChatOpenAI

from mcp_repl_tool import McpReplTool


def main() -> None:
    chat = ChatOpenAI(
        system_prompt=(
            "Use the `repl` tool for Python execution. It runs mcp-repl in "
            "pager mode. If output opens the pager, send pager commands "
            "through the `input` argument: empty "
            "input or `:next` advances, `:/pattern` searches, and `:q` exits."
        ),
    )

    repl = McpReplTool(
        [
            "--sandbox",
            "workspace-write",
            "--oversized-output",
            "pager",
            "--interpreter",
            "python",
        ]
    )

    try:
        repl.start()
        chat.register_tool(repl)
        response = chat.chat(
            "Use the repl tool to print 400 numbered lines in the format "
            "sync-record-0001 through sync-record-0400. When pager mode starts, "
            "search the pager for sync-record-0250, then answer with the "
            "matching line and the pager command that found it.",
            echo="none",
        )
        print(response.get_content())
    finally:
        repl.close()


if __name__ == "__main__":
    main()
