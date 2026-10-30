// src-tauri/src/doc_parse.rs
//
// items.id=837 (decisions.id=927): PDF and DOCX text extraction runs in a CHILD
// process of the app binary, so a hostile file can only cost the child. Both
// parsers read untrusted bytes, and one crafted file under 2 MB grew them to
// 22-26 GB in-process (items.id=665): the read cap, a timeout and catch_unwind
// do not bound that, an allocation failure is an abort.
//
// CHILD SIDE. `dispatch_parse_mode()` is the first thing `main()` calls, before
// logging, CEF, Tokio, Tauri, the key registry or any database, and does
// nothing unless the process was started in parse mode.
//
// Protocol: argv = [exe, PARSE_ARG, "pdf" | "docx"]; the document bytes arrive
// on stdin and nothing else does (no key, no path, no secrets); the extracted
// text leaves on stdout. The exit status says what happened: EXIT_OK (text,
// possibly empty), EXIT_PARSE_ERROR (the parser refused or panicked on the
// file), EXIT_IO, EXIT_USAGE. A kill by signal (the memory limit's abort,
// SIGXCPU, a stack overflow) is the parent's to read.
//
// PARENT SIDE (Linux). `extract_in_child` spawns the child with the limits of
// `ParseLimits` applied to the CHILD only: RLIMIT_AS, RLIMIT_CPU, no core dump,
// every inherited descriptor above stderr marked close-on-exec, an empty
// environment (except LD_LIBRARY_PATH, which the loader needs), its own process
// group, and a wall-clock kill of that group on timeout, oversize output,
// error and cancellation. The child is always waited for. The limits are set
// by the parent, never by the child: a parser that is out of control cannot be
// trusted to limit itself. Windows and macOS limits are items.id=838; they
// replace `apply_child_limits` and the process-group handling, not the shape of
// `extract_in_child`.

use std::io::{Read, Write};

use thiserror::Error;

/// argv[1] of a parse-mode invocation. Anything else starts the real app.
pub const PARSE_ARG: &str = "--qr-parse-document";

/// The child reads at most this many bytes from stdin (the same 100 MiB as
/// commands::ingest's MAX_INGEST_BYTES, which is what the parent sends).
pub const MAX_INPUT_BYTES: u64 = 100 * 1024 * 1024;

pub const EXIT_OK: i32 = 0;
pub const EXIT_PARSE_ERROR: i32 = 2;
pub const EXIT_IO: i32 = 3;
pub const EXIT_USAGE: i32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentKind {
    Pdf,
    Docx,
}

impl DocumentKind {
    pub fn as_arg(self) -> &'static str {
        match self {
            DocumentKind::Pdf => "pdf",
            DocumentKind::Docx => "docx",
        }
    }

    pub fn from_arg(arg: &str) -> Option<Self> {
        match arg {
            "pdf" => Some(DocumentKind::Pdf),
            "docx" => Some(DocumentKind::Docx),
            _ => None,
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("the parser could not read the document")]
    Unreadable,
    #[error("the parser panicked on the document")]
    Panicked,
}

/// The two parsers, called the same way for every caller (the child, and the
/// in-process path of targets without child limits). Both crates parse
/// untrusted bytes and pdf-extract's own tracker documents open panics on such
/// input (jrmuizel/pdf-extract#141 and others), so a panic is caught here and
/// reported as an error instead of unwinding into the caller.
pub fn parse_document_text(kind: DocumentKind, bytes: &[u8]) -> Result<String, ParseError> {
    let parsed = std::panic::catch_unwind(|| match kind {
        DocumentKind::Pdf => pdf_extract::extract_text_from_mem(bytes).ok(),
        DocumentKind::Docx => docx_rust::DocxFile::from_reader(std::io::Cursor::new(bytes))
            .ok()?
            .parse()
            .ok()
            .map(|docx| docx.document.body.text()),
    });
    match parsed {
        Ok(Some(text)) => Ok(text),
        Ok(None) => Err(ParseError::Unreadable),
        Err(_) => Err(ParseError::Panicked),
    }
}

