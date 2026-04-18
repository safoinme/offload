//! Pytest framework implementation using `pytest --collect-only` for discovery.

use std::path::PathBuf;

use async_trait::async_trait;

use super::{
    FrameworkError, FrameworkResult, TestFramework, TestInstance, TestRecord,
    discovery_error_detail,
};
use crate::config::PytestFrameworkConfig;
use crate::provider::Command;

/// Test framework for Python pytest projects.
///
/// Uses `pytest --collect-only -q` for test discovery and generates
/// commands with JUnit XML output for structured result parsing.
///
/// # Configuration
///
/// See [`PytestFrameworkConfig`] for available options including:
/// - `paths`: Directories to search
/// - `command`: Full pytest invocation command
/// - `run_args`: Extra arguments for execution only
pub struct PytestFramework {
    config: PytestFrameworkConfig,
    /// The program to invoke (first token of `command`).
    program: String,
    /// Additional arguments parsed from `command` (tokens after the program).
    prefix_args: Vec<String>,
}

impl PytestFramework {
    /// Creates a new pytest framework, validating the command at construction time.
    pub fn new(config: PytestFrameworkConfig) -> FrameworkResult<Self> {
        let mut parts = shell_words::split(&config.command).map_err(|e| {
            FrameworkError::DiscoveryFailed(format!(
                "Failed to parse command '{}': {}",
                config.command, e
            ))
        })?;

        if parts.is_empty() {
            return Err(FrameworkError::DiscoveryFailed(format!(
                "Command '{}' produced no tokens after parsing",
                config.command
            )));
        }

        let program = parts.remove(0);
        let prefix_args = parts;

        Ok(Self {
            config,
            program,
            prefix_args,
        })
    }

    /// Parse `pytest --collect-only -q` output to extract test records.
    ///
    /// `-q` prints one test id per line (e.g.
    /// `tests/test_foo.py::test_bar` or
    /// `tests/test_foo.py::test_bar[param with spaces]`) plus a trailing
    /// `N tests collected` summary, blank lines, and any pytest warnings or
    /// error blocks (`<SomeError ...>`). Parametrize ids can legitimately
    /// contain whitespace, `-`, `/`, `:`, and other punctuation — the only
    /// reliable marker is the `::` separator between file path and node id
    /// combined with a non-space first character (so we skip the indented
    /// summary / warning lines that pytest emits).
    fn parse_collect_output(&self, output: &str, group: &str) -> Vec<TestRecord> {
        let mut tests = Vec::new();

        for line in output.lines() {
            // Use the raw line rather than `trim()`, so that indented
            // non-test output (warnings, tracebacks, the "N tests collected"
            // summary) is skipped via the "starts with whitespace" check.
            let first_char = line.chars().next();
            if !matches!(first_char, Some(c) if !c.is_whitespace()) {
                continue;
            }
            if line.starts_with('<') {
                // `<Module ... >`, `<ErrorRepr ...>`, etc. from pytest.
                continue;
            }
            // A real test id must contain `::` separating the file path
            // from the node id.
            if !line.contains("::") {
                continue;
            }
            tests.push(TestRecord::new(line, group));
        }

        tests
    }
}

