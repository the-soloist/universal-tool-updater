use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::domain::{EnvironmentMode, HookWorkingDirectory, Tool};

use super::HookContext;

/// Hook parameters bundled to keep the interpreter runner's signature small.
pub(super) struct PythonHookSpec<'a> {
    pub(super) script: &'a Path,
    pub(super) args: &'a [String],
    pub(super) timeout_seconds: u64,
    pub(super) working_directory: HookWorkingDirectory,
    pub(super) environment_mode: EnvironmentMode,
    pub(super) environment: &'a BTreeMap<String, String>,
}

/// A resolved interpreter: the program plus its fixed launcher arguments
/// (for example `py` with `-3`).
type ResolvedPython = (OsString, Vec<OsString>);

/// Run-scoped memo of the resolved Python 3 interpreter. The resolution
/// depends only on the process environment and the run's root paths, so the
/// first success is remembered; aggregate failures are never cached, and a
/// remembered interpreter that later fails to spawn is dropped so the full
/// candidate loop runs again.
#[derive(Default)]
pub(super) struct PythonCache {
    resolved: OnceLock<Mutex<Option<ResolvedPython>>>,
}

impl PythonCache {
    fn slot(&self) -> &Mutex<Option<ResolvedPython>> {
        self.resolved.get_or_init(|| Mutex::new(None))
    }

    fn load(&self) -> Option<ResolvedPython> {
        self.slot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn remember(&self, value: ResolvedPython) {
        *self
            .slot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(value);
    }

    fn forget(&self) {
        *self
            .slot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

/// Hook parameters resolved to concrete paths for the spawn loop.
struct PreparedHook<'a> {
    script: &'a Path,
    args: &'a [String],
    timeout_seconds: u64,
    cwd: &'a Path,
    environment_mode: EnvironmentMode,
    environment: &'a BTreeMap<String, String>,
}

pub(super) fn run(
    cache: &PythonCache,
    spec: PythonHookSpec<'_>,
    tool: &Tool,
    context: &HookContext<'_>,
) -> Result<()> {
    let PythonHookSpec {
        script,
        args,
        timeout_seconds,
        working_directory,
        environment_mode,
        environment,
    } = spec;
    let script = context.app_root.join(script);
    let cwd = working_directory_path(working_directory, context).ok_or_else(|| {
        anyhow::anyhow!("working directory {working_directory:?} is unavailable at this stage")
    })?;
    resolve_and_run(
        cache,
        &PreparedHook {
            script: &script,
            args,
            timeout_seconds,
            cwd,
            environment_mode,
            environment,
        },
        python_candidates(),
        tool,
        context,
    )
}

fn resolve_and_run(
    cache: &PythonCache,
    hook: &PreparedHook<'_>,
    candidates: Vec<PythonInterpreter>,
    tool: &Tool,
    context: &HookContext<'_>,
) -> Result<()> {
    // Memoized fast path: spawn the remembered interpreter directly. A
    // NotFound failure means it vanished mid-run; drop the memo and fall
    // through to the full candidate loop.
    if let Some(resolved) = cache.load() {
        match spawn_interpreter(&resolved, hook, tool, context) {
            Ok(child) => return wait_for_script(child, hook.script, hook.timeout_seconds),
            Err(error) if error.kind() == ErrorKind::NotFound => cache.forget(),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("cannot execute Python interpreter {:?}", resolved.0)
                });
            }
        }
    }
    let mut missing = Vec::new();
    let mut incompatible = Vec::new();
    let mut rejected = Vec::new();
    let path_var = std::env::var_os("PATH");
    for interpreter in candidates {
        if let Some(resolved) =
            rejected_interpreter_path(&interpreter.program, path_var.as_deref(), context)
        {
            tracing::warn!(
                interpreter = %resolved.display(),
                "skipping Python interpreter inside a managed directory; PATH entries under the toolkit or downloads directories can be poisoned by tool content"
            );
            rejected.push(interpreter.name());
            continue;
        }
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

        let resolved = interpreter.resolved();
        match spawn_interpreter(&resolved, hook, tool, context) {
            Ok(child) => {
                cache.remember(resolved);
                return wait_for_script(child, hook.script, hook.timeout_seconds);
            }
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
        "Python 3 interpreter not found; missing [{}], incompatible [{}], rejected [{}]; set UTU_PYTHON to a Python 3 interpreter path outside the toolkit and downloads directories",
        missing.join(", "),
        incompatible.join(", "),
        rejected.join(", ")
    )
}

