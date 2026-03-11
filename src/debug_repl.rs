use std::env;
use std::io::{self, BufRead, Write};
use std::thread;
use std::time::Duration;
use std::time::Instant;

use crate::backend::Backend;
use crate::pager;
use crate::reply_overflow::ReplyOverflowSettings;
use crate::sandbox_cli::SandboxCliPlan;
use crate::worker_process::{WorkerError, WorkerManager};
use crate::worker_protocol::{TextStream, WorkerContent, WorkerReply};

const DEFAULT_WRITE_STDIN_TIMEOUT: Duration = Duration::from_secs(60);
const SAFETY_MARGIN: f64 = 1.05;
const MIN_SERVER_GRACE: Duration = Duration::from_secs(1);
const DEBUG_REPL_PAGE_CHARS: u64 = 300;
const INITIAL_PROMPT_WAIT: Duration = Duration::from_secs(5);
const INITIAL_PROMPT_POLL_INTERVAL: Duration = Duration::from_millis(50);

pub(crate) fn run(
    backend: Backend,
    sandbox_plan: SandboxCliPlan,
    reply_overflow: ReplyOverflowSettings,
) -> Result<(), Box<dyn std::error::Error>> {
    ensure_debug_repl_page_size();
    let image_support = detect_image_support();
    eprintln!(
        "debug repl: write_stdin timeout={:.1}s | end input with END | commands: INTERRUPT, RESTART | Ctrl-D to exit | images={}",
        DEFAULT_WRITE_STDIN_TIMEOUT.as_secs_f64(),
        if image_support { "kitty" } else { "off" }
    );

    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    let server_timeout = apply_safety_margin(DEFAULT_WRITE_STDIN_TIMEOUT);

    let mut worker = WorkerManager::new(backend, sandbox_plan, reply_overflow)?;
    worker.warm_start()?;
    let reply = wait_for_initial_prompt(&mut worker, server_timeout)?;
    render_reply(reply, &mut stdout, &mut stderr, image_support)?;

    let stdin = io::stdin();
    let mut stdin = stdin.lock();

    loop {
        let Some(line) = read_line(&mut stdin)? else {
            break;
        };

        if is_exact_command(&line, "INTERRUPT") {
            let reply = worker.interrupt(DEFAULT_WRITE_STDIN_TIMEOUT)?;
            render_reply(reply, &mut stdout, &mut stderr, image_support)?;
            continue;
        }
        if is_exact_command(&line, "RESTART") {
            let reply = worker.restart(DEFAULT_WRITE_STDIN_TIMEOUT)?;
            render_reply(reply, &mut stdout, &mut stderr, image_support)?;
            continue;
        }
        if is_exact_command(&line, "END") {
            let reply = worker.write_stdin(
                String::new(),
                DEFAULT_WRITE_STDIN_TIMEOUT,
                server_timeout,
                None,
                false,
            )?;
            render_reply(reply, &mut stdout, &mut stderr, image_support)?;
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

        let reply = worker.write_stdin(
            input,
            DEFAULT_WRITE_STDIN_TIMEOUT,
            server_timeout,
            None,
            false,
        )?;
        render_reply(reply, &mut stdout, &mut stderr, image_support)?;
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
                    WorkerContent::ContentText { text, stream } => match stream {
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

fn detect_image_support() -> bool {
    if let Ok(value) = env::var("MCP_CONSOLE_REPL_IMAGES") {
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

fn ensure_debug_repl_page_size() {
    if env::var_os(pager::PAGER_PAGE_CHARS_ENV).is_some() {
        return;
    }
    unsafe {
        env::set_var(
            pager::PAGER_PAGE_CHARS_ENV,
            DEBUG_REPL_PAGE_CHARS.to_string(),
        );
    }
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