#[async_trait]
impl TestFramework for PytestFramework {
    async fn discover(
        &self,
        paths: &[PathBuf],
        filters: &str,
        group: &str,
    ) -> FrameworkResult<Vec<TestRecord>> {
        // Build the pytest --collect-only command
        let mut cmd = tokio::process::Command::new(&self.program);
        for arg in &self.prefix_args {
            cmd.arg(arg);
        }
        cmd.arg("--collect-only").arg("-q");

        // Add filters if provided
        if !filters.is_empty() {
            let args = shell_words::split(filters).map_err(|e| {
                FrameworkError::DiscoveryFailed(format!(
                    "Invalid filter string '{}': {}",
                    filters, e
                ))
            })?;
            for arg in args {
                cmd.arg(arg);
            }
        }

        // Add paths to search
        let search_paths: Vec<_> = if paths.is_empty() {
            self.config
                .paths
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect()
        } else {
            paths
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect()
        };

        for path in &search_paths {
            cmd.arg(path);
        }

        // Build a display string for the command before running it
        let mut cmd_parts: Vec<&str> = Vec::new();
        cmd_parts.push(&self.program);
        for arg in &self.prefix_args {
            cmd_parts.push(arg);
        }
        cmd_parts.push("--collect-only");
        cmd_parts.push("-q");
        let filter_display: String;
        if !filters.is_empty() {
            filter_display = filters.to_string();
            cmd_parts.push(&filter_display);
        }
        for path in &search_paths {
            cmd_parts.push(path);
        }
        let cmd_display = cmd_parts.join(" ");

        let output = cmd
            .output()
            .await
            .map_err(|e| FrameworkError::DiscoveryFailed(e.to_string()))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() && !stdout.contains("::") {
            let detail = discovery_error_detail(&stderr, &stdout);
            return Err(FrameworkError::DiscoveryFailed(format!(
                "pytest --collect-only failed ({}):\n  command: {}\n  {}",
                output.status, cmd_display, detail
            )));
        }

        let tests = self.parse_collect_output(&stdout, group);

        if tests.is_empty() {
            tracing::warn!(
                "No tests discovered. Output: {}",
                discovery_error_detail(&stderr, &stdout)
            );
        }

        Ok(tests)
    }

