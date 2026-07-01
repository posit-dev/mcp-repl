use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde::Deserialize;

pub(crate) const PYTHON_EXECUTABLE_ENV: &str = "MCP_REPL_PYTHON_EXECUTABLE";
pub(crate) const PYTHON_MODULE_SEARCH_PATH_ENV: &str = "MCP_REPL_PYTHON_MODULE_SEARCH_PATH";
const PYTHON_PROGRAM: &str = "python3";
const PYTHON_PROGRAM_FALLBACK: &str = "python";
const PYTHON_CONFIG_SNIPPET: &str = r#"
import json
import sys
import sysconfig

def var(name):
    value = sysconfig.get_config_var(name)
    return "" if value is None else str(value)

print(json.dumps({
    "executable": sys.executable,
    "base_executable": getattr(sys, "_base_executable", sys.executable),
    "path": sys.path,
    "prefix": sys.prefix,
    "base_prefix": sys.base_prefix,
    "exec_prefix": sys.exec_prefix,
    "base_exec_prefix": sys.base_exec_prefix,
    "version": [sys.version_info[0], sys.version_info[1]],
    "ldlibrary": var("LDLIBRARY"),
    "instsoname": var("INSTSONAME"),
    "libdir": var("LIBDIR"),
    "libpl": var("LIBPL"),
    "bindir": var("BINDIR"),
    "pythonframeworkprefix": var("PYTHONFRAMEWORKPREFIX"),
    "pythonframeworkinstalldir": var("PYTHONFRAMEWORKINSTALLDIR"),
}))
"#;

#[derive(Debug)]
pub(crate) struct PythonRuntimeConfig {
    pub(crate) executable: PathBuf,
    pub(crate) libpython: PathBuf,
    pub(crate) module_search_paths: Vec<PathBuf>,
}

pub(crate) trait PythonCommandRunner {
    fn output(&mut self, program: &Path, args: &[String]) -> Result<Output, String>;
}

pub(crate) struct DirectPythonCommandRunner;

impl PythonCommandRunner for DirectPythonCommandRunner {
    fn output(&mut self, program: &Path, args: &[String]) -> Result<Output, String> {
        Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|err| err.to_string())
    }
}

#[derive(Debug, Deserialize)]
struct PythonRuntimeProbe {
    executable: String,
    base_executable: String,
    path: Vec<String>,
    prefix: String,
    base_prefix: String,
    exec_prefix: String,
    base_exec_prefix: String,
    version: [u64; 2],
    ldlibrary: String,
    instsoname: String,
    libdir: String,
    libpl: String,
    #[cfg(windows)]
    bindir: String,
    pythonframeworkprefix: String,
    pythonframeworkinstalldir: String,
}

fn find_dot_venv_pythons(start: &Path) -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    // Search HOME itself, then stop. Do not ascend to HOME's parent.
    let stop_at_home = home
        .as_ref()
        .filter(|home| start.starts_with(home.as_path()))
        .cloned();
    let mut dir = start.to_path_buf();
    loop {
        let mut candidates = Vec::new();
        for candidate in [
            dir.join(".venv").join("bin").join("python"),
            dir.join(".venv").join("bin").join("python3"),
        ] {
            if candidate.is_file() {
                candidates.push(candidate);
            }
        }
        if !candidates.is_empty() {
            return candidates;
        }

        if let Some(stop) = stop_at_home.as_ref()
            && &dir == stop
        {
            break;
        }

        let Some(parent) = dir.parent() else {
            break;
        };
        if parent == dir {
            break;
        }
        dir = parent.to_path_buf();
    }
    Vec::new()
}

pub(crate) fn find_program_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for candidate in program_candidates_in_dir(&dir, name) {
            if !candidate.is_file() {
                continue;
            }

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = std::fs::metadata(&candidate)
                    && meta.permissions().mode() & 0o111 != 0
                {
                    return Some(candidate);
                }
            }

            #[cfg(not(unix))]
            {
                return Some(candidate);
            }
        }
    }
    None
}

