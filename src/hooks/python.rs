use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{ErrorKind, Read};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::domain::{HookWorkingDirectory, Tool};

use super::HookContext;

pub(super) fn run(
    script: &Path,
    args: &[String],
    timeout_seconds: u64,
    working_directory: HookWorkingDirectory,
    environment: &BTreeMap<String, String>,
    tool: &Tool,
    context: &HookContext<'_>,
) -> Result<()> {
    let script = context.app_root.join(script);
    let cwd = working_directory_path(working_directory, context).ok_or_else(|| {
        anyhow::anyhow!("working directory {working_directory:?} is unavailable at this stage")
    })?;
    let mut missing = Vec::new();
    let mut incompatible = Vec::new();
    for interpreter in python_candidates() {
        match interpreter.probe() {
            Ok(true) => {}
            Ok(false) => {
                incompatible.push(interpreter.name());
                continue;
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                missing.push(interpreter.name());
                continue;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "cannot inspect Python interpreter {:?}",
                        interpreter.program
                    )
                });
            }
        }

        let mut command = interpreter.command();
        command
            .arg(&script)
            .args(args)
            .current_dir(cwd)
            .envs(environment)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("UTU_TOOL_ID", &tool.id)
            .env("UTU_TOOL_NAME", &tool.name)
            .env("UTU_TOOLKIT_ROOT", context.toolkit_root)
            .env("UTU_DOWNLOAD_DIR", context.downloads)
            .env("UTU_INSTALL_DIR", context.install);
        if let Some(staging) = context.staging {
            command.env("UTU_STAGING_DIR", staging);
        }
        if let Some(version) = context.version {
            command.env("UTU_VERSION", version);
        }
        match command.spawn() {
            Ok(child) => return wait_for_script(child, &script, timeout_seconds),
            Err(error) if error.kind() == ErrorKind::NotFound => missing.push(interpreter.name()),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "cannot execute Python interpreter {:?}",
                        interpreter.program
                    )
                });
            }
        }
    }
    bail!(
        "Python 3 interpreter not found; missing [{}], incompatible [{}]; set UTU_PYTHON to a Python 3 interpreter path",
        missing.join(", "),
        incompatible.join(", ")
    )
}

fn wait_for_script(
    mut child: std::process::Child,
    script: &Path,
    timeout_seconds: u64,
) -> Result<()> {
    let stdout = child.stdout.take().map(capture_pipe);
    let stderr = child.stderr.take().map(capture_pipe);
    let started = Instant::now();
    let outcome = loop {
        if let Some(status) = child.try_wait()? {
            break WaitOutcome::Exited(status);
        }
        if started.elapsed() >= Duration::from_secs(timeout_seconds) {
            let _ = child.kill();
            let _ = child.wait();
            break WaitOutcome::TimedOut;
        }
        thread::sleep(Duration::from_millis(25));
    };
    let stdout = collect_pipe(stdout, "stdout")?;
    let stderr = collect_pipe(stderr, "stderr")?;
    log_output(script, "stdout", &stdout);
    log_output(script, "stderr", &stderr);

    match outcome {
        WaitOutcome::Exited(status) if status.success() => Ok(()),
        WaitOutcome::Exited(status) => bail!(
            "Python script {} exited with {status}{}",
            script.display(),
            error_output(&stdout, &stderr)
        ),
        WaitOutcome::TimedOut => bail!(
            "Python script {} timed out after {timeout_seconds} seconds{}",
            script.display(),
            error_output(&stdout, &stderr)
        ),
    }
}

enum WaitOutcome {
    Exited(ExitStatus),
    TimedOut,
}

struct PipeCapture {
    output: Arc<Mutex<CapturedOutput>>,
    finished: mpsc::Receiver<std::io::Result<()>>,
}

#[derive(Clone, Default)]
struct CapturedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

const MAX_CAPTURED_OUTPUT: usize = 64 * 1024;
const PIPE_DRAIN_GRACE: Duration = Duration::from_millis(250);

fn capture_pipe(mut pipe: impl Read + Send + 'static) -> PipeCapture {
    let output = Arc::new(Mutex::new(CapturedOutput::default()));
    let thread_output = Arc::clone(&output);
    let (sender, finished) = mpsc::channel();
    drop(thread::spawn(move || {
        let result = (|| {
            let mut buffer = [0_u8; 8192];
            loop {
                let read = pipe.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                let mut captured = thread_output
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let remaining = MAX_CAPTURED_OUTPUT.saturating_sub(captured.bytes.len());
                let retained = remaining.min(read);
                captured.bytes.extend_from_slice(&buffer[..retained]);
                captured.truncated |= retained < read;
            }
            Ok(())
        })();
        let _ = sender.send(result);
    }));
    PipeCapture { output, finished }
}

