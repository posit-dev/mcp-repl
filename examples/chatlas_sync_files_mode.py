"""Use synchronous chatlas tools with mcp-repl overflow files mode."""

from chatlas import ChatOpenAI

from mcp_repl_tool import McpReplTool, list_directory, read_text_file


def main() -> None:
    chat = ChatOpenAI(
        system_prompt=(
            "Use the `repl` tool for Python execution. It runs mcp-repl in "
            "overflow files mode. When a reply discloses an output bundle "
            "path, inspect it with `list_directory` and `read_text_file`. "
            "Prefer explicit line ranges when reading large files; "
            "`read_text_file` does not impose a fixed size limit."
        ),
    )
    chat.register_tool(list_directory)
    chat.register_tool(read_text_file)

    repl = McpReplTool(
        [
            "--sandbox",
            "workspace-write",
            "--oversized-output",
            "files",
            "--interpreter",
            "python",
        ]
    )

    try:
        repl.start()
        chat.register_tool(repl)
        response = chat.chat(
            "Use the repl tool to print 2500 numbered lines in the format "
            "sync-bundle-record-0001 through sync-bundle-record-2500 so the "
            "output overflows to files mode. Then inspect the disclosed bundle: "
            "list the bundle directory, read transcript.txt with line ranges "
            "until you find sync-bundle-record-1750, and report the matching "
            "line.",
            echo="none",
        )
        print(response.get_content())
    finally:
        repl.close()


if __name__ == "__main__":
    main()