fn program_candidates_in_dir(dir: &Path, name: &str) -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let path = Path::new(name);
        if path.extension().is_some() {
            return vec![dir.join(name)];
        }
        let pathext = std::env::var_os("PATHEXT")
            .map(|value| {
                value
                    .to_string_lossy()
                    .split(';')
                    .filter(|ext| !ext.is_empty())
                    .map(|ext| ext.to_string())
                    .collect::<Vec<_>>()
            })
            .filter(|exts| !exts.is_empty())
            .unwrap_or_else(|| vec![".COM".into(), ".EXE".into(), ".BAT".into(), ".CMD".into()]);
        let mut candidates = vec![dir.join(name)];
        candidates.extend(
            pathext
                .into_iter()
                .map(|ext| dir.join(format!("{name}{ext}"))),
        );
        candidates
    }
    #[cfg(not(windows))]
    {
        vec![dir.join(name)]
    }
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

fn python_program_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        for venv_python in find_dot_venv_pythons(&cwd) {
            push_unique_path(&mut candidates, venv_python);
        }
    }
    push_unique_path(
        &mut candidates,
        find_program_on_path(PYTHON_PROGRAM).unwrap_or_else(|| PathBuf::from(PYTHON_PROGRAM)),
    );
    push_unique_path(
        &mut candidates,
        find_program_on_path(PYTHON_PROGRAM_FALLBACK)
            .unwrap_or_else(|| PathBuf::from(PYTHON_PROGRAM_FALLBACK)),
    );
    candidates
}

pub(crate) fn query_python_runtime_config(
    executable: &Path,
) -> Result<PythonRuntimeConfig, String> {
    let mut runner = DirectPythonCommandRunner;
    query_python_runtime_config_with_runner(executable, &mut runner)
}

pub(crate) fn query_python_runtime_config_with_runner(
    executable: &Path,
    runner: &mut dyn PythonCommandRunner,
) -> Result<PythonRuntimeConfig, String> {
    let output = runner
        .output(
            executable,
            &[
                "-I".to_string(),
                "-c".to_string(),
                PYTHON_CONFIG_SNIPPET.to_string(),
            ],
        )
        .map_err(|err| {
            format!(
                "failed to query Python runtime config from {}: {err}",
                executable.display()
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "failed to query Python runtime config from {}: {}",
            executable.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let probe: PythonRuntimeProbe = serde_json::from_slice(&output.stdout).map_err(|err| {
        format!(
            "failed to parse Python runtime config from {}: {err}",
            executable.display()
        )
    })?;
    let libpython = resolve_libpython_path(&probe).ok_or_else(|| {
        format!(
            "failed to locate a shared libpython for {}",
            executable.display()
        )
    })?;
    let executable = first_non_empty([probe.executable.as_str(), probe.base_executable.as_str()])
        .map(PathBuf::from)
        .unwrap_or_else(|| executable.to_path_buf());
    let mut module_search_paths = Vec::new();
    push_unique_path(&mut module_search_paths, PathBuf::new());
    for path in &probe.path {
        push_unique_path(&mut module_search_paths, PathBuf::from(path));
    }
    Ok(PythonRuntimeConfig {
        executable,
        libpython,
        module_search_paths,
    })
}

#[cfg(test)]
fn select_python_program(
    mut candidates: Vec<PathBuf>,
    mut starts: impl FnMut(&Path) -> bool,
) -> PathBuf {
    if candidates.is_empty() {
        candidates.push(PathBuf::from(PYTHON_PROGRAM));
    }
    candidates
        .iter()
        .find(|candidate| starts(candidate))
        .cloned()
        .unwrap_or_else(|| candidates.remove(0))
}

fn select_python_runtime_config(
    executable_override: Option<PathBuf>,
    mut candidates: Vec<PathBuf>,
    mut query: impl FnMut(&Path) -> Result<PythonRuntimeConfig, String>,
) -> Result<PythonRuntimeConfig, String> {
    if let Some(executable) = executable_override {
        return query(&executable);
    }

    if candidates.is_empty() {
        candidates.push(PathBuf::from(PYTHON_PROGRAM));
    }

    let mut errors = Vec::new();
    for candidate in candidates {
        match query(&candidate) {
            Ok(config) => return Ok(config),
            Err(err) => errors.push(format!("{}: {err}", candidate.display())),
        }
    }

    Err(format!(
        "failed to query Python runtime config from candidate interpreters: {}",
        errors.join("; ")
    ))
}

pub(crate) fn resolve_python_runtime_config() -> Result<PythonRuntimeConfig, String> {
    let mut config = select_python_runtime_config(
        std::env::var_os(PYTHON_EXECUTABLE_ENV).map(PathBuf::from),
        python_program_candidates(),
        query_python_runtime_config,
    )?;
    if let Some(paths) = std::env::var_os(PYTHON_MODULE_SEARCH_PATH_ENV) {
        config.module_search_paths = std::env::split_paths(&paths).collect();
    } else {
        config.module_search_paths = Vec::new();
    }
    Ok(config)
}

fn resolve_libpython_path(probe: &PythonRuntimeProbe) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    push_python_library_candidates(&mut candidates, probe, &probe.ldlibrary);
    if probe.instsoname != probe.ldlibrary {
        push_python_library_candidates(&mut candidates, probe, &probe.instsoname);
    }
    push_windows_python_library_candidates(&mut candidates, probe);

    let version = format!("{}.{}", probe.version[0], probe.version[1]);
    for root in [
        probe.base_exec_prefix.as_str(),
        probe.exec_prefix.as_str(),
        probe.base_prefix.as_str(),
        probe.prefix.as_str(),
    ] {
        if root.is_empty() {
            continue;
        }
        candidates.push(
            Path::new(root)
                .join("lib")
                .join(format!("libpython{version}.so")),
        );
        candidates.push(
            Path::new(root)
                .join("lib")
                .join(format!("libpython{version}.dylib")),
        );
        candidates.push(Path::new(root).join("Python"));
    }

    candidates
        .into_iter()
        .find(|candidate| is_loadable_libpython_candidate(candidate))
}

fn is_loadable_libpython_candidate(candidate: &Path) -> bool {
    if !candidate.is_file() {
        return false;
    }
    !candidate
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("a") || extension.eq_ignore_ascii_case("lib")
        })
}