fn collect_pipe(capture: Option<PipeCapture>, stream: &str) -> Result<String> {
    let Some(capture) = capture else {
        return Ok(String::new());
    };
    let complete = match capture.finished.recv_timeout(PIPE_DRAIN_GRACE) {
        Ok(result) => {
            result.with_context(|| format!("cannot read Python {stream}"))?;
            true
        }
        Err(mpsc::RecvTimeoutError::Timeout) => false,
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            bail!("Python {stream} reader stopped unexpectedly")
        }
    };
    let captured = capture
        .output
        .lock()
        .map_err(|_| anyhow::anyhow!("Python {stream} capture state is poisoned"))?
        .clone();
    let mut output = String::from_utf8_lossy(&captured.bytes).into_owned();
    if captured.truncated {
        output.push_str("\n[output truncated]");
    }
    if !complete {
        output.push_str("\n[output capture still open after script exit]");
    }
    Ok(output)
}

fn log_output(script: &Path, stream: &str, output: &str) {
    let output = output.trim();
    if !output.is_empty() {
        tracing::debug!(script = %script.display(), stream, output, "Python hook output");
    }
}

fn error_output(stdout: &str, stderr: &str) -> String {
    let stdout = stdout.trim();
    let stderr = stderr.trim();
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => String::new(),
        (false, true) => format!("; stdout: {stdout}"),
        (true, false) => format!("; stderr: {stderr}"),
        (false, false) => format!("; stdout: {stdout}; stderr: {stderr}"),
    }
}

struct PythonInterpreter {
    program: OsString,
    launcher_args: Vec<OsString>,
}

impl PythonInterpreter {
    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.launcher_args);
        command
    }

    fn probe(&self) -> std::io::Result<bool> {
        self.command()
            .arg("-c")
            .arg("import sys; raise SystemExit(sys.version_info.major != 3)")
            .status()
            .map(|status| status.success())
    }

    fn name(&self) -> String {
        self.program.to_string_lossy().into_owned()
    }
}

fn python_candidates() -> Vec<PythonInterpreter> {
    if let Some(program) = std::env::var_os("UTU_PYTHON").filter(|value| !value.is_empty()) {
        return vec![PythonInterpreter {
            program,
            launcher_args: Vec::new(),
        }];
    }
    #[cfg(windows)]
    let candidates: &[(&str, &[&str])] = &[
        ("py", &["-3"][..]),
        ("python3", &[][..]),
        ("python", &[][..]),
    ];
    #[cfg(not(windows))]
    let candidates: &[(&str, &[&str])] = &[("python3", &[][..]), ("python", &[][..])];
    candidates
        .iter()
        .map(|(program, args)| PythonInterpreter {
            program: OsString::from(*program),
            launcher_args: args.iter().map(|arg| OsString::from(*arg)).collect(),
        })
        .collect()
}

fn working_directory_path<'a>(
    working_directory: HookWorkingDirectory,
    context: &'a HookContext<'_>,
) -> Option<&'a Path> {
    match working_directory {
        HookWorkingDirectory::App => Some(context.app_root),
        HookWorkingDirectory::Toolkit => Some(context.toolkit_root),
        HookWorkingDirectory::Downloads => Some(context.downloads),
        HookWorkingDirectory::Staging => context.staging,
        HookWorkingDirectory::Install => Some(context.install),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use super::wait_for_script;

    #[test]
    fn captures_hook_output_instead_of_writing_through_the_progress_display() {
        let child = Command::new("sh")
            .args(["-c", "printf hook-out; printf hook-error >&2; exit 7"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let error = wait_for_script(child, Path::new("hook.py"), 5).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("hook-out"));
        assert!(message.contains("hook-error"));
    }

    #[test]
    fn terminates_scripts_that_exceed_the_timeout() {
        let child = Command::new("sh")
            .args(["-c", "while :; do :; done"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let error = wait_for_script(child, Path::new("slow-hook.py"), 0).unwrap_err();

        assert!(error.to_string().contains("timed out after 0 seconds"));
    }

    #[test]
    fn does_not_wait_for_a_descendant_that_keeps_output_open() {
        let child = Command::new("sh")
            .args(["-c", "(sleep 2) & printf done"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let started = Instant::now();

        wait_for_script(child, Path::new("background-hook.py"), 5).unwrap();

        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
