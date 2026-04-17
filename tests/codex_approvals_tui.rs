mod common;

use common::TestResult;

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
mod unix_impl {
    use super::{TestResult, common};
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use serde_json::Value;
    use std::collections::BTreeMap;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    use std::io::{ErrorKind, Read, Write};
    use std::net::SocketAddr;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::Stdio;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    use std::sync::mpsc::{Receiver, RecvTimeoutError};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};
    use toml_edit::DocumentMut;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    use vt100::Parser;

    const WORKSPACE_WRITE_MARKER: &str = "SANDBOX_TEST_1";
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    const FULL_ACCESS_MARKER: &str = "SANDBOX_TEST_2";
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    const WARMUP_MARKER: &str = "WARMUP_TEST";
    const INSTALL_SCRIPTED_TOOL_CALL_MARKER: &str = "INSTALL_SCRIPTED_TOOL_CALL";
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    const FULL_ACCESS_TEST_ENV: &str = "MCP_REPL_ENABLE_FULL_ACCESS_TUI_TEST";

    struct IsolatedCodexEnv {
        _temp_dir: tempfile::TempDir,
        workspace: PathBuf,
        codex_home: PathBuf,
        debug_dir: PathBuf,
    }

    #[derive(Clone, Copy)]
    enum ExecSnapshotMode {
        Json,
        Plain,
    }

    pub(super) async fn run_codex_exec_initial_sandbox_state() -> TestResult<String> {
        run_codex_exec_initial_sandbox_state_for_mode(ExecSnapshotMode::Json).await
    }

    pub(super) async fn run_codex_exec_initial_sandbox_state_plain() -> TestResult<String> {
        run_codex_exec_initial_sandbox_state_for_mode(ExecSnapshotMode::Plain).await
    }

    pub(super) async fn run_install_then_codex_exec_uses_generated_config() -> TestResult<()> {
        if !codex_available() {
            eprintln!("codex not found on PATH; skipping");
            return Ok(());
        }
        if !loopback_bind_available().await {
            eprintln!("loopback TCP bind unavailable; skipping");
            return Ok(());
        }

        let tool_args = tool_args_for_code("1+1");
        let mock_server =
            MockResponsesServer::start_with_first_user_turn_tool_call(tool_name(), tool_args)
                .await?;
        let mcp_repl = resolve_mcp_repl_path()?;
        let env = create_isolated_codex_env_for_install(&mock_server.base_url())?;
        let sandbox_mode = codex_exec_sandbox_mode();

        run_mcp_repl_install_for_codex_r(&mcp_repl, &env.codex_home)?;
        assert_codex_install_wrote_r_inherit_config(&env.codex_home, &mcp_repl)?;

        let cmd = codex_exec_command(
            &env,
            &mock_server.base_url(),
            INSTALL_SCRIPTED_TOOL_CALL_MARKER,
            sandbox_mode,
            Some("--json"),
        );

        let output = run_command_with_timeout(cmd, Duration::from_secs(60))?;
        let stdout = String::from_utf8(output.stdout)
            .map_err(|err| format!("codex exec stdout was not valid UTF-8: {err}"))?;
        let stderr = String::from_utf8(output.stderr)
            .map_err(|err| format!("codex exec stderr was not valid UTF-8: {err}"))?;
        let outputs = mock_server.function_call_outputs().await;
        if codex_exec_environment_unavailable(&stdout, &stderr, &outputs) {
            eprintln!("codex exec sandbox/backend unavailable in this environment; skipping");
            return Ok(());
        }
        if !output.status.success() {
            let request_paths = mock_server.request_paths().await;
            let last_request = mock_server.last_request().await;
            return Err(format!(
                "codex exec with installed config failed with status {status}\nrequest_paths: {request_paths:?}\nlast_request: {last_request:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
                status = output.status
            )
            .into());
        }

        assert_exec_output_contains_tool_call(&stdout, "r", "repl", "1+1\n")?;

        let saw_result = outputs.iter().any(|out| out.contains("[1] 2"));
        if !saw_result {
            let request_paths = mock_server.request_paths().await;
            let last_request = mock_server.last_request().await;
            return Err(format!(
                "expected installed codex config to run r repl and return 2\nrequest_paths: {request_paths:?}\nlast_request: {last_request:?}\nstdout:\n{stdout}\nstderr:\n{stderr}\noutputs: {outputs:?}"
            )
            .into());
        }

        Ok(())
    }

    async fn run_codex_exec_initial_sandbox_state_for_mode(
        mode: ExecSnapshotMode,
    ) -> TestResult<String> {
        if !codex_available() {
            eprintln!("codex not found on PATH; skipping");
            return Ok(String::new());
        }
        if !loopback_bind_available().await {
            eprintln!("loopback TCP bind unavailable; skipping");
            return Ok(String::new());
        }

        let tool_args = tool_args_for_code(&sandbox_run_code());
        let mock_server =
            MockResponsesServer::start(tool_name(), tool_args.clone(), Some(tool_args)).await?;
        let mcp_repl = resolve_mcp_repl_path()?;
        let env = create_isolated_codex_env(&mcp_repl, &mock_server.base_url())?;
        let sandbox_mode = codex_exec_sandbox_mode();

        let cmd = codex_exec_command(
            &env,
            &mock_server.base_url(),
            &format!("{WORKSPACE_WRITE_MARKER}: run the sandbox write test"),
            sandbox_mode,
            match mode {
                ExecSnapshotMode::Json => Some("--json"),
                ExecSnapshotMode::Plain => None,
            },
        );

        let output = run_command_with_timeout(cmd, Duration::from_secs(60))?;
        let stdout = String::from_utf8(output.stdout)
            .map_err(|err| format!("codex exec stdout was not valid UTF-8: {err}"))?;
        let stderr = String::from_utf8(output.stderr)
            .map_err(|err| format!("codex exec stderr was not valid UTF-8: {err}"))?;
        let outputs = mock_server.function_call_outputs().await;
        if codex_exec_environment_unavailable(&stdout, &stderr, &outputs) {
            eprintln!("codex exec sandbox/backend unavailable in this environment; skipping");
            return Ok(String::new());
        }
        if !output.status.success() {
            let request_paths = mock_server.request_paths().await;
            let last_request = mock_server.last_request().await;
            return Err(format!(
                "codex exec failed with status {status}\nrequest_paths: {request_paths:?}\nlast_request: {last_request:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
                status = output.status
            )
            .into());
        }

        wait_for_log_contains(
            &env.debug_dir,
            codex_exec_expected_sandbox_log(),
            Duration::from_secs(10),
        )?;
        let saw_write_ok = outputs.iter().any(|out| out.contains("WRITE_OK"));
        if !saw_write_ok {
            let request_paths = mock_server.request_paths().await;
            let last_request = mock_server.last_request().await;
            return Err(format!(
                "expected workspace-write call to succeed\nrequest_paths: {request_paths:?}\nlast_request: {last_request:?}\nstdout:\n{stdout}\nstderr:\n{stderr}\noutputs: {outputs:?}"
            )
            .into());
        }

        render_exec_snapshot(mode, &stdout, &stderr, &env.workspace, &env.codex_home)
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub(super) async fn run_codex_tui_full_access_sandbox_update() -> TestResult<()> {
        if !full_access_test_enabled() {
            eprintln!(
                "{FULL_ACCESS_TEST_ENV} is not set; skipping full-access Codex TUI integration test"
            );
            return Ok(());
        }
        if !codex_available() {
            eprintln!("codex not found on PATH; skipping");
            return Ok(());
        }
        if !common::sandbox_exec_available() {
            eprintln!("sandbox-exec unavailable; skipping");
            return Ok(());
        }
        if !loopback_bind_available().await {
            eprintln!("loopback TCP bind unavailable; skipping");
            return Ok(());
        }

        let workspace_args = tool_args_for_code(&sandbox_run_code());
        let full_access_probe_args = tool_args_for_code(&outside_workspace_probe_code()?);
        let mock_server =
            MockResponsesServer::start(tool_name(), workspace_args, Some(full_access_probe_args))
                .await?;
        let mcp_repl = resolve_mcp_repl_path()?;
        let env = create_isolated_codex_env(&mcp_repl, &mock_server.base_url())?;

        let mut driver = CodexPtyDriver::spawn(
            &env.codex_home,
            &env.workspace,
            &mock_server.base_url(),
            &env.debug_dir,
        )?;
        driver.drain(Duration::from_millis(800));
        driver.ensure_running("after startup")?;
        driver.wait_for_warmup(Duration::from_secs(10))?;
        wait_for_log_contains(&env.debug_dir, "workspace-write", Duration::from_secs(10))?;

        driver.send_line(&format!(
            "{FULL_ACCESS_MARKER}: probe write before full access"
        ))?;
        driver.wait_for_contains("WRITE_ERROR:", Duration::from_secs(20))?;
        driver.wait_for_contains("Tool call 1 completed", Duration::from_secs(20))?;

        driver.send_line("/approvals")?;
        driver.wait_for_contains("Update Model Permissions", Duration::from_secs(15))?;
        driver.send("3")?;
        driver.wait_for_contains(
            "Permissions updated to Full Access",
            Duration::from_secs(15),
        )?;

        wait_for_log_contains(
            &env.debug_dir,
            "danger-full-access",
            Duration::from_secs(20),
        )?;
        wait_for_log_contains(&env.debug_dir, "tool-call-meta", Duration::from_secs(20))?;

        driver.send_line(&format!(
            "{FULL_ACCESS_MARKER}: probe write after full access"
        ))?;
        driver.wait_for_contains("WRITE_OK", Duration::from_secs(20))?;
        driver.wait_for_contains("Tool call 2 completed", Duration::from_secs(20))?;
        driver.kill()?;

        let outputs = mock_server.function_call_outputs().await;
        assert!(
            outputs.iter().any(|out| out.contains("WRITE_ERROR:")),
            "expected pre-full-access tool call to fail outside workspace: {outputs:?}"
        );
        assert!(
            outputs.iter().any(|out| out.contains("WRITE_OK")),
            "expected post-full-access tool call to succeed outside workspace: {outputs:?}"
        );
        Ok(())
    }

    pub(super) async fn run_mock_rejects_malformed_responses_payload() -> TestResult<()> {
        let tool_args = tool_args_for_code(&sandbox_run_code());
        let mock_server =
            MockResponsesServer::start(tool_name(), tool_args.clone(), Some(tool_args)).await?;

        let mut stream = TcpStream::connect(mock_server.addr).await?;
        let payload = b"not-json";
        let request = format!(
            "POST /v1/responses HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            payload.len()
        );
        stream.write_all(request.as_bytes()).await?;
        stream.write_all(payload).await?;
        stream.shutdown().await?;

        let mut response_bytes = Vec::new();
        stream.read_to_end(&mut response_bytes).await?;
        let response = String::from_utf8(response_bytes)
            .map_err(|err| format!("malformed-response test got non-UTF8 bytes: {err}"))?;
        assert!(
            response.starts_with("HTTP/1.1 400 Bad Request"),
            "expected 400 for malformed /responses payload, got:\n{response}"
        );

        let last_request = mock_server.last_request().await;
        assert!(
            last_request
                .as_ref()
                .and_then(|request| request.get("parse_error"))
                .and_then(Value::as_str)
                .is_some(),
            "expected parse_error marker in mock request log, got: {last_request:?}"
        );
        Ok(())
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn full_access_test_enabled() -> bool {
        std::env::var_os(FULL_ACCESS_TEST_ENV).is_some()
    }

    async fn loopback_bind_available() -> bool {
        TcpListener::bind("127.0.0.1:0").await.is_ok()
    }

    #[cfg(target_os = "linux")]
    fn codex_exec_bwrap_unavailable(text: &str) -> bool {
        text.contains("Failed RTM_NEWADDR")
            || text.contains("Failed RTM_NEWLINK")
            || text.contains("setting up uid map: Permission denied")
            || text.contains("No permissions to create a new namespace")
            || (text.contains("loopback") && text.contains("Operation not permitted"))
    }

    fn codex_exec_backend_unavailable(text: &str) -> bool {
        text.contains("Fatal error: cannot create 'R_TempDir'")
            || text.contains("failed to start R session")
            || text.contains("worker exited with signal")
            || text.contains("worker exited with status")
            || text.contains("worker io error: Broken pipe")
            || text.contains("unable to initialize the JIT")
            || text.contains("libR.so: cannot open shared object file")
            || text.contains("options(\"defaultPackages\") was not found")
            || text
                .contains("worker protocol error: ipc disconnected while waiting for backend info")
            || text.contains(
                "worker protocol error: ipc disconnected while waiting for request completion",
            )
    }

    fn codex_exec_environment_unavailable(stdout: &str, stderr: &str, outputs: &[String]) -> bool {
        let mut combined = String::with_capacity(
            stdout.len() + stderr.len() + outputs.iter().map(String::len).sum::<usize>(),
        );
        combined.push_str(stdout);
        combined.push_str(stderr);
        for output in outputs {
            combined.push_str(output);
        }

        #[cfg(target_os = "linux")]
        if codex_exec_bwrap_unavailable(&combined) {
            return true;
        }

        codex_exec_backend_unavailable(&combined)
    }

    fn codex_available() -> bool {
        std::process::Command::new("codex")
            .arg("--version")
            .output()
            .is_ok()
    }

    fn create_isolated_codex_env_with_config(
        build_config: impl FnOnce(&Path) -> String,
    ) -> TestResult<IsolatedCodexEnv> {
        let temp_dir = tempfile::tempdir()?;
        let workspace = temp_dir.path().join("workspace");
        std::fs::create_dir_all(&workspace)?;
        let git_status = std::process::Command::new("git")
            .arg("init")
            .arg("--quiet")
            .current_dir(&workspace)
            .status()?;
        if !git_status.success() {
            return Err(
                format!("failed to initialize temp workspace git repo: {git_status}").into(),
            );
        }

        let codex_home = temp_dir.path().join("codex-home");
        std::fs::create_dir_all(codex_home.join("shell_snapshots"))?;

        let debug_dir = temp_dir.path().join("debug");
        std::fs::create_dir_all(&debug_dir)?;

        let config = build_config(&workspace);
        std::fs::write(codex_home.join("config.toml"), config)?;

        Ok(IsolatedCodexEnv {
            _temp_dir: temp_dir,
            workspace,
            codex_home,
            debug_dir,
        })
    }

    fn create_isolated_codex_env(
        mcp_repl: &Path,
        openai_base_url: &str,
    ) -> TestResult<IsolatedCodexEnv> {
        create_isolated_codex_env_with_config(|workspace| {
            codex_config(mcp_repl, workspace, openai_base_url)
        })
    }

    fn create_isolated_codex_env_for_install(
        openai_base_url: &str,
    ) -> TestResult<IsolatedCodexEnv> {
        create_isolated_codex_env_with_config(|workspace| {
            codex_install_base_config(workspace, openai_base_url)
        })
    }

    fn codex_exec_sandbox_mode() -> &'static str {
        #[cfg(target_os = "windows")]
        {
            "danger-full-access"
        }
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            "workspace-write"
        }
    }

    fn codex_exec_expected_sandbox_log() -> &'static str {
        codex_exec_sandbox_mode()
    }

    fn codex_exec_command(
        env: &IsolatedCodexEnv,
        base_url: &str,
        prompt: &str,
        sandbox_mode: &str,
        mode_flag: Option<&str>,
    ) -> std::process::Command {
        let mut cmd = std::process::Command::new("codex");
        cmd.env("CODEX_HOME", env.codex_home.display().to_string());
        cmd.env("CODEX_OSS_BASE_URL", base_url);
        cmd.env("MCP_REPL_DEBUG_DIR", env.debug_dir.display().to_string());
        cmd.env("TERM", "xterm-256color");
        cmd.env("LANG", "C");
        cmd.arg("exec");
        if let Some(flag) = mode_flag {
            cmd.arg(flag);
        }
        cmd.arg("--sandbox");
        cmd.arg(sandbox_mode);
        cmd.arg("--skip-git-repo-check");
        cmd.arg("--cd");
        cmd.arg(&env.workspace);
        cmd.arg(prompt);
        cmd.current_dir(&env.workspace);
        cmd
    }

    fn run_command_with_timeout(
        mut cmd: std::process::Command,
        timeout: Duration,
    ) -> TestResult<std::process::Output> {
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        cmd.process_group(0);
        let mut child = cmd.spawn()?;
        let mut stdout_reader = child
            .stdout
            .take()
            .ok_or_else(|| "failed to capture codex exec stdout".to_string())?;
        let mut stderr_reader = child
            .stderr
            .take()
            .ok_or_else(|| "failed to capture codex exec stderr".to_string())?;

        let (stdout_tx, stdout_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stdout_reader.read_to_end(&mut buf);
            let _ = stdout_tx.send(buf);
        });
        let (stderr_tx, stderr_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stderr_reader.read_to_end(&mut buf);
            let _ = stderr_tx.send(buf);
        });

        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                #[cfg(any(target_os = "macos", target_os = "linux"))]
                {
                    let pid = child.id() as i32;
                    // Ensure the timeout path tears down the whole subtree (codex + children).
                    unsafe {
                        libc::killpg(pid, libc::SIGKILL);
                    }
                }
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "timed out waiting for codex exec to finish after {}s",
                    timeout.as_secs()
                )
                .into());
            }
            std::thread::sleep(Duration::from_millis(20));
        };

        let stdout = stdout_rx
            .recv_timeout(Duration::from_secs(1))
            .map_err(|_| "timed out collecting codex exec stdout".to_string())?;
        let stderr = stderr_rx
            .recv_timeout(Duration::from_secs(1))
            .map_err(|_| "timed out collecting codex exec stderr".to_string())?;
        Ok(std::process::Output {
            status,
            stdout,
            stderr,
        })
    }

    fn render_exec_snapshot(
        mode: ExecSnapshotMode,
        stdout: &str,
        _stderr: &str,
        workspace: &Path,
        codex_home: &Path,
    ) -> TestResult<String> {
        let mut out = String::new();
        out.push_str(match mode {
            ExecSnapshotMode::Json => {
                "$ codex exec --json --sandbox workspace-write --skip-git-repo-check --cd <WORKSPACE> \"SANDBOX_TEST_1: run the sandbox write test\"\n"
            }
            ExecSnapshotMode::Plain => {
                "$ codex exec --sandbox workspace-write --skip-git-repo-check --cd <WORKSPACE> \"SANDBOX_TEST_1: run the sandbox write test\"\n"
            }
        });

        for line in stdout.lines() {
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                continue;
            }
            let normalized = normalize_exec_text(trimmed, workspace, codex_home);
            if normalized.is_empty() {
                continue;
            }
            out.push_str(&normalized);
            out.push('\n');
        }

        Ok(renumber_exec_item_ids(out.trim_end()))
    }

    fn normalize_exec_text(text: &str, workspace: &Path, codex_home: &Path) -> String {
        if text.contains("WARN codex_core::shell_snapshot: Failed to delete shell snapshot") {
            return String::new();
        }
        if text == "Reading additional input from stdin..." {
            return String::new();
        }
        if text.contains(
            "WARNING: proceeding, even though we could not update PATH: Refusing to create helper binaries under temporary dir ",
        ) {
            return String::new();
        }
        if text.contains(
            "Codex's Linux sandbox uses bubblewrap and needs access to create user namespaces.",
        ) {
            return String::new();
        }
        let workspace_display = workspace.display().to_string();
        let codex_home_display = codex_home.display().to_string();
        let workspace_private = format!("/private{workspace_display}");
        let codex_home_private = format!("/private{codex_home_display}");

        let mut text = text.to_string();
        for (needle, replacement) in [
            (&workspace_private, "<WORKSPACE>"),
            (&workspace_display, "<WORKSPACE>"),
            (&codex_home_private, "<CODEX_HOME>"),
            (&codex_home_display, "<CODEX_HOME>"),
        ] {
            text = text.replace(needle, replacement);
        }

        let mut text = normalize_temp_paths(&normalize_codex_home_path(&text));
        text = normalize_loopback_websocket_url(&text);
        text = normalize_json_string_field(&text, "thread_id", "<THREAD_ID>");
        text = normalize_json_number_field(&text, "input_tokens", "\"<N>\"");
        text = normalize_json_number_field(&text, "cached_input_tokens", "\"<N>\"");
        text = normalize_json_number_field(&text, "output_tokens", "\"<N>\"");
        text = normalize_ms_duration(&text);
        if text.starts_with("OpenAI Codex v") {
            text = "OpenAI Codex vN.NN.N (research preview)".to_string();
        }
        if text.starts_with("session id: ") {
            text = "session id: <SESSION_ID>".to_string();
        }
        if text.starts_with("workdir: ") {
            text = "workdir: <WORKSPACE>".to_string();
        }
        if text.starts_with("provider: mock-openai") {
            text = "provider: openai".to_string();
        }
        if text.chars().all(|ch| ch.is_ascii_digit() || ch == ',') {
            text = "<N>".to_string();
        }
        if text.starts_with("202") {
            if let Some(pos) = text.find(" WARN ") {
                text = format!("<TIMESTAMP>{}", &text[pos..]);
            } else if let Some(pos) = text.find(" ERROR ") {
                text = format!("<TIMESTAMP>{}", &text[pos..]);
            }
        }
        text
    }

    fn renumber_exec_item_ids(text: &str) -> String {
        let marker = r#""id":"item_"#;
        let mut ids = BTreeMap::new();
        let mut next_id = 0usize;
        let mut out = String::with_capacity(text.len());
        let mut idx = 0usize;

        while let Some(pos) = text[idx..].find(marker) {
            let start = idx + pos;
            let digits_start = start + marker.len();
            let mut digits_end = digits_start;
            while digits_end < text.len() && text.as_bytes()[digits_end].is_ascii_digit() {
                digits_end += 1;
            }
            if digits_end == digits_start {
                out.push_str(&text[idx..digits_start]);
                idx = digits_start;
                continue;
            }

            out.push_str(&text[idx..digits_start]);
            let original = &text[digits_start..digits_end];
            let canonical = ids.entry(original.to_string()).or_insert_with(|| {
                let current = next_id;
                next_id += 1;
                current.to_string()
            });
            out.push_str(canonical);
            idx = digits_end;
        }

        out.push_str(&text[idx..]);
        out
    }

    fn normalize_ms_duration(text: &str) -> String {
        let marker = " in ";
        let mut out = String::with_capacity(text.len());
        let mut idx = 0;
        while let Some(pos) = text[idx..].find(marker) {
            let abs = idx + pos;
            out.push_str(&text[idx..abs + marker.len()]);
            let mut end = abs + marker.len();
            while end < text.len() && text.as_bytes()[end].is_ascii_digit() {
                end += 1;
            }
            if end > abs + marker.len() && text[end..].starts_with("ms") {
                out.push('N');
                idx = end;
            } else {
                idx = abs + marker.len();
            }
        }
        out.push_str(&text[idx..]);
        out
    }

    fn normalize_loopback_websocket_url(text: &str) -> String {
        let marker = "ws://127.0.0.1:";
        let mut out = String::with_capacity(text.len());
        let mut idx = 0;
        while let Some(pos) = text[idx..].find(marker) {
            let start = idx + pos;
            out.push_str(&text[idx..start]);
            let mut end = start + marker.len();
            while end < text.len() && text.as_bytes()[end].is_ascii_digit() {
                end += 1;
            }
            if end > start + marker.len() && text[end..].starts_with("/v1/responses") {
                out.push_str("<WS_URL>");
                idx = end + "/v1/responses".len();
            } else {
                out.push_str(marker);
                idx = start + marker.len();
            }
        }
        out.push_str(&text[idx..]);
        out
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn sh_single_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }

    fn normalize_json_string_field(text: &str, key: &str, replacement: &str) -> String {
        let marker = format!("\"{key}\":\"");
        let mut out = String::with_capacity(text.len());
        let mut idx = 0;

        while let Some(pos) = text[idx..].find(&marker) {
            let start = idx + pos;
            out.push_str(&text[idx..start + marker.len()]);
            let mut end = start + marker.len();
            while end < text.len() {
                if text.as_bytes()[end] == b'"' {
                    break;
                }
                end += 1;
            }
            out.push_str(replacement);
            if end < text.len() {
                out.push('"');
                idx = end + 1;
            } else {
                idx = end;
            }
        }

        out.push_str(&text[idx..]);
        out
    }

    fn normalize_json_number_field(text: &str, key: &str, replacement: &str) -> String {
        let marker = format!("\"{key}\":");
        let mut out = String::with_capacity(text.len());
        let mut idx = 0;

        while let Some(pos) = text[idx..].find(&marker) {
            let start = idx + pos;
            out.push_str(&text[idx..start + marker.len()]);
            let mut end = start + marker.len();
            while end < text.len() && text.as_bytes()[end].is_ascii_digit() {
                end += 1;
            }
            if end > start + marker.len() {
                out.push_str(replacement);
                idx = end;
            } else {
                idx = start + marker.len();
            }
        }

        out.push_str(&text[idx..]);
        out
    }

    fn resolve_mcp_repl_path() -> TestResult<PathBuf> {
        if let Ok(path) = std::env::var("CARGO_BIN_EXE_mcp-repl") {
            return Ok(PathBuf::from(path));
        }

        let mut path = std::env::current_exe()?;
        path.pop();
        path.pop();
        {
            let candidate = "mcp-repl";
            let mut candidate_path = path.clone();
            candidate_path.push(candidate);
            if cfg!(windows) {
                candidate_path.set_extension("exe");
            }
            if candidate_path.exists() {
                return Ok(candidate_path);
            }
        }
        Err("unable to locate mcp-repl test binary".into())
    }

    fn run_mcp_repl_install_for_codex_r(mcp_repl: &Path, codex_home: &Path) -> TestResult<()> {
        let output = std::process::Command::new(mcp_repl)
            .arg("install")
            .arg("--client")
            .arg("codex")
            .arg("--interpreter")
            .arg("r")
            .env("CODEX_HOME", codex_home)
            .output()?;
        if output.status.success() {
            return Ok(());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "mcp-repl install --client codex --interpreter r failed with status {status}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            status = output.status
        )
        .into())
    }

    fn assert_codex_install_wrote_r_inherit_config(
        codex_home: &Path,
        mcp_repl: &Path,
    ) -> TestResult<()> {
        let config_path = codex_home.join("config.toml");
        let text = std::fs::read_to_string(&config_path)?;
        let doc = text.parse::<DocumentMut>()?;
        let expected_command = mcp_repl.to_string_lossy().to_string();
        assert_eq!(
            doc["mcp_servers"]["r"]["command"].as_str(),
            Some(expected_command.as_str()),
            "expected install to register the current mcp-repl executable"
        );
        let r_args = doc["mcp_servers"]["r"]["args"]
            .as_array()
            .ok_or_else(|| "expected mcp_servers.r.args array".to_string())?;
        let has_sandbox_inherit = r_args
            .iter()
            .zip(r_args.iter().skip(1))
            .any(|(a, b)| a.as_str() == Some("--sandbox") && b.as_str() == Some("inherit"));
        assert!(
            has_sandbox_inherit,
            "expected installed Codex config to include `--sandbox inherit`"
        );
        Ok(())
    }

    fn tool_name() -> String {
        "mcp__r__repl".to_string()
    }

    #[test]
    fn normalize_exec_text_drops_stdin_status_line() {
        let workspace = Path::new("/tmp/workspace");
        let codex_home = Path::new("/tmp/codex-home");
        let normalized = normalize_exec_text(
            "Reading additional input from stdin...",
            workspace,
            codex_home,
        );
        assert_eq!(normalized, "");
    }

    #[test]
    fn render_exec_snapshot_ignores_stderr() -> TestResult<()> {
        let workspace = Path::new("/tmp/workspace");
        let codex_home = Path::new("/tmp/codex-home");
        let snapshot = render_exec_snapshot(
            ExecSnapshotMode::Plain,
            "visible stdout\n",
            "warning on stderr\n",
            workspace,
            codex_home,
        )?;
        assert!(snapshot.contains("visible stdout"));
        assert!(!snapshot.contains("warning on stderr"));
        Ok(())
    }

    fn codex_config(mcp_repl: &Path, repo_root: &Path, openai_base_url: &str) -> String {
        let mcp_repl = toml_escape(&mcp_repl.display().to_string());
        let repo_root = toml_escape(&repo_root.display().to_string());
        let openai_base_url = toml_escape(openai_base_url);
        format!(
            r#"model_provider = "mock-openai"
disable_paste_burst = true
project_doc_max_bytes = 0

[model_providers.mock-openai]
name = "Mock OpenAI"
base_url = "{openai_base_url}"
wire_api = "responses"
requires_openai_auth = false
supports_websockets = false

[notice]
hide_full_access_warning = true

[tui]
alternate_screen = "never"
animations = false

[features]
steer = true
remote_models = true
responses_websockets = false

[mcp_servers.r]
command = "{mcp_repl}"
args = ["--sandbox", "inherit"]
env_vars = ["MCP_REPL_DEBUG_DIR"]
[projects."{repo_root}"]
trust_level = "trusted"
"#,
        )
    }

    fn codex_install_base_config(repo_root: &Path, openai_base_url: &str) -> String {
        let repo_root = toml_escape(&repo_root.display().to_string());
        let openai_base_url = toml_escape(openai_base_url);
        format!(
            r#"model_provider = "mock-openai"
disable_paste_burst = true
project_doc_max_bytes = 0

[model_providers.mock-openai]
name = "Mock OpenAI"
base_url = "{openai_base_url}"
wire_api = "responses"
requires_openai_auth = false
supports_websockets = false

[notice]
hide_full_access_warning = true

[tui]
alternate_screen = "never"
animations = false

[features]
steer = true
remote_models = true
responses_websockets = false

[projects."{repo_root}"]
trust_level = "trusted"
"#,
        )
    }

    fn toml_escape(value: &str) -> String {
        value.replace('\\', "\\\\").replace('"', "\\\"")
    }

    fn sandbox_run_code() -> String {
        "target <- tempfile(\"mcp-repl-codex\")\ntryCatch({\n  writeLines(\"ok\", target)\n  cat(\"WRITE_OK\\n\")\n  unlink(target)\n}, error = function(e) {\n  message(\"WRITE_ERROR:\", conditionMessage(e))\n})"
            .to_string()
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn outside_workspace_probe_code() -> TestResult<String> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let target = std::env::temp_dir().join(format!("mcp-repl-codex-probe-{nanos}.txt"));
        let target_literal = serde_json::to_string(&target.to_string_lossy().to_string())
            .map_err(|err| format!("failed to encode target path: {err}"))?;
        Ok(r#"target <- __TARGET__
tryCatch({
  writeLines("ok", target)
  cat("WRITE_OK\n")
  unlink(target)
}, error = function(e) {
  message("WRITE_ERROR:", conditionMessage(e))
})"#
        .replace("__TARGET__", &target_literal))
    }

    fn tool_args_for_code(r_code: &str) -> String {
        serde_json::json!({
            "input": format!("{r_code}\n"),
        })
        .to_string()
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn normalize_screen(screen: &str) -> String {
        fn is_prompt_line(line: &str) -> bool {
            line.as_bytes().starts_with(&[0xE2, 0x80, 0xBA])
        }

        fn normalize_codex_version(line: &str) -> String {
            let Some(start) = line.find("OpenAI Codex (v") else {
                return line.to_string();
            };
            let version_start = start + "OpenAI Codex (".len();
            let Some(version_end_rel) = line[version_start..].find(')') else {
                return line.to_string();
            };
            let version_end = version_start + version_end_rel;
            let mut out = line.to_string();
            let original_len = out.len();
            out.replace_range(version_start..version_end, "vN.NN.N");
            if out.len() < original_len {
                let pad = original_len - out.len();
                if let Some(border_idx) = out.rfind('│') {
                    out.insert_str(border_idx, &" ".repeat(pad));
                } else {
                    out.push_str(&" ".repeat(pad));
                }
            } else if out.len() > original_len {
                let mut excess = out.len() - original_len;
                if let Some(border_idx) = out.rfind('│') {
                    let mut start = border_idx;
                    let bytes = out.as_bytes();
                    while excess > 0 && start > 0 && bytes[start - 1] == b' ' {
                        start -= 1;
                        excess -= 1;
                    }
                    if start < border_idx {
                        out.replace_range(start..border_idx, "");
                    }
                }
            }
            out
        }

        fn is_horizontal_rule(line: &str) -> bool {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return false;
            }
            if trimmed.contains("Worked for ") {
                return true;
            }
            trimmed.chars().all(|ch| ch == '─')
        }

        let mut lines: Vec<String> = Vec::new();
        let mut skipping_underdev_warning = false;
        let mut skipping_wrapped_tool_args = false;
        for raw in screen.lines() {
            let line = raw.trim_end().to_string();

            if skipping_wrapped_tool_args {
                let trimmed = line.trim_start();
                let indent = line.len().saturating_sub(trimmed.len());
                if trimmed.is_empty() || indent >= 4 {
                    continue;
                }
                skipping_wrapped_tool_args = false;
            }

            if skipping_underdev_warning {
                if line.starts_with("  ") {
                    continue;
                }
                skipping_underdev_warning = false;
            }

            let trimmed = line.trim_start();
            if trimmed.starts_with("└ r.repl(") {
                // This line may wrap differently between Codex versions; keep the structure but
                // omit the unstable argument rendering.
                lines.push("  └ r.repl(<omitted>)".to_string());
                skipping_wrapped_tool_args = true;
                continue;
            }
            if trimmed.starts_with("⚠ Under-development features enabled:") {
                skipping_underdev_warning = true;
                continue;
            }

            let normalized_for_checks =
                normalize_temp_paths(&normalize_codex_home_path(&normalize_codex_version(&line)));
            let trimmed_normalized = normalized_for_checks.trim_start();
            if normalized_for_checks.starts_with("  ")
                && trimmed_normalized == "<CODEX_HOME>/config.toml."
            {
                continue;
            }

            if trimmed.starts_with("guides/") {
                continue;
            }
            if trimmed.contains("developers.openai.com/mcp") {
                continue;
            }
            if trimmed.contains("directory:") {
                continue;
            }
            if trimmed == "experimental!" {
                continue;
            }
            if trimmed.starts_with("Tip:") {
                continue;
            }
            if trimmed.starts_with("• Starting MCP servers") || trimmed.starts_with("• Working")
            {
                continue;
            }
            if trimmed.starts_with("⚠ MCP startup incomplete")
                || trimmed.starts_with("⚠ MCP client for `mcp-repl` failed to start")
            {
                continue;
            }
            if is_prompt_line(trimmed)
                && !trimmed.contains(WARMUP_MARKER)
                && !trimmed.contains(WORKSPACE_WRITE_MARKER)
                && !trimmed.contains(FULL_ACCESS_MARKER)
            {
                continue;
            }
            if is_horizontal_rule(trimmed) {
                continue;
            }
            if lines.is_empty() && trimmed.is_empty() {
                continue;
            }
            if lines.is_empty() && trimmed.starts_with("╰") {
                continue;
            }
            if lines.is_empty() && trimmed.contains(WARMUP_MARKER) {
                continue;
            }

            lines.push(normalize_temp_paths(&normalize_codex_home_path(
                &normalize_codex_version(&line),
            )));
        }

        // Codex output can vary a bit between versions (extra blank lines), so make the snapshot
        // resilient by collapsing consecutive empty lines.
        let mut collapsed: Vec<String> = Vec::with_capacity(lines.len());
        let mut previous_was_empty = false;
        for line in lines {
            let is_empty = line.trim().is_empty();
            if is_empty && previous_was_empty {
                continue;
            }
            previous_was_empty = is_empty;
            collapsed.push(line);
        }
        let mut lines = collapsed;

        if let Some(first_nonempty) = lines.iter().position(|line| !line.trim().is_empty())
            && first_nonempty > 0
        {
            lines.drain(0..first_nonempty);
        }

        while matches!(lines.last(), Some(line) if line.trim().is_empty()) {
            lines.pop();
        }

        if lines.len() >= 2 {
            let last_line = lines[lines.len() - 1].trim_start();
            if last_line.contains("context left") {
                lines.pop();
                if let Some(prev) = lines.last()
                    && is_prompt_line(prev)
                {
                    lines.pop();
                }
            }
        }

        if matches!(lines.last(), Some(line) if is_prompt_line(line)) {
            lines.pop();
        }

        let normalized: Vec<String> = lines.into_iter().map(|line| scrub_seconds(&line)).collect();
        normalized.join("\n").trim_end().to_string()
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn scrub_seconds(line: &str) -> String {
        let mut out = String::with_capacity(line.len());
        let mut chars = line.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch.is_ascii_digit() {
                let mut digits = String::new();
                digits.push(ch);
                while let Some(next) = chars.peek()
                    && next.is_ascii_digit()
                {
                    digits.push(*next);
                    chars.next();
                }
                if matches!(chars.peek(), Some('s')) {
                    chars.next();
                    out.push_str("Ns");
                } else {
                    out.push_str(&digits);
                }
            } else {
                out.push(ch);
            }
        }
        out
    }

    fn normalize_codex_home_path(line: &str) -> String {
        let needle = "codex-home/config.toml";
        let Some(path_end) = line.find(needle) else {
            return line.to_string();
        };

        let mut path_start = path_end;
        while path_start > 0 {
            let ch = line.as_bytes()[path_start - 1];
            if ch == b' ' {
                break;
            }
            path_start -= 1;
        }

        let mut normalized = line.to_string();
        let replacement = "<CODEX_HOME>/config.toml";
        normalized.replace_range(path_start..path_end + needle.len(), replacement);
        normalized
    }

    fn normalize_temp_paths(line: &str) -> String {
        let mut out = String::with_capacity(line.len());
        let mut idx = 0;
        let bytes = line.as_bytes();
        while let Some(pos) = line[idx..].find("/tmp/.tmp") {
            let abs_pos = idx + pos;
            out.push_str(&line[idx..abs_pos]);
            let mut end = abs_pos + "/tmp/.tmp".len();
            while end < bytes.len() {
                let ch = bytes[end];
                if ch.is_ascii_whitespace() || ch == b'/' {
                    break;
                }
                end += 1;
            }
            let mut had_slash = false;
            if end < bytes.len() && bytes[end] == b'/' {
                had_slash = true;
                end += 1;
            }
            out.push_str("/tmp/<TMP>");
            if had_slash {
                out.push('/');
            }
            idx = end;
        }
        out.push_str(&line[idx..]);
        out
    }

    fn latest_sandbox_log_path(debug_dir: &Path) -> Option<PathBuf> {
        let mut sessions = std::fs::read_dir(debug_dir)
            .ok()?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.is_dir())
            .collect::<Vec<_>>();
        sessions.sort();
        sessions
            .last()
            .map(|session| session.join("sandbox-state.jsonl"))
    }

    fn wait_for_log_contains(debug_dir: &Path, needle: &str, timeout: Duration) -> TestResult<()> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(path) = latest_sandbox_log_path(debug_dir)
                && let Ok(contents) = std::fs::read_to_string(&path)
                && contents.contains(needle)
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                let path = latest_sandbox_log_path(debug_dir)
                    .unwrap_or_else(|| debug_dir.join("<missing>/sandbox-state.jsonl"));
                let contents = std::fs::read_to_string(&path)
                    .map_err(|err| format!("sandbox log read failed: {err}"))
                    .unwrap_or_else(|err| err);
                return Err(format!(
                    "timeout waiting for sandbox log to contain {needle}\nlog path: {path:?}\nlog contents:\n{contents}"
                )
                .into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn detect_cursor_request(
        chunk: &[u8],
        carry: &mut Vec<u8>,
        writer: &Arc<Mutex<Box<dyn Write + Send>>>,
    ) {
        let mut data = Vec::with_capacity(carry.len() + chunk.len());
        data.extend_from_slice(carry);
        data.extend_from_slice(chunk);
        let mut idx = 0;
        while idx + 3 < data.len() {
            if data[idx..idx + 4] == [0x1b, 0x5b, 0x36, 0x6e]
                && let Ok(mut guard) = writer.lock()
            {
                let _ = guard.write_all(b"\x1b[1;1R");
                let _ = guard.flush();
            }
            idx += 1;
        }
        let keep = data.len().saturating_sub(3);
        carry.clear();
        carry.extend_from_slice(&data[keep..]);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    struct CodexPtyDriver {
        child: Box<dyn portable_pty::Child + Send>,
        writer: Arc<Mutex<Box<dyn Write + Send>>>,
        rx: Receiver<Vec<u8>>,
        parser: Parser,
        _master: Box<dyn portable_pty::MasterPty + Send>,
        _slave: Box<dyn portable_pty::SlavePty + Send>,
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    impl CodexPtyDriver {
        fn spawn(
            codex_home: &Path,
            workspace: &Path,
            base_url: &str,
            debug_dir: &Path,
        ) -> TestResult<Self> {
            let pty_system = native_pty_system();
            let pair = pty_system
                .openpty(PtySize {
                    rows: 32,
                    cols: 110,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|err| format!("openpty failed: {err}"))?;

            let mut cmd = CommandBuilder::new("sh");
            let shell_script = format!(
                "CODEX_HOME={} CODEX_OSS_BASE_URL={} MCP_REPL_DEBUG_DIR={} TERM=xterm-256color LANG=C codex --sandbox workspace-write --ask-for-approval on-request --cd {} {}",
                sh_single_quote(&codex_home.display().to_string()),
                sh_single_quote(base_url),
                sh_single_quote(&debug_dir.display().to_string()),
                sh_single_quote(&workspace.display().to_string()),
                sh_single_quote(WARMUP_MARKER),
            );
            cmd.arg("-c");
            cmd.arg(shell_script);

            let child = pair
                .slave
                .spawn_command(cmd)
                .map_err(|err| format!("spawn codex failed: {err}"))?;
            let reader = pair
                .master
                .try_clone_reader()
                .map_err(|err| format!("clone reader failed: {err}"))?;
            let writer = pair
                .master
                .take_writer()
                .map_err(|err| format!("take writer failed: {err}"))?;
            let writer = Arc::new(Mutex::new(writer));
            let reader_writer = Arc::clone(&writer);
            let (tx, rx) = std::sync::mpsc::channel();

            std::thread::spawn(move || {
                let mut reader = reader;
                let mut buf = [0u8; 8192];
                let mut carry = Vec::new();
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            detect_cursor_request(&buf[..n], &mut carry, &reader_writer);
                            if tx.send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                        Err(err) if err.kind() == ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        Err(_) => break,
                    }
                }
            });

            Ok(Self {
                child,
                writer,
                rx,
                parser: Parser::new(32, 110, 0),
                _master: pair.master,
                _slave: pair.slave,
            })
        }

        fn send(&mut self, text: &str) -> TestResult<()> {
            let mut writer = self.writer.lock().map_err(|_| "pty lock failed")?;
            writer
                .write_all(text.as_bytes())
                .map_err(|err| format!("pty write failed: {err}"))?;
            writer
                .flush()
                .map_err(|err| format!("pty flush failed: {err}"))?;
            Ok(())
        }

        fn send_line(&mut self, text: &str) -> TestResult<()> {
            self.send_slow(text, Duration::from_millis(12))
                .map_err(|err| format!("pty write line failed: {err}"))?;
            self.send("\r")
                .map_err(|err| format!("pty write enter failed: {err}"))?;
            Ok(())
        }

        fn send_slow(&mut self, text: &str, delay: Duration) -> TestResult<()> {
            for ch in text.chars() {
                let mut buf = [0u8; 4];
                let slice = ch.encode_utf8(&mut buf);
                self.send(slice)?;
                std::thread::sleep(delay);
            }
            Ok(())
        }

        fn drain(&mut self, duration: Duration) {
            let deadline = Instant::now() + duration;
            while Instant::now() < deadline {
                match self.rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(chunk) => self.parser.process(&chunk),
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        }

        fn wait_for_contains(&mut self, needle: &str, timeout: Duration) -> TestResult<()> {
            let ok = self.wait_for_screen(timeout, |screen| screen.contains(needle));
            if ok {
                Ok(())
            } else {
                Err(format!(
                    "timeout waiting for screen to contain {needle}\n{}",
                    normalize_screen(&self.snapshot_screen())
                )
                .into())
            }
        }

        fn wait_for_warmup(&mut self, timeout: Duration) -> TestResult<()> {
            let deadline = Instant::now() + timeout;
            let mut dismissed_upgrade = false;
            loop {
                self.drain(Duration::from_millis(100));
                let snapshot = self.snapshot_screen();
                if snapshot.contains("Warmup complete") {
                    return Ok(());
                }
                if !dismissed_upgrade
                    && (snapshot.contains("Codex just got an upgrade")
                        || snapshot.contains("Use existing model"))
                {
                    self.send("\u{1b}[B")?;
                    self.send("\r")?;
                    dismissed_upgrade = true;
                }
                if Instant::now() >= deadline {
                    return Err(format!(
                        "timeout waiting for screen to contain Warmup complete\n{}",
                        normalize_screen(&self.snapshot_screen())
                    )
                    .into());
                }
            }
        }

        fn wait_for_screen(
            &mut self,
            timeout: Duration,
            mut predicate: impl FnMut(&str) -> bool,
        ) -> bool {
            let deadline = Instant::now() + timeout;
            loop {
                self.drain(Duration::from_millis(100));
                let snapshot = self.snapshot_screen();
                if predicate(&snapshot) {
                    return true;
                }
                if Instant::now() >= deadline {
                    return false;
                }
            }
        }

        fn snapshot_screen(&self) -> String {
            self.parser.screen().contents()
        }

        fn ensure_running(&mut self, context: &str) -> TestResult<()> {
            if let Some(status) = self
                .child
                .try_wait()
                .map_err(|err| format!("codex wait failed: {err}"))?
            {
                return Err(format!(
                    "codex exited {context}: {status:?}\n{}",
                    normalize_screen(&self.snapshot_screen())
                )
                .into());
            }
            Ok(())
        }

        fn kill(mut self) -> TestResult<()> {
            let _ = self.child.kill();
            let _ = self.child.wait();
            Ok(())
        }
    }

    struct MockResponsesServer {
        addr: SocketAddr,
        state: Arc<tokio::sync::Mutex<MockState>>,
    }

    struct MockState {
        tool_name: String,
        workspace_write_tool_args: String,
        full_access_tool_args: Option<String>,
        first_user_turn_tool_args: Option<String>,
        requests: Vec<Value>,
        request_paths: Vec<String>,
        next_call_ordinal: usize,
        pending_call_id: Option<String>,
        pending_call_ordinal: Option<usize>,
    }

    impl MockResponsesServer {
        async fn start(
            tool_name: String,
            workspace_write_tool_args: String,
            full_access_tool_args: Option<String>,
        ) -> TestResult<Self> {
            Self::start_with_options(
                tool_name,
                workspace_write_tool_args,
                full_access_tool_args,
                None,
            )
            .await
        }

        async fn start_with_first_user_turn_tool_call(
            tool_name: String,
            first_user_turn_tool_args: String,
        ) -> TestResult<Self> {
            Self::start_with_options(
                tool_name,
                String::new(),
                None,
                Some(first_user_turn_tool_args),
            )
            .await
        }

        async fn start_with_options(
            tool_name: String,
            workspace_write_tool_args: String,
            full_access_tool_args: Option<String>,
            first_user_turn_tool_args: Option<String>,
        ) -> TestResult<Self> {
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let addr = listener.local_addr()?;
            let state = Arc::new(tokio::sync::Mutex::new(MockState {
                tool_name,
                workspace_write_tool_args,
                full_access_tool_args,
                first_user_turn_tool_args,
                requests: Vec::new(),
                request_paths: Vec::new(),
                next_call_ordinal: 1,
                pending_call_id: None,
                pending_call_ordinal: None,
            }));
            let state_clone = Arc::clone(&state);
            tokio::spawn(async move {
                loop {
                    let (socket, _) = match listener.accept().await {
                        Ok(pair) => pair,
                        Err(_) => break,
                    };
                    let state = Arc::clone(&state_clone);
                    tokio::spawn(async move {
                        let _ = handle_connection(socket, state).await;
                    });
                }
            });

            Ok(Self { addr, state })
        }

        fn base_url(&self) -> String {
            format!("http://{}/v1", self.addr)
        }

        async fn last_request(&self) -> Option<Value> {
            let state = self.state.lock().await;
            state.requests.last().cloned()
        }

        async fn request_paths(&self) -> Vec<String> {
            let state = self.state.lock().await;
            state.request_paths.clone()
        }

        async fn function_call_outputs(&self) -> Vec<String> {
            let state = self.state.lock().await;
            collect_function_call_outputs(&state.requests)
        }
    }

    async fn handle_connection(
        stream: TcpStream,
        state: Arc<tokio::sync::Mutex<MockState>>,
    ) -> std::io::Result<()> {
        let mut reader = BufReader::new(stream);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).await? == 0 {
            return Ok(());
        }
        let request_line = request_line.trim_end();
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("");
        let path = parts.next().unwrap_or("");

        let mut content_length = 0usize;
        let mut transfer_chunked = false;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).await?;
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((key, value)) = line.split_once(':')
                && key.eq_ignore_ascii_case("content-length")
            {
                content_length = value.trim().parse::<usize>().unwrap_or(0);
            }
            if let Some((key, value)) = line.split_once(':')
                && key.eq_ignore_ascii_case("transfer-encoding")
            {
                transfer_chunked = value
                    .split(',')
                    .any(|encoding| encoding.trim().eq_ignore_ascii_case("chunked"));
            }
        }

        let mut body = Vec::new();
        if transfer_chunked {
            loop {
                let mut size_line = String::new();
                reader.read_line(&mut size_line).await?;
                let size_line = size_line.trim_end();
                if size_line.is_empty() {
                    continue;
                }
                let size_hex = size_line.split(';').next().unwrap_or("0");
                let chunk_size = usize::from_str_radix(size_hex, 16).unwrap_or(0);
                if chunk_size == 0 {
                    loop {
                        let mut trailer = String::new();
                        reader.read_line(&mut trailer).await?;
                        if trailer.trim_end().is_empty() {
                            break;
                        }
                    }
                    break;
                }
                let mut chunk = vec![0u8; chunk_size];
                reader.read_exact(&mut chunk).await?;
                body.extend_from_slice(&chunk);
                let mut crlf = [0u8; 2];
                reader.read_exact(&mut crlf).await?;
            }
        } else if content_length > 0 {
            body.resize(content_length, 0u8);
            reader.read_exact(&mut body).await?;
        }

        {
            let mut locked = state.lock().await;
            locked.request_paths.push(format!("{method} {path}"));
        }

        let response_body = if method == "GET" && path.contains("/models") {
            models_response()
        } else if method == "POST" && path.contains("/responses") {
            match serde_json::from_slice::<Value>(&body) {
                Ok(body_json) => {
                    let mut locked = state.lock().await;
                    locked.requests.push(body_json.clone());
                    response_for_request(&body_json, &mut locked)
                }
                Err(err) => {
                    let mut locked = state.lock().await;
                    locked.requests.push(serde_json::json!({
                        "parse_error": err.to_string(),
                    }));
                    let body = serde_json::json!({
                        "error": format!("invalid /responses payload: {err}"),
                    })
                    .to_string();
                    let response = format!(
                        "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let stream = reader.get_mut();
                    stream.write_all(response.as_bytes()).await?;
                    stream.flush().await?;
                    return Ok(());
                }
            }
        } else {
            r#"{"error":"unsupported"}"#.to_string()
        };

        let is_event_stream = method == "POST" && path.contains("/responses");
        let content_type = if is_event_stream {
            "text/event-stream"
        } else {
            "application/json"
        };
        let status = if is_event_stream || path.contains("/models") {
            "HTTP/1.1 200 OK\r\n"
        } else {
            "HTTP/1.1 404 Not Found\r\n"
        };
        let response = format!(
            "{status}Content-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
            response_body.len()
        );
        let stream = reader.get_mut();
        stream.write_all(response.as_bytes()).await?;
        stream.flush().await?;
        Ok(())
    }

    fn models_response() -> String {
        serde_json::json!({
            "models": [{
                "slug": "gpt-5.1-codex-mini",
                "display_name": "gpt-5.1-codex-mini",
                "description": "mock model",
                "default_reasoning_level": "low",
                "supported_reasoning_levels": [{"effort": "low", "description": "low"}],
                "shell_type": "shell_command",
                "visibility": "list",
                "supported_in_api": true,
                "priority": 1,
                "upgrade": null,
                "base_instructions": "test instructions",
                "model_instructions_template": null,
                "supports_reasoning_summaries": false,
                "support_verbosity": false,
                "default_verbosity": null,
                "apply_patch_tool_type": null,
                "truncation_policy": {"mode": "bytes", "limit": 10000},
                "supports_parallel_tool_calls": false,
                "context_window": 8000,
                "auto_compact_token_limit": null,
                "effective_context_window_percent": 95,
                "experimental_supported_tools": [],
            }]
        })
        .to_string()
    }

    fn response_for_request(body: &Value, state: &mut MockState) -> String {
        if has_user_marker(body, WARMUP_MARKER) {
            return response_body_with_items(vec![message_item("Warmup complete")], "resp-warmup");
        }

        if let Some(call_id) = state.pending_call_id.as_deref()
            && has_function_call_output(body, call_id)
        {
            let ordinal = state.pending_call_ordinal.take().unwrap_or_default();
            state.pending_call_id = None;
            return response_body_with_items(
                vec![message_item(&format!("Tool call {ordinal} completed"))],
                &format!("resp-tool-{ordinal}"),
            );
        }

        if has_user_message(body)
            && let Some(tool_args) = state.first_user_turn_tool_args.take()
        {
            return queue_tool_call(body, state, tool_args);
        }

        if has_user_marker(body, WORKSPACE_WRITE_MARKER) {
            return queue_tool_call(body, state, state.workspace_write_tool_args.clone());
        }
        if has_user_marker(body, FULL_ACCESS_MARKER)
            && let Some(tool_args) = state.full_access_tool_args.clone()
        {
            return queue_tool_call(body, state, tool_args);
        }
        response_body_with_items(vec![message_item("ready")], "resp-ready")
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct MockToolCallSpec {
        name: String,
        namespace: Option<String>,
    }

    fn queue_tool_call(request: &Value, state: &mut MockState, tool_args: String) -> String {
        let ordinal = state.next_call_ordinal;
        state.next_call_ordinal += 1;
        let call_id = format!("call-{ordinal}");
        state.pending_call_id = Some(call_id.clone());
        state.pending_call_ordinal = Some(ordinal);
        let tool_call =
            resolve_tool_call_spec(request, &state.tool_name).unwrap_or_else(|| MockToolCallSpec {
                name: state.tool_name.clone(),
                namespace: None,
            });
        response_body_with_items(
            vec![function_call_item(&tool_call, &call_id, &tool_args)],
            &format!("resp-call-{ordinal}"),
        )
    }

    fn response_body_with_items(items: Vec<Value>, response_id: &str) -> String {
        let mut body = String::new();
        for item in items {
            let event = serde_json::json!({
                "type": "response.output_item.done",
                "item": item,
            });
            body.push_str(&format!("data: {event}\n\n"));
        }
        let completed = serde_json::json!({
            "type": "response.completed",
            "response": { "id": response_id },
        });
        body.push_str(&format!("data: {completed}\n\n"));
        body
    }

    fn function_call_item(tool: &MockToolCallSpec, call_id: &str, args: &str) -> Value {
        let mut item = serde_json::json!({
            "type": "function_call",
            "name": tool.name,
            "arguments": args,
            "call_id": call_id,
        });
        if let Some(namespace) = &tool.namespace {
            item["namespace"] = serde_json::Value::String(namespace.clone());
        }
        item
    }

    fn resolve_tool_call_spec(request: &Value, legacy_tool_name: &str) -> Option<MockToolCallSpec> {
        let (namespace, name) = split_legacy_tool_name(legacy_tool_name)?;
        let tools = request.get("tools")?.as_array()?;
        let namespaced_tool_present = tools.iter().any(|tool| {
            tool.get("type").and_then(Value::as_str) == Some("namespace")
                && tool.get("name").and_then(Value::as_str) == Some(namespace)
                && tool
                    .get("tools")
                    .and_then(Value::as_array)
                    .is_some_and(|children| {
                        children.iter().any(|child| {
                            child.get("type").and_then(Value::as_str) == Some("function")
                                && child.get("name").and_then(Value::as_str) == Some(name)
                        })
                    })
        });
        namespaced_tool_present.then(|| MockToolCallSpec {
            name: name.to_string(),
            namespace: Some(namespace.to_string()),
        })
    }

    fn split_legacy_tool_name(legacy_tool_name: &str) -> Option<(&str, &str)> {
        let split = legacy_tool_name.rfind("__")?;
        let namespace_end = split + 2;
        (namespace_end < legacy_tool_name.len()).then(|| {
            (
                &legacy_tool_name[..namespace_end],
                &legacy_tool_name[namespace_end..],
            )
        })
    }

    fn message_item(text: &str) -> Value {
        serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": text,
            }],
        })
    }

    fn has_user_marker(body: &Value, marker: &str) -> bool {
        let Some(items) = body.get("input").and_then(Value::as_array) else {
            return false;
        };
        let Some(item) = items.iter().rev().find(|item| {
            item.get("type").and_then(Value::as_str) == Some("message")
                && item.get("role").and_then(Value::as_str) == Some("user")
        }) else {
            return false;
        };
        item.get("content")
            .and_then(Value::as_array)
            .map(|content| {
                content.iter().any(|part| {
                    part.get("type").and_then(Value::as_str) == Some("input_text")
                        && part
                            .get("text")
                            .and_then(Value::as_str)
                            .map(|text| text.contains(marker))
                            .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    }

    fn has_user_message(body: &Value) -> bool {
        let Some(items) = body.get("input").and_then(Value::as_array) else {
            return false;
        };
        items.iter().rev().any(|item| {
            item.get("type").and_then(Value::as_str) == Some("message")
                && item.get("role").and_then(Value::as_str) == Some("user")
        })
    }

    fn has_function_call_output(body: &Value, call_id: &str) -> bool {
        let Some(items) = body.get("input").and_then(Value::as_array) else {
            return false;
        };
        items.iter().any(|item| {
            item.get("type").and_then(Value::as_str) == Some("function_call_output")
                && item.get("call_id").and_then(Value::as_str) == Some(call_id)
        })
    }

    fn collect_function_call_outputs(requests: &[Value]) -> Vec<String> {
        let mut outputs = Vec::new();
        for request in requests {
            let Some(items) = request.get("input").and_then(Value::as_array) else {
                continue;
            };
            for item in items {
                if item.get("type").and_then(Value::as_str) != Some("function_call_output") {
                    continue;
                }
                if let Some(output) = item.get("output") {
                    if let Some(text) = output.as_str() {
                        outputs.push(text.to_string());
                    } else if let Some(content) = output.get("content").and_then(Value::as_str) {
                        outputs.push(content.to_string());
                    }
                }
            }
        }
        outputs
    }

    fn assert_exec_output_contains_tool_call(
        stdout: &str,
        server: &str,
        tool: &str,
        input: &str,
    ) -> TestResult<()> {
        let mut saw_json = false;
        for line in stdout.lines() {
            let trimmed = line.trim();
            if !trimmed.starts_with('{') {
                continue;
            }
            saw_json = true;
            let Ok(event) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            let Some(item) = event.get("item") else {
                continue;
            };
            if item.get("type").and_then(Value::as_str) != Some("mcp_tool_call") {
                continue;
            }
            let Some(arguments) = item.get("arguments") else {
                continue;
            };
            if item.get("server").and_then(Value::as_str) == Some(server)
                && item.get("tool").and_then(Value::as_str) == Some(tool)
                && arguments.get("input").and_then(Value::as_str) == Some(input)
            {
                return Ok(());
            }
        }

        let json_note = if saw_json {
            "saw json events, but not the expected mcp tool call"
        } else {
            "stdout did not contain json events"
        };
        Err(format!(
            "expected codex exec output to include {server}.{tool} with input {input:?}; {json_note}\nstdout:\n{stdout}"
        )
        .into())
    }

    #[test]
    fn resolve_tool_call_spec_prefers_namespace_shape_when_present() {
        let request = serde_json::json!({
            "tools": [{
                "type": "namespace",
                "name": "mcp__r__",
                "tools": [{
                    "type": "function",
                    "name": "repl",
                }],
            }],
        });
        assert_eq!(
            resolve_tool_call_spec(&request, "mcp__r__repl"),
            Some(MockToolCallSpec {
                name: "repl".to_string(),
                namespace: Some("mcp__r__".to_string()),
            })
        );
    }

    #[test]
    fn resolve_tool_call_spec_falls_back_when_namespace_shape_absent() {
        let request = serde_json::json!({
            "tools": [{
                "type": "function",
                "name": "mcp__r__repl",
            }],
        });
        assert_eq!(resolve_tool_call_spec(&request, "mcp__r__repl"), None);
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::TestResult;

    #[tokio::test(flavor = "multi_thread")]
    async fn codex_exec_initial_sandbox_state() -> TestResult<()> {
        let snapshot = super::unix_impl::run_codex_exec_initial_sandbox_state().await?;
        if snapshot.is_empty() {
            return Ok(());
        }
        insta::assert_snapshot!("codex_exec_initial_sandbox_state", snapshot);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn codex_exec_initial_sandbox_state_plain() -> TestResult<()> {
        let snapshot = super::unix_impl::run_codex_exec_initial_sandbox_state_plain().await?;
        if snapshot.is_empty() {
            return Ok(());
        }
        insta::assert_snapshot!("codex_exec_initial_sandbox_state_plain", snapshot);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn install_then_codex_exec_uses_generated_config() -> TestResult<()> {
        super::unix_impl::run_install_then_codex_exec_uses_generated_config().await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn codex_tui_full_access_sandbox_update() -> TestResult<()> {
        super::unix_impl::run_codex_tui_full_access_sandbox_update().await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mock_rejects_malformed_responses_payload() -> TestResult<()> {
        super::unix_impl::run_mock_rejects_malformed_responses_payload().await
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::TestResult;

    #[tokio::test(flavor = "multi_thread")]
    async fn codex_exec_initial_sandbox_state() -> TestResult<()> {
        let snapshot = super::unix_impl::run_codex_exec_initial_sandbox_state().await?;
        if snapshot.is_empty() {
            return Ok(());
        }
        insta::assert_snapshot!("codex_exec_initial_sandbox_state", snapshot);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn codex_exec_initial_sandbox_state_plain() -> TestResult<()> {
        let snapshot = super::unix_impl::run_codex_exec_initial_sandbox_state_plain().await?;
        if snapshot.is_empty() {
            return Ok(());
        }
        insta::assert_snapshot!("codex_exec_initial_sandbox_state_plain", snapshot);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn install_then_codex_exec_uses_generated_config() -> TestResult<()> {
        super::unix_impl::run_install_then_codex_exec_uses_generated_config().await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn codex_tui_full_access_sandbox_update() -> TestResult<()> {
        super::unix_impl::run_codex_tui_full_access_sandbox_update().await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mock_rejects_malformed_responses_payload() -> TestResult<()> {
        super::unix_impl::run_mock_rejects_malformed_responses_payload().await
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use super::TestResult;

    #[tokio::test(flavor = "multi_thread")]
    async fn codex_exec_initial_sandbox_state() -> TestResult<()> {
        let snapshot = super::unix_impl::run_codex_exec_initial_sandbox_state().await?;
        if snapshot.is_empty() {
            return Ok(());
        }
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn install_then_codex_exec_uses_generated_config() -> TestResult<()> {
        super::unix_impl::run_install_then_codex_exec_uses_generated_config().await
    }
}
