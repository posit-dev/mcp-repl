use std::env;
use std::io::{self, BufRead, Write};
use std::thread;
use std::time::Duration;
use std::time::Instant;

use rmcp::model::{CallToolResult, RawContent};

use crate::backend::Backend;
use crate::oversized_output::OversizedOutputMode;
use crate::sandbox_cli::SandboxCliPlan;
use crate::server::response::{
    ResponseState, TimeoutBundleReuse, text_stream_from_content, timeout_bundle_reuse_for_input,
};
use crate::worker_process::{WorkerError, WorkerManager};
use crate::worker_protocol::{TextStream, WorkerContent, WorkerErrorCode, WorkerReply};

const DEFAULT_WRITE_STDIN_TIMEOUT: Duration = Duration::from_secs(60);
const SAFETY_MARGIN: f64 = 1.05;
const MIN_SERVER_GRACE: Duration = Duration::from_secs(1);
const INITIAL_PROMPT_WAIT: Duration = Duration::from_secs(5);
const INITIAL_PROMPT_POLL_INTERVAL: Duration = Duration::from_millis(50);

struct VisibleReplyContext {
    pending_request_after: bool,
    detached_prefix_item_count: usize,
    timeout_bundle_reuse: TimeoutBundleReuse,
}

pub(crate) fn run(
    backend: Backend,
    sandbox_plan: SandboxCliPlan,
    oversized_output: OversizedOutputMode,
) -> Result<(), Box<dyn std::error::Error>> {
    let image_support = detect_image_support();
    eprintln!(
        "debug repl: write_stdin timeout={:.1}s | end input with END | commands: INTERRUPT, RESTART | Ctrl-D to exit | images={}",
        DEFAULT_WRITE_STDIN_TIMEOUT.as_secs_f64(),
        if image_support { "kitty" } else { "off" }
    );

    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    let server_timeout = apply_safety_margin(DEFAULT_WRITE_STDIN_TIMEOUT);

    let mut worker = WorkerManager::new(backend, sandbox_plan, oversized_output)?;
    let mut response = if oversized_output == OversizedOutputMode::Files {
        Some(ResponseState::new()?)
    } else {
        None
    };
    worker.warm_start()?;
    let reply = wait_for_initial_prompt(&mut worker, server_timeout)?;
    render_visible_reply(
        response.as_mut(),
        Ok(reply),
        VisibleReplyContext {
            pending_request_after: worker.pending_request(),
            detached_prefix_item_count: worker.detached_prefix_item_count(),
            timeout_bundle_reuse: TimeoutBundleReuse::FullReply,
        },
        &mut stdout,
        &mut stderr,
        image_support,
    )?;

    let stdin = io::stdin();
    let mut stdin = stdin.lock();

    loop {
        let Some(line) = read_line(&mut stdin)? else {
            break;
        };

        if is_exact_command(&line, "INTERRUPT") {
            let reply = worker.interrupt(DEFAULT_WRITE_STDIN_TIMEOUT);
            render_visible_reply(
                response.as_mut(),
                reply,
                VisibleReplyContext {
                    pending_request_after: worker.pending_request(),
                    detached_prefix_item_count: 0,
                    timeout_bundle_reuse: timeout_bundle_reuse_for_exact_command("INTERRUPT"),
                },
                &mut stdout,
                &mut stderr,
                image_support,
            )?;
            continue;
        }
        if is_exact_command(&line, "RESTART") {
            let reply = worker.restart(DEFAULT_WRITE_STDIN_TIMEOUT);
            render_visible_reply(
                response.as_mut(),
                reply,
                VisibleReplyContext {
                    pending_request_after: worker.pending_request(),
                    detached_prefix_item_count: 0,
                    timeout_bundle_reuse: timeout_bundle_reuse_for_exact_command("RESTART"),
                },
                &mut stdout,
                &mut stderr,
                image_support,
            )?;
            continue;
        }
        if is_exact_command(&line, "END") {
            let reply = worker.write_stdin(
                String::new(),
                DEFAULT_WRITE_STDIN_TIMEOUT,
                server_timeout,
                None,
                false,
            );
            render_visible_reply(
                response.as_mut(),
                reply,
                VisibleReplyContext {
                    pending_request_after: worker.pending_request(),
                    detached_prefix_item_count: worker.detached_prefix_item_count(),
                    timeout_bundle_reuse: TimeoutBundleReuse::FullReply,
                },
                &mut stdout,
                &mut stderr,
                image_support,
            )?;
            continue;
        }

        let (chunk, done) = split_end_marker(&line);
        let mut input = chunk;
        if !done {
            loop {
                let Some(next) = read_line(&mut stdin)? else {
                    return Err("EOF reached while reading input; expected END".into());
                };
                let (chunk, done) = split_end_marker(&next);
                input.push_str(&chunk);
                if done {
                    break;
                }
            }
        }

        let timeout_bundle_reuse = timeout_bundle_reuse_for_input(&input);
        let reply = worker.write_stdin(
            input,
            DEFAULT_WRITE_STDIN_TIMEOUT,
            server_timeout,
            None,
            false,
        );
        render_visible_reply(
            response.as_mut(),
            reply,
            VisibleReplyContext {
                pending_request_after: worker.pending_request(),
                detached_prefix_item_count: worker.detached_prefix_item_count(),
                timeout_bundle_reuse,
            },
            &mut stdout,
            &mut stderr,
            image_support,
        )?;
    }

    if let Some(response) = response.as_mut() {
        response.shutdown()?;
    }

    Ok(())
}

