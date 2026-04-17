mod common;

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::Command;

    use crate::common::TestResult;
    use tempfile::tempdir;

    fn repo_root() -> &'static Path {
        Path::new(env!("CARGO_MANIFEST_DIR"))
    }

    fn write_executable(path: &Path, contents: &str) -> TestResult<()> {
        fs::write(path, contents)?;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms)?;
        Ok(())
    }

    fn make_fake_uname(bin_dir: &Path) -> TestResult<()> {
        write_executable(
            &bin_dir.join("uname"),
            r#"#!/bin/sh
case "$1" in
  -s) echo Linux ;;
  -m) echo x86_64 ;;
  *) exit 1 ;;
esac
"#,
        )
    }

    fn make_fake_getconf(bin_dir: &Path, body: &str) -> TestResult<()> {
        write_executable(&bin_dir.join("getconf"), &format!("#!/bin/sh\n{body}\n"))
    }

    fn make_fake_curl_failing(bin_dir: &Path) -> TestResult<()> {
        write_executable(
            &bin_dir.join("curl"),
            r#"#!/bin/sh
echo "curl called" >&2
exit 99
"#,
        )
    }

    fn make_fake_curl_copying_archive(bin_dir: &Path) -> TestResult<()> {
        write_executable(
            &bin_dir.join("curl"),
            r#"#!/bin/sh
out=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o)
      out="$2"
      shift 2
      ;;
    *)
      shift
      ;;
  esac
done
cp "$FAKE_ARCHIVE_SOURCE" "$out"
"#,
        )
    }

    fn make_fake_release_archive(temp: &Path, binary_body: &str) -> TestResult<std::path::PathBuf> {
        let package_dir = temp.join("mcp-repl-x86_64-unknown-linux-gnu");
        fs::create_dir(&package_dir)?;
        write_executable(&package_dir.join("mcp-repl"), binary_body)?;
        fs::write(package_dir.join("README.md"), "README")?;
        fs::write(package_dir.join("LICENSE"), "LICENSE")?;

        let archive_path = temp.join("mcp-repl-x86_64-unknown-linux-gnu.tar.gz");
        let status = Command::new("tar")
            .arg("-czf")
            .arg(&archive_path)
            .arg("-C")
            .arg(temp)
            .arg("mcp-repl-x86_64-unknown-linux-gnu")
            .status()?;
        assert!(
            status.success(),
            "expected tar to build fake release archive"
        );
        Ok(archive_path)
    }

    fn run_install_script(getconf_body: &str) -> TestResult<std::process::Output> {
        let temp = tempdir()?;
        let bin_dir = temp.path().join("bin");
        fs::create_dir(&bin_dir)?;
        make_fake_uname(&bin_dir)?;
        make_fake_getconf(&bin_dir, getconf_body)?;
        make_fake_curl_failing(&bin_dir)?;

        let path = format!("{}:/usr/bin:/bin", bin_dir.display());
        let script = repo_root().join("scripts/install.sh");
        let output = Command::new("sh")
            .arg(&script)
            .env("HOME", temp.path())
            .env("PATH", path)
            .output()?;
        Ok(output)
    }

    #[test]
    fn install_script_rejects_linux_glibc_older_than_2_35() -> TestResult<()> {
        let output = run_install_script(
            r#"if [ "$1" = "GNU_LIBC_VERSION" ]; then
  echo "glibc 2.31"
  exit 0
fi
exit 1"#,
        )?;
        assert!(
            !output.status.success(),
            "expected old glibc runtime to be rejected"
        );
        let stderr = String::from_utf8(output.stderr)?;
        assert!(
            stderr.contains("unsupported glibc version: 2.31"),
            "expected old glibc error, got: {stderr:?}"
        );
        assert!(
            !stderr.contains("curl called"),
            "expected install script to fail before download, got: {stderr:?}"
        );
        Ok(())
    }

    fn run_install_script_with_archive(
        getconf_body: &str,
        binary_body: &str,
    ) -> TestResult<(std::process::Output, tempfile::TempDir)> {
        let temp = tempdir()?;
        let bin_dir = temp.path().join("bin");
        fs::create_dir(&bin_dir)?;
        make_fake_uname(&bin_dir)?;
        make_fake_getconf(&bin_dir, getconf_body)?;
        make_fake_curl_copying_archive(&bin_dir)?;
        let archive_path = make_fake_release_archive(temp.path(), binary_body)?;

        let path = format!("{}:/usr/bin:/bin", bin_dir.display());
        let script = repo_root().join("scripts/install.sh");
        let output = Command::new("sh")
            .arg(&script)
            .env("HOME", temp.path())
            .env("PATH", path)
            .env("FAKE_ARCHIVE_SOURCE", archive_path)
            .output()?;
        Ok((output, temp))
    }

    #[test]
    fn install_script_succeeds_without_getconf_when_binary_runs() -> TestResult<()> {
        let (output, temp) = run_install_script_with_archive(
            "exit 127",
            r#"#!/bin/sh
if [ "$1" = "--help" ]; then
  echo "help"
  exit 0
fi
exit 1
"#,
        )?;
        assert!(
            output.status.success(),
            "expected install to succeed without getconf, got: {output:?}"
        );
        assert!(
            temp.path().join(".local/bin/mcp-repl").exists(),
            "expected binary to be installed"
        );
        Ok(())
    }

    #[test]
    fn install_script_succeeds_when_getconf_output_is_unrecognized_but_binary_runs()
    -> TestResult<()> {
        let (output, temp) = run_install_script_with_archive(
            r#"if [ "$1" = "GNU_LIBC_VERSION" ]; then
  echo "something-unexpected"
  exit 0
fi
exit 1"#,
            r#"#!/bin/sh
if [ "$1" = "--help" ]; then
  echo "help"
  exit 0
fi
exit 1
"#,
        )?;
        assert!(
            output.status.success(),
            "expected install to succeed on unrecognized getconf output, got: {output:?}"
        );
        assert!(
            temp.path().join(".local/bin/mcp-repl").exists(),
            "expected binary to be installed"
        );
        Ok(())
    }

    #[test]
    fn install_script_rejects_incompatible_binary_before_install_when_getconf_is_missing()
    -> TestResult<()> {
        let (output, temp) = run_install_script_with_archive(
            "exit 127",
            r#"#!/bin/sh
echo "binary incompatible" >&2
exit 1
"#,
        )?;
        assert!(
            !output.status.success(),
            "expected install to fail when the extracted binary does not start"
        );
        let stderr = String::from_utf8(output.stderr)?;
        assert!(
            stderr.contains("binary incompatible"),
            "expected extracted binary failure to be visible, got: {stderr:?}"
        );
        assert!(
            !temp.path().join(".local/bin/mcp-repl").exists(),
            "expected install script to avoid installing an incompatible binary"
        );
        Ok(())
    }
}
