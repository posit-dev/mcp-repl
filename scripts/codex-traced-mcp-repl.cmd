@echo off
setlocal

set "MCP_REPL_DEBUG_DIR=C:\Users\kalin\Documents\GitHub\mcp-repl\.mcp-repl-debug"
set "MCP_REPL_KEEP_SESSION_TMPDIR=1"
set "MCP_REPL_TRACE_FORWARD_STDERR=1"

"C:\Python314\python.exe" "C:\Users\kalin\Documents\GitHub\mcp-repl\scripts\codex-stdio-trace-win.py" "C:\Users\kalin\Documents\GitHub\mcp-repl\target\release\mcp-repl.exe" %*

endlocal