fn wait_for_initial_prompt(
    worker: &mut WorkerManager,
    server_timeout: Duration,
) -> Result<WorkerReply, WorkerError> {
    let deadline = Instant::now() + INITIAL_PROMPT_WAIT;
    let mut last_reply = worker.write_stdin(
        String::new(),
        DEFAULT_WRITE_STDIN_TIMEOUT,
        server_timeout,
        None,
        false,
    )?;
    while !reply_has_prompt(&last_reply) && Instant::now() < deadline {
        thread::sleep(INITIAL_PROMPT_POLL_INTERVAL);
        last_reply = worker.write_stdin(
            String::new(),
            DEFAULT_WRITE_STDIN_TIMEOUT,
            server_timeout,
            None,
            false,
        )?;
    }
    Ok(last_reply)
}

fn reply_has_prompt(reply: &WorkerReply) -> bool {
    match reply {
        WorkerReply::Output { prompt, .. } => prompt
            .as_ref()
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false),
    }
}

fn read_line(reader: &mut impl BufRead) -> Result<Option<String>, WorkerError> {
    let mut line = String::new();
    let bytes = reader.read_line(&mut line).map_err(WorkerError::Io)?;
    if bytes == 0 {
        return Ok(None);
    }
    Ok(Some(line))
}

fn is_exact_command(line: &str, command: &str) -> bool {
    let trimmed = line.trim_end_matches(['\n', '\r']);
    trimmed == command
}

fn timeout_bundle_reuse_for_exact_command(command: &str) -> TimeoutBundleReuse {
    match command {
        "INTERRUPT" => TimeoutBundleReuse::FullReply,
        "RESTART" => TimeoutBundleReuse::None,
        _ => unreachable!("unsupported exact debug repl command: {command}"),
    }
}

fn split_end_marker(line: &str) -> (String, bool) {
    let (body, _newline) = split_line_ending(line);
    if let Some(prefix) = body.strip_suffix("END") {
        return (prefix.to_string(), true);
    }
    (line.to_string(), false)
}

fn split_line_ending(line: &str) -> (&str, &str) {
    if let Some(stripped) = line.strip_suffix("\r\n") {
        (stripped, "\n")
    } else if let Some(stripped) = line.strip_suffix('\n') {
        (stripped, "\n")
    } else {
        (line, "")
    }
}

fn apply_safety_margin(duration: Duration) -> Duration {
    let scaled = Duration::from_secs_f64(duration.as_secs_f64() * SAFETY_MARGIN);
    let min = duration.saturating_add(MIN_SERVER_GRACE);
    if scaled < min { min } else { scaled }
}

fn render_reply(
    reply: WorkerReply,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
    image_support: bool,
) -> io::Result<()> {
    match reply {
        WorkerReply::Output {
            contents,
            is_error,
            error_code,
            ..
        } => {
            if let Some(code) = error_code {
                writeln!(stderr, "[repl] error: {code:?}")?;
            } else if is_error {
                writeln!(stderr, "[repl] error")?;
            }
            for content in contents {
                match content {
                    WorkerContent::ContentText { text, stream, .. } => match stream {
                        TextStream::Stdout => stdout.write_all(text.as_bytes())?,
                        TextStream::Stderr => stderr.write_all(text.as_bytes())?,
                    },
                    WorkerContent::ContentImage {
                        data,
                        mime_type,
                        id,
                        is_new,
                    } => {
                        if image_support && write_kitty_image(stdout, &data, &mime_type)? {
                            // image rendered
                        } else {
                            writeln!(
                                stderr,
                                "[repl] image id={id} mime={mime_type} bytes={} new={is_new}",
                                data.len()
                            )?;
                        }
                    }
                }
            }
        }
    }
    stdout.flush()?;
    stderr.flush()?;
    Ok(())
}