/// Runs the parse-mode child when this process was started as one, and never
/// returns in that case (it ends the process with `_exit`: the atexit chain
/// segfaults once CEF's libraries are loaded, items.id=315). Returns at once,
/// touching nothing, when argv[1] is not PARSE_ARG.
pub fn dispatch_parse_mode() {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new(PARSE_ARG)) {
        return;
    }
    let kind = args
        .next()
        .and_then(|a| a.to_str().and_then(DocumentKind::from_arg));
    let code = match kind {
        Some(kind) => run_child(kind),
        None => EXIT_USAGE,
    };
    // SAFETY: _exit only terminates the process; stdout was flushed by
    // run_child and nothing here depends on destructors or atexit handlers.
    unsafe { libc::_exit(code) }
}

fn run_child(kind: DocumentKind) -> i32 {
    let mut bytes = Vec::new();
    if std::io::stdin()
        .lock()
        .take(MAX_INPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() as u64 > MAX_INPUT_BYTES
    {
        return EXIT_IO;
    }
    match parse_document_text(kind, &bytes) {
        Ok(text) => {
            let mut out = std::io::stdout().lock();
            if out
                .write_all(text.as_bytes())
                .and_then(|()| out.flush())
                .is_err()
            {
                return EXIT_IO;
            }
            EXIT_OK
        }
        Err(_) => EXIT_PARSE_ERROR,
    }
}

#[cfg(target_os = "linux")]
pub use isolated::{extract_in_child, interpret, ParseCommand, ParseFailure, ParseLimits};

#[cfg(target_os = "linux")]
mod isolated {
    use std::ffi::OsString;
    use std::io;
    use std::os::unix::process::ExitStatusExt;
    use std::path::PathBuf;
    use std::process::{ExitStatus, Stdio};
    use std::time::Duration;

    use thiserror::Error;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::process::{Child, Command};

    use super::{DocumentKind, EXIT_OK, PARSE_ARG};

    /// Limits on one child. Memory is the headline requirement (a timeout alone
    /// does not bound it); the wall clock and the CPU limit are belt and braces.
    #[derive(Debug, Clone, Copy)]
    pub struct ParseLimits {
        /// RLIMIT_AS, bytes of address space.
        pub memory_bytes: u64,
        /// RLIMIT_CPU soft limit in seconds (the hard limit is 2 s later).
        pub cpu_secs: u64,
        /// Wall clock for the whole exchange: spawn, stdin, parse, stdout, exit.
        pub wall: Duration,
        /// Largest accepted extracted text, bytes. More is a failure, never a
        /// truncation.
        pub max_output: usize,
    }

    impl ParseLimits {
        /// items.id=837 (Jason, 2026-10-30). Measured on the release binary: the
        /// loader alone needs ~705 MiB of address space with libcef mapped, the
        /// largest legitimate files tried (a 103 MB PDF, a 40 MiB-XML DOCX) peak
        /// at 958 MiB and 1.49 GiB, and the hostile fixtures abort in under a
        /// second at 2 GiB.
        pub const MEMORY_BYTES: u64 = 2 * 1024 * 1024 * 1024;
        /// 16 MiB of text is about 8,000 dense pages; a book is 1-3 MiB.
        pub const MAX_OUTPUT: usize = 16 * 1024 * 1024;

        /// The limits ingest uses, with the caller's wall clock (ingest owns
        /// EXTRACTION_TIMEOUT, which has to stay under the writer gate's drain
        /// ceiling).
        pub fn production(wall: Duration) -> Self {
            Self {
                memory_bytes: Self::MEMORY_BYTES,
                cpu_secs: wall.as_secs().max(1),
                wall,
                max_output: Self::MAX_OUTPUT,
            }
        }
    }

    /// What to run. Production runs this very executable in parse mode; tests
    /// substitute other programs to exercise the kill, wait and cap paths.
    #[derive(Debug, Clone)]
    pub struct ParseCommand {
        pub program: PathBuf,
        pub args: Vec<OsString>,
    }

    impl ParseCommand {
        /// This executable in parse mode for `kind`. `/proc/self/exe` rather
        /// than `current_exe()`: it still works after an update replaced the
        /// file on disk (current_exe would then carry a " (deleted)" suffix).
        pub fn this_executable(kind: DocumentKind) -> Self {
            Self::executable(PathBuf::from("/proc/self/exe"), kind)
        }

        /// The given executable in parse mode for `kind`.
        pub fn executable(program: PathBuf, kind: DocumentKind) -> Self {
            Self {
                program,
                args: vec![PARSE_ARG.into(), kind.as_arg().into()],
            }
        }
    }

    /// Why a child produced no text. Content-free: none of this carries a
    /// path or any of the document.
    #[derive(Debug, Error, PartialEq, Eq)]
    pub enum ParseFailure {
        #[error("the parser process could not be started: {0}")]
        Spawn(String),
        #[error("the parser process failed: {0}")]
        Io(String),
        #[error("the parser did not finish in time")]
        Timeout,
        #[error("the extracted text is larger than the cap")]
        Oversize,
        #[error("the parser could not read the document (exit {0})")]
        Exit(i32),
        #[error("the parser was killed by signal {0}")]
        Signal(i32),
        #[error("the extracted text is not valid UTF-8")]
        NotUtf8,
    }

    /// The exit status and stdout of a finished child as text or a failure.
    pub fn interpret(status: ExitStatus, stdout: Vec<u8>) -> Result<String, ParseFailure> {
        if let Some(signal) = status.signal() {
            return Err(ParseFailure::Signal(signal));
        }
        match status.code() {
            Some(code) if code == EXIT_OK => {
                String::from_utf8(stdout).map_err(|_| ParseFailure::NotUtf8)
            }
            Some(code) => Err(ParseFailure::Exit(code)),
            None => Err(ParseFailure::Io("no exit status".to_owned())),
        }
    }

    /// Owns the child; whatever way the exchange ends, including the future
    /// being dropped (a cancelled command), the child's process group is killed.
    /// `Child::id` is None once the child was reaped, which is exactly when
    /// signalling its group id would no longer be safe.
    struct ChildGuard(Child);

    impl ChildGuard {
        fn kill_group(&self) {
            if let Some(pid) = self.0.id() {
                // SAFETY: killpg only sends a signal. The child is its own
                // group leader (process_group(0)) and has not been reaped (id()
                // is Some), so the group id cannot belong to anything else.
                unsafe {
                    libc::killpg(pid as libc::pid_t, libc::SIGKILL);
                }
            }
        }
    }

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            self.kill_group();
            // kill_on_drop(true) queues the zombie with tokio's reaper.
        }
    }

    /// Runs the child, feeds it `bytes` and returns its text, or why there is
    /// none. Always leaves no running process and no zombie behind.
    pub async fn extract_in_child(
        command: &ParseCommand,
        bytes: &[u8],
        limits: ParseLimits,
    ) -> Result<String, ParseFailure> {
        let mut cmd = Command::new(&command.program);
        cmd.args(&command.args)
            // Nothing from the app's environment, secrets included. The dynamic
            // loader still needs LD_LIBRARY_PATH where the build relies on it
            // (cargo's test runs find libcef through it, CI finds sqlcipher).
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .kill_on_drop(true);
        if let Some(path) = std::env::var_os("LD_LIBRARY_PATH") {
            cmd.env("LD_LIBRARY_PATH", path);
        }
        let (memory, cpu) = (limits.memory_bytes, limits.cpu_secs);
        // SAFETY: the closure runs between fork and exec and only calls
        // async-signal-safe functions (setrlimit, close_range, fcntl,
        // getrlimit); it does not allocate.
        unsafe {
            cmd.pre_exec(move || apply_child_limits(memory, cpu));
        }

        let mut guard = ChildGuard(
            cmd.spawn()
                .map_err(|e| ParseFailure::Spawn(e.to_string()))?,
        );
        let stdin = guard.0.stdin.take();
        let stdout = guard.0.stdout.take();
        let (Some(stdin), Some(stdout)) = (stdin, stdout) else {
            return Err(ParseFailure::Io("child pipes missing".to_owned()));
        };

        let exchange = async {
            let feed = async move {
                let mut stdin = stdin;
                // A child that exits early closes the pipe: not an error here,
                // its exit status says what happened.
                let _ = stdin.write_all(bytes).await;
                let _ = stdin.shutdown().await;
            };
            let drain = read_capped(stdout, limits.max_output);
            let ((), text) = tokio::join!(feed, drain);
            let text = text?;
            let status = guard
                .0
                .wait()
                .await
                .map_err(|e| ParseFailure::Io(e.to_string()))?;
            interpret(status, text)
        };
        let result = tokio::time::timeout(limits.wall, exchange)
            .await
            .unwrap_or(Err(ParseFailure::Timeout));

        if result.is_err() {
            // Not reaped on these paths unless the exchange got to wait(), in
            // which case id() is None and there is nothing to signal.
            guard.kill_group();
            let _ = tokio::time::timeout(Duration::from_secs(2), guard.0.wait()).await;
        }
        result
    }

    /// Reads all of `reader`, failing as soon as more than `cap` bytes arrive.
    async fn read_capped<R: tokio::io::AsyncRead + Unpin>(
        reader: R,
        cap: usize,
    ) -> Result<Vec<u8>, ParseFailure> {
        let mut buf = Vec::new();
        reader
            .take(cap as u64 + 1)
            .read_to_end(&mut buf)
            .await
            .map_err(|e| ParseFailure::Io(e.to_string()))?;
        if buf.len() > cap {
            return Err(ParseFailure::Oversize);
        }
        Ok(buf)
    }

    const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;
    /// Descriptors the fallback loop walks at most.
    const FD_FALLBACK_CEILING: libc::rlim_t = 1 << 20;

    /// Runs in the child between fork and exec.
    fn apply_child_limits(memory_bytes: u64, cpu_secs: u64) -> io::Result<()> {
        set_limit(libc::RLIMIT_AS as libc::c_int, memory_bytes, memory_bytes)?;
        set_limit(libc::RLIMIT_CPU as libc::c_int, cpu_secs, cpu_secs + 2)?;
        // A crashed or memory-killed child must not leave a core file holding
        // the document (CI allows cores up to 2 GiB).
        set_limit(libc::RLIMIT_CORE as libc::c_int, 0, 0)?;
        mark_fds_close_on_exec();
        Ok(())
    }

    fn set_limit(resource: libc::c_int, soft: u64, hard: u64) -> io::Result<()> {
        let limit = libc::rlimit {
            rlim_cur: soft as libc::rlim_t,
            rlim_max: hard as libc::rlim_t,
        };
        // SAFETY: setrlimit reads one initialized struct.
        if unsafe { libc::setrlimit(resource as _, &limit) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// Marks every descriptor above stderr close-on-exec, so nothing the app has
    /// open (a database, a socket) reaches the child. It MARKS and does not
    /// close: std reports an exec failure to the parent through a private
    /// close-on-exec pipe, and closing that pipe here would turn a spawn error
    /// into a child that dies with SIGABRT.
    fn mark_fds_close_on_exec() {
        // SAFETY: plain syscalls on integers; no memory is shared.
        unsafe {
            let done = libc::syscall(
                libc::SYS_close_range,
                3 as libc::c_uint,
                libc::c_uint::MAX,
                CLOSE_RANGE_CLOEXEC,
            );
            if done == 0 {
                return;
            }
            // ENOSYS before Linux 5.9, EINVAL on 5.9-5.10 (no CLOEXEC flag).
            let mut nofile = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::getrlimit(libc::RLIMIT_NOFILE, &mut nofile) != 0 {
                nofile.rlim_cur = 1024;
            }
            let top = nofile.rlim_cur.min(FD_FALLBACK_CEILING) as libc::c_int;
            for fd in 3..top {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::process::ExitStatusExt;

        #[test]
        fn exit_zero_with_utf8_is_text() {
            assert_eq!(
                interpret(ExitStatus::from_raw(0), b"hello".to_vec()),
                Ok("hello".to_owned())
            );
            assert_eq!(
                interpret(ExitStatus::from_raw(0), Vec::new()),
                Ok(String::new())
            );
        }

        #[test]
        fn exit_zero_with_other_bytes_is_not_utf8() {
            assert_eq!(
                interpret(ExitStatus::from_raw(0), vec![0xff, 0xfe]),
                Err(ParseFailure::NotUtf8)
            );
        }

        #[test]
        fn a_nonzero_exit_is_a_failure_and_ignores_stdout() {
            // wait status 2 << 8 is "exited with code 2"
            assert_eq!(
                interpret(ExitStatus::from_raw(2 << 8), b"partial".to_vec()),
                Err(ParseFailure::Exit(2))
            );
        }

        #[test]
        fn a_signal_is_reported_as_one() {
            assert_eq!(
                interpret(ExitStatus::from_raw(libc::SIGABRT), Vec::new()),
                Err(ParseFailure::Signal(libc::SIGABRT))
            );
        }

        #[test]
        fn production_limits_follow_the_wall_clock_and_the_decided_caps() {
            let l = ParseLimits::production(Duration::from_secs(20));
            assert_eq!(l.memory_bytes, 2 * 1024 * 1024 * 1024);
            assert_eq!(l.max_output, 16 * 1024 * 1024);
            assert_eq!(l.cpu_secs, 20);
            assert_eq!(l.wall, Duration::from_secs(20));
        }
    }
}
