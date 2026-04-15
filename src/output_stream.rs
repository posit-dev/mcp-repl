use std::cell::Cell;
use std::io::{self, Write};
use std::sync::{Mutex, MutexGuard};

static OUTPUT_LOCK: Mutex<()> = Mutex::new(());
thread_local! {
    static OUTPUT_DEPTH: Cell<usize> = const { Cell::new(0) };
}

pub(crate) fn with_output_lock<T>(f: impl FnOnce() -> T) -> T {
    OUTPUT_DEPTH.with(|depth| {
        let _guard = OutputLockGuard::new(depth);
        f()
    })
}

struct OutputLockGuard<'a> {
    depth: &'a Cell<usize>,
    guard: Option<MutexGuard<'static, ()>>,
}

impl<'a> OutputLockGuard<'a> {
    fn new(depth: &'a Cell<usize>) -> Self {
        let guard = if depth.get() == 0 {
            Some(OUTPUT_LOCK.lock().unwrap())
        } else {
            None
        };
        depth.set(depth.get().saturating_add(1));
        Self { depth, guard }
    }
}

impl Drop for OutputLockGuard<'_> {
    fn drop(&mut self) {
        let current = self.depth.get();
        if current > 0 {
            self.depth.set(current - 1);
        }
        if self.depth.get() == 0 {
            self.guard.take();
        }
    }
}

pub(crate) fn write_stdout_bytes(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    with_output_lock(|| {
        let stdout = std::io::stdout();
        let mut stdout = stdout.lock();
        let _ = write_all_bytes(&mut stdout, bytes);
    });
}

pub(crate) fn write_stderr_bytes(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    with_output_lock(|| {
        let stderr = std::io::stderr();
        let mut stderr = stderr.lock();
        let _ = write_all_bytes(&mut stderr, bytes);
    });
}

fn write_all_bytes<W: Write>(writer: &mut W, bytes: &[u8]) -> io::Result<()> {
    let mut offset = 0usize;
    while offset < bytes.len() {
        let chunk = &bytes[offset..];
        match writer.write(chunk) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write output bytes",
                ));
            }
            Ok(written) => {
                offset = offset.saturating_add(written);
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {
                continue;
            }
            Err(err) => return Err(err),
        }
    }
    writer.flush()?;
    Ok(())
}