    fn produce_test_execution_command(
        &self,
        tests: &[TestInstance],
        result_path: &str,
        fail_fast: bool,
    ) -> Command {
        let mut cmd = Command::new(&self.program);
        for arg in &self.prefix_args {
            cmd = cmd.arg(arg);
        }

        cmd = cmd
            .arg("-v")
            .arg("--tb=short")
            .arg(format!("--junitxml={}", result_path));

        if fail_fast {
            cmd = cmd.arg("-x");
        }

        // Append run_args for test execution only (not discovery)
        if let Some(run_args) = &self.config.run_args {
            match shell_words::split(run_args) {
                Ok(args) => {
                    for arg in args {
                        cmd = cmd.arg(arg);
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to parse run_args '{}': {}", run_args, e);
                }
            }
        }

        // Add test IDs
        for test in tests {
            cmd = cmd.arg(test.id());
        }

        cmd
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PytestFrameworkConfig;
    use crate::framework::TestInstance;

    #[test]
    fn test_command_prefix_with_command() -> Result<(), Box<dyn std::error::Error>> {
        let config = PytestFrameworkConfig {
            command: "uv run pytest".to_string(),
            ..Default::default()
        };
        let fw = PytestFramework::new(config)?;
        assert_eq!(fw.program, "uv");
        assert_eq!(fw.prefix_args, vec!["run", "pytest"]);
        Ok(())
    }

    #[test]
    fn test_command_prefix_default() -> Result<(), Box<dyn std::error::Error>> {
        let config = PytestFrameworkConfig {
            command: "python -m pytest".to_string(),
            ..Default::default()
        };
        let fw = PytestFramework::new(config)?;
        assert_eq!(fw.program, "python");
        assert_eq!(fw.prefix_args, vec!["-m", "pytest"]);
        Ok(())
    }

    #[test]
    fn test_new_rejects_invalid_command() {
        let config = PytestFrameworkConfig {
            command: "unclosed 'quote".to_string(),
            ..Default::default()
        };
        assert!(PytestFramework::new(config).is_err());
    }

    #[test]
    fn test_new_rejects_empty_command() {
        let config = PytestFrameworkConfig {
            command: "".to_string(),
            ..Default::default()
        };
        assert!(PytestFramework::new(config).is_err());
    }

    #[test]
    fn test_execution_command_with_run_args() -> Result<(), Box<dyn std::error::Error>> {
        let config = PytestFrameworkConfig {
            command: "uv run pytest".to_string(),
            run_args: Some("--no-cov --timeout=30".to_string()),
            ..Default::default()
        };
        let fw = PytestFramework::new(config)?;
        let record = TestRecord::new("tests/test_a.py::test_one", "test-group");
        let tests = vec![TestInstance::new(&record)];
        let cmd = fw.produce_test_execution_command(&tests, "/tmp/junit.xml", false);
        assert_eq!(cmd.program, "uv");
        assert!(cmd.args.contains(&"--no-cov".to_string()));
        assert!(cmd.args.contains(&"--timeout=30".to_string()));
        assert!(cmd.args.contains(&"tests/test_a.py::test_one".to_string()));
        Ok(())
    }

    fn fw() -> PytestFramework {
        PytestFramework::new(PytestFrameworkConfig {
            command: "python -m pytest".to_string(),
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn test_parse_collect_output_simple_ids() {
        let out = "\
tests/unit/foo.py::test_one
tests/unit/foo.py::test_two
";
        let ids: Vec<_> = fw()
            .parse_collect_output(out, "unit")
            .iter()
            .map(|t| t.id.clone())
            .collect();
        assert_eq!(
            ids,
            vec!["tests/unit/foo.py::test_one", "tests/unit/foo.py::test_two"]
        );
    }

    #[test]
    fn test_parse_collect_output_keeps_parametrize_ids_with_spaces() {
        // Regression: earlier parser rejected any line containing a space,
        // which silently dropped parametrize ids with spaces / special
        // characters from the schedule even though they collect fine.
        let out = "\
tests/integration/test_step_context.py::test_meta[Only custom name]
tests/integration/test_step_context.py::test_meta[Custom name, metadata and tags]
tests/integration/test_util.py::test_url[ftp://example.com - Must start with http]
tests/unit/models/test_filter_models.py::test_fails[2022/12/12 12-12-12]

5 tests collected in 0.12s
";
        let ids: Vec<_> = fw()
            .parse_collect_output(out, "mixed")
            .iter()
            .map(|t| t.id.clone())
            .collect();
        assert_eq!(
            ids,
            vec![
                "tests/integration/test_step_context.py::test_meta[Only custom name]",
                "tests/integration/test_step_context.py::test_meta[Custom name, metadata and tags]",
                "tests/integration/test_util.py::test_url[ftp://example.com - Must start with http]",
                "tests/unit/models/test_filter_models.py::test_fails[2022/12/12 12-12-12]",
            ]
        );
    }

    #[test]
    fn test_parse_collect_output_skips_non_test_lines() {
        // `-q` output can include error blocks, warnings, the trailing
        // summary, blank lines, and indented traceback fragments. None of
        // them should be interpreted as a test id.
        let out = "\
<ErrorRepr ('tests/bad.py', 1, 'ImportError: ...')>
  WARNING: something
tests/unit/foo.py::test_good[with spaces]

1 tests collected in 0.02s
";
        let ids: Vec<_> = fw()
            .parse_collect_output(out, "unit")
            .iter()
            .map(|t| t.id.clone())
            .collect();
        assert_eq!(ids, vec!["tests/unit/foo.py::test_good[with spaces]"]);
    }

    #[test]
    fn test_execution_command_fail_fast() -> Result<(), Box<dyn std::error::Error>> {
        let config = PytestFrameworkConfig {
            command: "python -m pytest".to_string(),
            ..Default::default()
        };
        let fw = PytestFramework::new(config)?;
        let record = TestRecord::new("tests/test_a.py::test_one", "grp");
        let tests = vec![TestInstance::new(&record)];

        let cmd = fw.produce_test_execution_command(&tests, "/tmp/junit.xml", true);
        assert!(cmd.args.contains(&"-x".to_string()));

        let cmd_no = fw.produce_test_execution_command(&tests, "/tmp/junit.xml", false);
        assert!(!cmd_no.args.contains(&"-x".to_string()));

        Ok(())
    }
}