#[cfg(windows)]
fn push_windows_python_library_candidates(
    candidates: &mut Vec<PathBuf>,
    probe: &PythonRuntimeProbe,
) {
    let compact_version = format!("{}{}", probe.version[0], probe.version[1]);
    let library_names = [
        format!("python{compact_version}.dll"),
        "python3.dll".to_string(),
    ];

    for root in [
        Path::new(probe.executable.as_str()).parent(),
        Path::new(probe.base_executable.as_str()).parent(),
        non_empty(&probe.bindir).map(Path::new),
        non_empty(&probe.base_exec_prefix).map(Path::new),
        non_empty(&probe.exec_prefix).map(Path::new),
        non_empty(&probe.base_prefix).map(Path::new),
        non_empty(&probe.prefix).map(Path::new),
    ]
    .into_iter()
    .flatten()
    {
        for library in &library_names {
            candidates.push(root.join(library));
        }
    }
}

#[cfg(not(windows))]
fn push_windows_python_library_candidates(
    _candidates: &mut Vec<PathBuf>,
    _probe: &PythonRuntimeProbe,
) {
}

fn push_python_library_candidates(
    candidates: &mut Vec<PathBuf>,
    probe: &PythonRuntimeProbe,
    library: &str,
) {
    let Some(library) = non_empty(library) else {
        return;
    };
    let path = Path::new(library);
    if path.is_absolute() {
        candidates.push(path.to_path_buf());
    }
    for executable in [probe.executable.as_str(), probe.base_executable.as_str()] {
        let Some(executable) = non_empty(executable) else {
            continue;
        };
        let Some(parent) = Path::new(executable).parent() else {
            continue;
        };
        candidates.push(parent.join(library));
    }
    for root in [
        probe.libdir.as_str(),
        probe.libpl.as_str(),
        probe.pythonframeworkprefix.as_str(),
        probe.pythonframeworkinstalldir.as_str(),
    ] {
        if let Some(root) = non_empty(root) {
            candidates.push(Path::new(root).join(library));
        }
    }
}

fn first_non_empty<'a>(values: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    values.into_iter().find_map(non_empty)
}