fn render_visible_reply(
    response: Option<&mut ResponseState>,
    reply: Result<WorkerReply, WorkerError>,
    context: VisibleReplyContext,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
    image_support: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(response) = response {
        let error_banner = reply_error_banner(&reply);
        let reply = response.finalize_worker_result(
            reply,
            context.pending_request_after,
            context.timeout_bundle_reuse,
            context.detached_prefix_item_count,
        );
        render_finalized_reply(reply, error_banner, stdout, stderr, image_support)?;
        return Ok(());
    }

    render_reply(reply?, stdout, stderr, image_support)?;
    Ok(())
}

fn reply_error_banner(reply: &Result<WorkerReply, WorkerError>) -> Option<Option<WorkerErrorCode>> {
    match reply {
        Ok(WorkerReply::Output {
            is_error,
            error_code,
            ..
        }) => {
            if error_code.is_some() {
                Some(*error_code)
            } else if *is_error {
                Some(None)
            } else {
                None
            }
        }
        Err(_) => Some(None),
    }
}

fn render_finalized_reply(
    reply: CallToolResult,
    error_banner: Option<Option<WorkerErrorCode>>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
    image_support: bool,
) -> io::Result<()> {
    if let Some(code) = error_banner.flatten() {
        writeln!(stderr, "[repl] error: {code:?}")?;
    } else if error_banner.is_some() {
        writeln!(stderr, "[repl] error")?;
    }

    for content in reply.content {
        let stream = text_stream_from_content(&content).unwrap_or(TextStream::Stdout);
        match content.raw {
            RawContent::Text(text) => match stream {
                TextStream::Stdout => stdout.write_all(text.text.as_bytes())?,
                TextStream::Stderr => stderr.write_all(text.text.as_bytes())?,
            },
            RawContent::Image(image) => {
                if image_support && write_kitty_image(stdout, &image.data, &image.mime_type)? {
                    // image rendered
                } else {
                    writeln!(
                        stderr,
                        "[repl] image mime={} bytes={}",
                        image.mime_type,
                        image.data.len()
                    )?;
                }
            }
            RawContent::Audio(audio) => {
                writeln!(
                    stderr,
                    "[repl] audio mime={} bytes={}",
                    audio.mime_type,
                    audio.data.len()
                )?;
            }
            RawContent::Resource(_) => {
                writeln!(stderr, "[repl] resource content omitted")?;
            }
            RawContent::ResourceLink(_) => {
                writeln!(stderr, "[repl] resource link omitted")?;
            }
        }
    }
    stdout.flush()?;
    stderr.flush()?;
    Ok(())
}

fn detect_image_support() -> bool {
    if let Ok(value) = env::var("MCP_REPL_IMAGES") {
        return is_truthy(&value);
    }
    let term = env::var("TERM").unwrap_or_default().to_lowercase();
    if term.contains("xterm-kitty") {
        return true;
    }
    if env::var_os("KITTY_WINDOW_ID").is_some() {
        return true;
    }
    let term_program = env::var("TERM_PROGRAM").unwrap_or_default().to_lowercase();
    matches!(term_program.as_str(), "ghostty" | "wezterm" | "iterm.app")
}

fn is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_lowercase().as_str(),
        "1" | "true" | "yes" | "on" | "kitty"
    )
}