/// Builds the interpreter command line for one hook invocation and spawns it.
fn spawn_interpreter(
    resolved: &ResolvedPython,
    hook: &PreparedHook<'_>,
    tool: &Tool,
    context: &HookContext<'_>,
) -> std::io::Result<Child> {
    let mut command = Command::new(&resolved.0);
    command.args(&resolved.1);
    if hook.environment_mode == EnvironmentMode::Minimal {
        apply_minimal_environment(&mut command);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command
        .arg(hook.script)
        .args(hook.args)
        .current_dir(hook.cwd)
        .envs(hook.environment)
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
    command.spawn()
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
            kill_process_tree(&mut child);
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

/// Terminates the script together with any processes it spawned. The direct
/// child is always killed as a fallback so the subsequent wait cannot hang.
fn kill_process_tree(child: &mut std::process::Child) {
    let pid = child.id();
    #[cfg(windows)]
    let terminated = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .status()
        .is_ok_and(|status| status.success());
    #[cfg(unix)]
    let terminated = Command::new("kill")
        .args(["-9", &format!("-{pid}")])
        .status()
        .is_ok_and(|status| status.success());
    if !terminated {
        let _ = child.kill();
    }
}

/// Replaces the inherited environment with a small allow-list; the reserved
/// UTU_* variables and the configured `environment` map are applied on top.
fn apply_minimal_environment(command: &mut Command) {
    command.env_clear();
    for name in minimal_environment_names() {
        if let Ok(value) = std::env::var(name) {
            command.env(name, value);
        }
    }
}

fn minimal_environment_names() -> &'static [&'static str] {
    #[cfg(windows)]
    {
        &["PATH", "SYSTEMROOT", "TEMP", "TMP", "TZ"]
    }
    #[cfg(not(windows))]
    {
        &["PATH", "TEMP", "TMP", "TZ"]
    }
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
    // 子进程可能在脚本退出后继承管道；限制排空等待时间，避免无关后代进程让更新器无限期挂起。
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
        // Known limitation: the abandoned reader thread stays alive until the
        // OS reclaims it once the script's descendant processes exit and close
        // the pipe; a Job Object would be needed to force collection.
        tracing::debug!(
            stream,
            "Python {stream} reader thread did not finish; abandoning it, it is reclaimed once the script's descendant processes exit"
        );
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

    fn resolved(&self) -> ResolvedPython {
        (self.program.clone(), self.launcher_args.clone())
    }

    fn probe(&self) -> std::io::Result<bool> {
        self.command()
            .arg("-c")
            .arg("import sys; raise SystemExit(sys.version_info.major != 3)")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
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

/// Resolves the interpreter binary that spawning `program` would actually
/// execute: paths with a directory component are canonicalized directly,
/// bare names are searched on the provided PATH (`path_var` is injected so
/// the lookup stays testable), appending `.exe` on Windows to match the OS
/// loader. Unresolvable programs yield None and fall back to the plain
/// probe-and-spawn flow.
fn resolve_program_path(program: &OsStr, path_var: Option<&OsStr>) -> Option<PathBuf> {
    let program = Path::new(program);
    if program
        .parent()
        .is_some_and(|parent| !parent.as_os_str().is_empty())
    {
        return fs::canonicalize(program).ok();
    }
    let path_var = path_var?;
    std::env::split_paths(path_var)
        .find_map(|directory| probe_program_in(&directory, program))
        .and_then(|candidate| fs::canonicalize(&candidate).ok())
}

fn probe_program_in(directory: &Path, program: &Path) -> Option<PathBuf> {
    let direct = directory.join(program);
    if direct.is_file() {
        return Some(direct);
    }
    #[cfg(windows)]
    if program.extension().is_none() {
        let executable = directory.join(format!("{}.exe", program.to_string_lossy()));
        if executable.is_file() {
            return Some(executable);
        }
    }
    None
}

/// Returns the canonicalized interpreter path when the candidate resolves
/// inside a directory the updater itself manages, marking it as rejected.
fn rejected_interpreter_path(
    program: &OsStr,
    path_var: Option<&OsStr>,
    context: &HookContext<'_>,
) -> Option<PathBuf> {
    resolve_program_path(program, path_var).filter(|resolved| {
        [context.toolkit_root, context.downloads]
            .iter()
            .any(|root| {
                let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
                resolved.starts_with(canonical)
            })
    })
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

#[cfg(test)]
mod path_boundary_tests {
    use std::ffi::OsStr;
    use std::fs;

    use tempfile::tempdir;

    use super::{rejected_interpreter_path, resolve_program_path};
    use crate::hooks::HookContext;

    fn context<'a>(
        toolkit: &'a std::path::Path,
        downloads: &'a std::path::Path,
    ) -> HookContext<'a> {
        HookContext {
            app_root: toolkit,
            toolkit_root: toolkit,
            downloads,
            staging: None,
            install: toolkit,
            version: None,
        }
    }

    #[test]
    fn resolves_bare_program_names_through_the_provided_path() {
        let root = tempdir().unwrap();
        let toolkit = root.path().join("toolkit");
        let system = root.path().join("system");
        fs::create_dir_all(&toolkit).unwrap();
        fs::create_dir_all(&system).unwrap();
        fs::write(system.join("python3"), b"").unwrap();
        let path_var = std::env::join_paths([&toolkit, &system]).unwrap();

        let resolved = resolve_program_path(OsStr::new("python3"), Some(&path_var)).unwrap();
        assert!(resolved.ends_with("python3"));
        assert!(
            rejected_interpreter_path(
                OsStr::new("python3"),
                Some(&path_var),
                &context(&toolkit, &root.path().join("downloads"))
            )
            .is_none(),
            "an interpreter outside the managed roots must not be rejected"
        );
    }

    #[cfg(windows)]
    #[test]
    fn appends_the_executable_extension_when_searching_the_path() {
        let root = tempdir().unwrap();
        let bin = root.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("python.exe"), b"").unwrap();
        let path_var = std::env::join_paths([&bin]).unwrap();

        let resolved = resolve_program_path(OsStr::new("python"), Some(&path_var)).unwrap();
        assert!(resolved.ends_with("python.exe"));
    }

    #[test]
    fn skips_candidates_resolved_inside_managed_roots() {
        let root = tempdir().unwrap();
        let toolkit = root.path().join("toolkit");
        let downloads = root.path().join("downloads");
        let toolkit_bin = toolkit.join("bin");
        let downloads_bin = downloads.join("bin");
        fs::create_dir_all(&toolkit_bin).unwrap();
        fs::create_dir_all(&downloads_bin).unwrap();
        // PATH search matches the bare name on Unix and appends .exe on Windows.
        let program = if cfg!(windows) {
            "python.exe"
        } else {
            "python"
        };
        fs::write(toolkit_bin.join(program), b"").unwrap();
        fs::write(downloads_bin.join(program), b"").unwrap();
        let path_var = std::env::join_paths([&toolkit_bin, &downloads_bin]).unwrap();
        let hook_context = context(&toolkit, &downloads);

        let rejected =
            rejected_interpreter_path(OsStr::new("python"), Some(&path_var), &hook_context)
                .unwrap();
        assert!(
            rejected.starts_with(toolkit.canonicalize().unwrap()),
            "the toolkit-poisoned candidate must be the one reported, got {}",
            rejected.display()
        );
    }
}

#[cfg(test)]
mod memoization_tests {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::thread;

    use tempfile::tempdir;

    use super::{PreparedHook, PythonCache, PythonInterpreter, ResolvedPython, resolve_and_run};
    use crate::domain::{EnvironmentMode, Tool};
    use crate::hooks::HookContext;
    use crate::test_support::tool as test_tool;

    static EMPTY_ENVIRONMENT: BTreeMap<String, String> = BTreeMap::new();

    /// Writes a stub executable that appends one line to `calls.log` next to
    /// itself and exits successfully, ignoring its arguments so both the
    /// probe (`-c ...`) and the script invocation succeed.
    fn write_stub(directory: &Path) -> PathBuf {
        #[cfg(windows)]
        let (path, contents) = (
            directory.join("stub-python.bat"),
            "@echo x>> \"%~dp0calls.log\"\r\n@exit /b 0\r\n".to_owned(),
        );
        #[cfg(not(windows))]
        let (path, contents) = (
            directory.join("stub-python.sh"),
            "#!/bin/sh\necho x >> \"$(dirname \"$0\")/calls.log\"\nexit 0\n".to_owned(),
        );
        fs::write(&path, contents).unwrap();
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    fn stub_interpreter(program: &Path) -> PythonInterpreter {
        PythonInterpreter {
            program: OsString::from(program),
            launcher_args: Vec::new(),
        }
    }

    fn call_count(directory: &Path) -> usize {
        fs::read_to_string(directory.join("calls.log"))
            .map(|log| log.lines().filter(|line| !line.trim().is_empty()).count())
            .unwrap_or(0)
    }

    fn run_hook(
        cache: &PythonCache,
        script: &Path,
        root: &Path,
        candidates: Vec<PythonInterpreter>,
        tool: &Tool,
    ) -> anyhow::Result<()> {
        // The managed roots must stay clear of the stub directories so the
        // poisoned-PATH guard does not reject the stub interpreters.
        let toolkit = root.join("toolkit");
        let downloads = root.join("downloads");
        fs::create_dir_all(&toolkit).unwrap();
        fs::create_dir_all(&downloads).unwrap();
        resolve_and_run(
            cache,
            &PreparedHook {
                script,
                args: &[],
                timeout_seconds: 30,
                cwd: root,
                environment_mode: EnvironmentMode::Inherit,
                environment: &EMPTY_ENVIRONMENT,
            },
            candidates,
            tool,
            &HookContext {
                app_root: root,
                toolkit_root: &toolkit,
                downloads: &downloads,
                staging: None,
                install: &toolkit,
                version: None,
            },
        )
    }

    fn hook_script(root: &Path) -> PathBuf {
        let script = root.join("hook.py");
        fs::write(&script, b"").unwrap();
        script
    }

    #[test]
    fn probes_once_across_multiple_runs() {
        let root = tempdir().unwrap();
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let stub = write_stub(&bin);
        let script = hook_script(root.path());
        let tool = test_tool("memo", root.path().join("Memo"));
        let cache = PythonCache::default();

        for _ in 0..3 {
            run_hook(
                &cache,
                &script,
                root.path(),
                vec![stub_interpreter(&stub)],
                &tool,
            )
            .unwrap();
        }

        assert_eq!(
            call_count(&bin),
            4,
            "3 script runs must share a single probe invocation"
        );
    }

    #[test]
    fn reprobes_the_candidate_list_after_the_cached_interpreter_disappears() {
        let root = tempdir().unwrap();
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let stub = write_stub(&bin);
        let script = hook_script(root.path());
        let tool = test_tool("memo", root.path().join("Memo"));
        let cache = PythonCache::default();

        // Seed the memo with an interpreter whose binary is gone, which
        // makes the cached spawn fail with NotFound on every platform.
        let vanished = root.path().join("vanished/utu-missing-python.exe");
        cache.remember((vanished.into(), Vec::new()));

        run_hook(
            &cache,
            &script,
            root.path(),
            vec![stub_interpreter(&stub)],
            &tool,
        )
        .unwrap();

        let resolved: ResolvedPython = (stub.clone().into(), Vec::new());
        assert_eq!(
            cache.load(),
            Some(resolved),
            "the stale memo must be replaced by the re-resolved interpreter"
        );
        assert_eq!(
            call_count(&bin),
            2,
            "the fallback must run the full probe + script loop"
        );
    }

    #[test]
    fn aggregate_failures_are_not_cached() {
        let root = tempdir().unwrap();
        let script = hook_script(root.path());
        let tool = test_tool("memo", root.path().join("Memo"));
        let cache = PythonCache::default();

        let error = run_hook(
            &cache,
            &script,
            root.path(),
            vec![stub_interpreter(Path::new("utu-no-such-python"))],
            &tool,
        )
        .unwrap_err();

        let message = format!("{error:#}");
        assert!(
            message.contains("Python 3 interpreter not found"),
            "expected the aggregate failure text, got {message}"
        );
        assert!(
            message.contains("missing [utu-no-such-python]"),
            "expected the missing list, got {message}"
        );
        assert!(
            cache.load().is_none(),
            "aggregate failures must not be memoized"
        );
    }

    #[test]
    fn concurrent_first_lookups_share_one_resolution_without_deadlocking() {
        let root = tempdir().unwrap();
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let stub = write_stub(&bin);
        let script = hook_script(root.path());
        let root = root.path();
        let script = script.as_path();
        let stub = stub.as_path();
        let cache = std::sync::Arc::new(PythonCache::default());

        thread::scope(|scope| {
            for _ in 0..4 {
                let cache = std::sync::Arc::clone(&cache);
                scope.spawn(move || {
                    run_hook(
                        &cache,
                        script,
                        root,
                        vec![stub_interpreter(stub)],
                        &test_tool("memo", root.join("Memo")),
                    )
                    .unwrap();
                });
            }
        });
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
    fn kills_the_whole_process_group_on_timeout() {
        use std::os::unix::process::CommandExt;

        let mut command = Command::new("sh");
        command
            .args(["-c", "(sleep 30) & printf started; wait"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let child = command.spawn().unwrap();

        let error = wait_for_script(child, Path::new("group-hook.py"), 0).unwrap_err();

        assert!(error.to_string().contains("timed out after 0 seconds"));
        assert!(
            !error
                .to_string()
                .contains("[output capture still open after script exit]"),
            "a descendant kept the output pipe open: {error:#}"
        );
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