fn non_empty(value: &str) -> Option<&str> {
    let value = value.trim();
    if value.is_empty() { None } else { Some(value) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime_config_for(path: &str) -> PythonRuntimeConfig {
        PythonRuntimeConfig {
            executable: PathBuf::from(path),
            libpython: PathBuf::from("python.dll"),
            module_search_paths: Vec::new(),
        }
    }

    fn runtime_probe_for_libpython(
        executable: &Path,
        version: [u64; 2],
        ldlibrary: &Path,
        libdir: &Path,
        prefix: &Path,
    ) -> PythonRuntimeProbe {
        PythonRuntimeProbe {
            executable: executable.to_string_lossy().into_owned(),
            base_executable: executable.to_string_lossy().into_owned(),
            path: Vec::new(),
            prefix: prefix.to_string_lossy().into_owned(),
            base_prefix: prefix.to_string_lossy().into_owned(),
            exec_prefix: prefix.to_string_lossy().into_owned(),
            base_exec_prefix: prefix.to_string_lossy().into_owned(),
            version,
            ldlibrary: ldlibrary.to_string_lossy().into_owned(),
            instsoname: ldlibrary.to_string_lossy().into_owned(),
            libdir: libdir.to_string_lossy().into_owned(),
            libpl: libdir.to_string_lossy().into_owned(),
            #[cfg(windows)]
            bindir: String::new(),
            pythonframeworkprefix: String::new(),
            pythonframeworkinstalldir: String::new(),
        }
    }

    #[test]
    fn python_program_selection_falls_back_after_broken_python3_candidate() {
        let selected = select_python_program(
            vec![PathBuf::from("python3"), PathBuf::from("python")],
            |candidate| candidate == Path::new("python"),
        );

        assert_eq!(selected, PathBuf::from("python"));
    }

    #[test]
    fn python_runtime_config_falls_back_after_broken_python3_candidate() {
        let mut attempts = Vec::new();

        let config = select_python_runtime_config(
            None,
            vec![PathBuf::from("python3"), PathBuf::from("python")],
            |candidate| {
                attempts.push(candidate.to_path_buf());
                if candidate == Path::new("python3") {
                    Err("store alias is not a usable interpreter".to_string())
                } else {
                    Ok(runtime_config_for("python"))
                }
            },
        )
        .expect("python fallback should be used after python3 fails");

        assert_eq!(
            attempts,
            vec![PathBuf::from("python3"), PathBuf::from("python")]
        );
        assert_eq!(config.executable, PathBuf::from("python"));
    }

    #[test]
    fn python_runtime_config_env_override_does_not_fallback() {
        let mut attempts = Vec::new();

        let err = select_python_runtime_config(
            Some(PathBuf::from("custom-python")),
            vec![PathBuf::from("python3"), PathBuf::from("python")],
            |candidate| {
                attempts.push(candidate.to_path_buf());
                Err(format!("{} is not usable", candidate.display()))
            },
        )
        .expect_err("explicit Python override should not fall back");

        assert_eq!(attempts, vec![PathBuf::from("custom-python")]);
        assert!(err.contains("custom-python is not usable"));
    }

    #[test]
    fn resolve_libpython_path_skips_static_archive_candidate() {
        let temp = tempfile::tempdir().expect("tempdir");
        let bin = temp.path().join("bin");
        let lib = temp.path().join("lib");
        std::fs::create_dir_all(&bin).expect("bin dir");
        std::fs::create_dir_all(&lib).expect("lib dir");
        let executable = bin.join("python3");
        let archive = lib.join("libpython3.11.a");
        let shared = lib.join("libpython3.11.so");
        std::fs::write(&executable, "").expect("python placeholder");
        std::fs::write(&archive, "!<arch>\n").expect("archive placeholder");
        std::fs::write(&shared, "").expect("shared placeholder");

        let probe = runtime_probe_for_libpython(&executable, [3, 11], &archive, &lib, temp.path());

        assert_eq!(resolve_libpython_path(&probe), Some(shared));
    }

    #[cfg(windows)]
    #[test]
    fn resolve_libpython_path_finds_windows_dll_next_to_executable() {
        fn runtime_probe_for(executable: &str) -> PythonRuntimeProbe {
            PythonRuntimeProbe {
                executable: executable.to_string(),
                base_executable: executable.to_string(),
                path: Vec::new(),
                prefix: String::new(),
                base_prefix: String::new(),
                exec_prefix: String::new(),
                base_exec_prefix: String::new(),
                version: [3, 11],
                ldlibrary: String::new(),
                instsoname: String::new(),
                libdir: String::new(),
                libpl: String::new(),
                bindir: String::new(),
                pythonframeworkprefix: String::new(),
                pythonframeworkinstalldir: String::new(),
            }
        }

        let temp = tempfile::tempdir().expect("tempdir");
        let python = temp.path().join("python.exe");
        let dll = temp.path().join("python311.dll");
        std::fs::write(&python, "").expect("python placeholder");
        std::fs::write(&dll, "").expect("dll placeholder");

        let probe = runtime_probe_for(&python.to_string_lossy());

        assert_eq!(resolve_libpython_path(&probe), Some(dll));
    }
}