fn write_kitty_image(stdout: &mut impl Write, data: &str, mime_type: &str) -> io::Result<bool> {
    let format = match mime_type.trim().to_lowercase().as_str() {
        "image/png" => 100,
        "image/jpeg" | "image/jpg" => 24,
        _ => return Ok(false),
    };
    const CHUNK: usize = 4096;
    let bytes = data.as_bytes();
    let mut offset = 0;
    while offset < bytes.len() {
        let end = (offset + CHUNK).min(bytes.len());
        let more = end < bytes.len();
        if offset == 0 {
            write!(
                stdout,
                "\x1b_Gf={},a=T,m={};",
                format,
                if more { 1 } else { 0 }
            )?;
        } else {
            write!(stdout, "\x1b_Gm={};", if more { 1 } else { 0 })?;
        }
        stdout.write_all(&bytes[offset..end])?;
        stdout.write_all(b"\x1b\\")?;
        offset = end;
    }
    stdout.write_all(b"\n")?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::server::response::{ResponseState, TimeoutBundleReuse};
    use crate::worker_protocol::{WorkerContent, WorkerErrorCode, WorkerReply};

    fn rendered_text(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|content| match &content.raw {
                RawContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn transcript_path(text: &str) -> Option<std::path::PathBuf> {
        let end = text
            .find("transcript.txt")?
            .saturating_add("transcript.txt".len());
        let start = text[..end]
            .rfind(|ch: char| ch.is_whitespace() || matches!(ch, '"' | '\'' | '[' | '('))
            .map_or(0, |idx| idx.saturating_add(1));
        Some(std::path::PathBuf::from(&text[start..end]))
    }

    #[test]
    fn detect_image_support_uses_mcp_repl_env() {
        let original = env::var_os("MCP_REPL_IMAGES");
        unsafe {
            env::set_var("MCP_REPL_IMAGES", "1");
        }
        let enabled = detect_image_support();
        match original {
            Some(value) => unsafe {
                env::set_var("MCP_REPL_IMAGES", value);
            },
            None => unsafe {
                env::remove_var("MCP_REPL_IMAGES");
            },
        }
        assert!(enabled, "expected MCP_REPL_IMAGES=1 to enable images");
    }

    #[test]
    fn finalized_reply_preserves_stderr_routing() {
        let mut response = ResponseState::new().expect("response state should initialize");
        let reply = response.finalize_worker_result(
            Ok(WorkerReply::Output {
                contents: vec![
                    WorkerContent::worker_stdout("stdout line\n"),
                    WorkerContent::worker_stderr("stderr: boom\n"),
                ],
                is_error: true,
                error_code: None,
                prompt: None,
                prompt_variants: None,
            }),
            false,
            TimeoutBundleReuse::None,
            0,
        );

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        render_finalized_reply(reply, Some(None), &mut stdout, &mut stderr, false)
            .expect("finalized reply should render");

        let stdout = String::from_utf8(stdout).expect("stdout should be valid UTF-8");
        let stderr = String::from_utf8(stderr).expect("stderr should be valid UTF-8");
        assert!(
            stdout.contains("stdout line\n"),
            "expected stdout text on stdout, got: {stdout:?}"
        );
        assert!(
            !stdout.contains("stderr: boom\n"),
            "stderr chunk leaked to stdout: {stdout:?}"
        );
        assert!(
            stderr.contains("[repl] error\n"),
            "expected error banner on stderr, got: {stderr:?}"
        );
        assert!(
            stderr.contains("stderr: boom\n"),
            "expected stderr chunk on stderr, got: {stderr:?}"
        );
    }

    #[test]
    fn exact_interrupt_keeps_disclosed_timeout_bundle_open() {
        let mut response = ResponseState::new().expect("response state should initialize");
        let timed_out_head = format!("HEAD_START\n{}\nHEAD_END\n", "x".repeat(5000));
        let first = response.finalize_worker_result(
            Ok(WorkerReply::Output {
                contents: vec![WorkerContent::worker_stdout(timed_out_head)],
                is_error: false,
                error_code: Some(WorkerErrorCode::Timeout),
                prompt: None,
                prompt_variants: None,
            }),
            true,
            TimeoutBundleReuse::FullReply,
            0,
        );
        let first_text = rendered_text(&first);
        let transcript_path = transcript_path(&first_text).unwrap_or_else(|| {
            panic!("expected transcript path after timed-out output, got: {first_text:?}")
        });

        let interrupt_tail = "INTERRUPT_TAIL\n";
        let second = response.finalize_worker_result(
            Ok(WorkerReply::Output {
                contents: vec![WorkerContent::worker_stdout(interrupt_tail)],
                is_error: false,
                error_code: None,
                prompt: Some("> ".to_string()),
                prompt_variants: None,
            }),
            false,
            timeout_bundle_reuse_for_exact_command("INTERRUPT"),
            0,
        );
        let second_text = rendered_text(&second);
        let transcript = fs::read_to_string(&transcript_path)
            .unwrap_or_else(|err| panic!("expected transcript to be readable: {err}"));

        assert!(
            second_text.contains(interrupt_tail),
            "expected interrupt reply to stay visible inline, got: {second_text:?}"
        );
        assert!(
            transcript.contains("HEAD_START") && transcript.contains("HEAD_END"),
            "expected earlier timed-out output in the disclosed transcript, got: {transcript:?}"
        );
        assert!(
            transcript.contains(interrupt_tail),
            "expected exact INTERRUPT to keep appending to the disclosed timeout bundle, got: {transcript:?}"
        );
    }
}
