use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;

use crate::harness::{
    exec_output, forget_login_shell_env, is_resolved_harness_binary, which_via_login_shell,
    LOGIN_SHELL_ENV,
};

const REGISTRY_URL: &str = "https://registry.npmjs.org";
const USER_AGENT: &str = "MonoCode";
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Only harnesses whose releases are published to npm. The rest ship through
/// their own installers with no public version feed to compare against.
fn npm_package(provider: &str) -> Option<&'static str> {
    match provider {
        "claude" => Some("@anthropic-ai/claude-code"),
        "codex" => Some("@openai/codex"),
        "opencode" => Some("opencode-ai"),
        "pi" => Some("@earendil-works/pi-coding-agent"),
        _ => None,
    }
}

/// Each CLI's own updater, which knows how it was installed (native, npm,
/// Homebrew) better than MonoCode could guess from the binary path. mise
/// installs are the exception; see `mise_tool`.
fn update_args(provider: &str) -> Option<&'static [&'static str]> {
    match provider {
        "claude" => Some(&["update"]),
        "codex" => Some(&["update"]),
        "opencode" => Some(&["upgrade"]),
        "pi" => Some(&["update", "--self"]),
        _ => None,
    }
}

/// A download plus, for npm installs, a full dependency install.
const UPDATE_TIMEOUT: Duration = Duration::from_secs(300);

static LAUNCH_CHECK_CLAIMED: AtomicBool = AtomicBool::new(false);

/// True for the first caller per app process, so a window opened later in the
/// same run does not repeat the launch check.
#[tauri::command]
pub fn harness_update_check_claim() -> bool {
    !LAUNCH_CHECK_CLAIMED.swap(true, Ordering::SeqCst)
}

#[tauri::command]
pub async fn harness_latest_version(provider: String) -> Result<String, String> {
    let package =
        npm_package(&provider).ok_or_else(|| format!("No update feed for harness: {provider}"))?;
    tauri::async_runtime::spawn_blocking(move || {
        let agent = ureq::AgentBuilder::new().timeout(HTTP_TIMEOUT).build();
        let text = agent
            .get(&format!("{REGISTRY_URL}/{package}/latest"))
            .set("User-Agent", USER_AGENT)
            .set("Accept", "application/json")
            .call()
            .map_err(|error| format!("npm registry request failed: {error}"))?
            .into_string()
            .map_err(|error| format!("npm registry response unreadable: {error}"))?;
        let body: Value = serde_json::from_str(&text)
            .map_err(|error| format!("npm registry returned invalid JSON: {error}"))?;
        latest_version(&body).ok_or_else(|| "npm registry returned no version".to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Runs the harness's self-update against the binary MonoCode resolved for
/// it, or `mise upgrade` when mise installed that binary. stdin is closed, so
/// an updater that stops to ask fails instead of hanging.
#[tauri::command]
pub async fn harness_update(
    command: String,
    binary_provider: String,
    binary_path: Option<String>,
) -> Result<(), String> {
    let args: Vec<String> = update_args(&binary_provider)
        .ok_or_else(|| format!("No updater for harness: {binary_provider}"))?
        .iter()
        .map(|arg| arg.to_string())
        .collect();
    tauri::async_runtime::spawn_blocking(move || {
        if !is_resolved_harness_binary(&command, Some(&binary_provider), binary_path.as_deref()) {
            return Err("harness_update: not a resolved harness CLI".to_string());
        }
        let mise = which_via_login_shell("mise");
        let output = run_update(&command, &args, mise.as_deref(), &LOGIN_SHELL_ENV)?;
        if output.status.success() {
            return Ok(());
        }
        Err(update_failure(&output.stdout, &output.stderr))
    })
    .await
    .map_err(|e| e.to_string())?
}

fn run_update(
    command: &str,
    args: &[String],
    mise: Option<&Path>,
    shell_env: &Mutex<Option<HashMap<String, String>>>,
) -> Result<Output, String> {
    let upgrade = match mise {
        Some(mise) => mise_tool(mise, Path::new(command))?.map(|tool| (mise, tool)),
        None => None,
    };
    let Some((mise, tool)) = upgrade else {
        return exec_output(command, args, None, UPDATE_TIMEOUT);
    };
    let output = exec_output(
        &mise.to_string_lossy(),
        &["upgrade".to_string(), "--yes".to_string(), tool],
        Some("~"),
        UPDATE_TIMEOUT,
    )?;
    // A tool locked in mise.lock is on PATH by its version directory, so the
    // cached PATH still names the old install.
    if output.status.success() {
        forget_login_shell_env(shell_env);
    }
    Ok(output)
}

/// The mise tool that installed `binary`.
///
/// A CLI's own updater cannot move a mise install forward. Claude Code only
/// prints `mise upgrade claude` and exits 0, and npm-style updaters install
/// into a directory mise never puts on PATH. mise records where each tool
/// version lives, so this works for any harness and any mise backend.
///
/// A shim is always mise's, so a failed lookup is an error rather than a
/// reason to run the CLI's own updater against mise's install. For any other
/// binary, a failed lookup only means MonoCode can't tell, and the CLI's own
/// updater runs.
fn mise_tool(mise: &Path, binary: &Path) -> Result<Option<String>, String> {
    let Ok(real) = std::fs::canonicalize(binary) else {
        return Ok(None);
    };
    // A shim is a link to mise itself, which picks the install at run time.
    if std::fs::canonicalize(mise).is_ok_and(|mise| mise == real) {
        let name = binary
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let target = run_mise(mise, &["which", &name])?;
        let real =
            std::fs::canonicalize(target.trim()).map_err(|e| format!("mise which {name}: {e}"))?;
        return installed_mise_tool(mise, binary, &real);
    }
    Ok(installed_mise_tool(mise, binary, &real).ok().flatten())
}

fn installed_mise_tool(mise: &Path, binary: &Path, real: &Path) -> Result<Option<String>, String> {
    let installed = run_mise(mise, &["ls", "--installed", "--json"])?;
    let installed: Value = serde_json::from_str(&installed).map_err(|e| format!("mise ls: {e}"))?;
    let version = exec_output(
        &binary.to_string_lossy(),
        &["--version".to_string()],
        None,
        MISE_QUERY_TIMEOUT,
    )?;
    Ok(mise_tool_for(
        &installed,
        real,
        &String::from_utf8_lossy(&version.stdout),
    ))
}

const MISE_QUERY_TIMEOUT: Duration = Duration::from_secs(15);

/// mise runs from the home directory so it reads the same global config as
/// the login shell MonoCode resolved the binary from.
fn run_mise(mise: &Path, args: &[&str]) -> Result<String, String> {
    let output = exec_output(
        &mise.to_string_lossy(),
        &args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>(),
        Some("~"),
        MISE_QUERY_TIMEOUT,
    )?;
    if !output.status.success() {
        let reason = update_failure(&output.stdout, &output.stderr);
        return Err(format!("mise {}: {reason}", args.join(" ")));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Finds the install in `mise ls --installed --json` (tool name to installed
/// versions, each with an `install_path`) that holds `binary`.
///
/// A runtime's install also holds whatever its package manager added: under
/// a mise Node, `npm i -g @openai/codex` lands in Node's install. That CLI
/// updates itself through npm, and `mise upgrade node` would be wrong. So the
/// install counts only when the CLI reports the version mise installed.
fn mise_tool_for(installed: &Value, binary: &Path, reported_version: &str) -> Option<String> {
    installed.as_object()?.iter().find_map(|(tool, versions)| {
        let owns = versions.as_array()?.iter().any(|install| {
            let holds_binary = install
                .get("install_path")
                .and_then(Value::as_str)
                .is_some_and(|dir| {
                    // `binary` is canonical; macOS reaches temp and data dirs
                    // through symlinks such as /var -> /private/var.
                    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| PathBuf::from(dir));
                    binary.starts_with(dir)
                });
            holds_binary
                && install
                    .get("version")
                    .and_then(Value::as_str)
                    .is_some_and(|version| reports_version(reported_version, version))
        });
        owns.then(|| tool.clone())
    })
}

/// Whether `--version` output names exactly `version`, as in
/// `2.1.308 (Claude Code)` or `opencode v2.1.4`. A substring match would let
/// mise's `2.1.3` claim a CLI that reports `2.1.30`.
fn reports_version(output: &str, version: &str) -> bool {
    output
        .split_whitespace()
        .any(|word| word.strip_prefix('v').unwrap_or(word) == version)
}

/// Updaters print their reason to either stream; the last line is the one
/// that says what went wrong.
fn update_failure(stdout: &[u8], stderr: &[u8]) -> String {
    [stderr, stdout]
        .iter()
        .filter_map(|bytes| {
            String::from_utf8_lossy(bytes)
                .lines()
                .map(str::trim)
                .rfind(|line| !line.is_empty())
                .map(str::to_string)
        })
        .next()
        .unwrap_or_else(|| "Update failed".to_string())
}

fn latest_version(body: &Value) -> Option<String> {
    let version = body.get("version")?.as_str()?.trim();
    (!version.is_empty()).then(|| version.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_only_npm_published_harnesses() {
        assert_eq!(npm_package("claude"), Some("@anthropic-ai/claude-code"));
        assert_eq!(npm_package("pi"), Some("@earendil-works/pi-coding-agent"));
        assert_eq!(npm_package("cursor"), None);
        assert_eq!(npm_package("../../evil"), None);
    }

    #[test]
    fn updates_only_through_each_cli_own_updater() {
        assert_eq!(update_args("pi"), Some(&["update", "--self"][..]));
        assert_eq!(update_args("opencode"), Some(&["upgrade"][..]));
        assert_eq!(update_args("cursor"), None);
    }

    #[test]
    fn finds_the_mise_tool_that_installed_a_cli() {
        let installed = json!({
            "aqua:anthropics/claude-code": [{
                "version": "2.1.308",
                "install_path": "/home/me/.local/share/mise/installs/aqua-anthropics-claude-code/2.1.308",
            }],
            "node": [{
                "version": "26.11.1",
                "install_path": "/home/me/.local/share/mise/installs/node/26.11.1",
            }],
        });
        let claude = Path::new(
            "/home/me/.local/share/mise/installs/aqua-anthropics-claude-code/2.1.308/claude",
        );
        assert_eq!(
            mise_tool_for(&installed, claude, "2.1.308 (Claude Code)\n"),
            Some("aqua:anthropics/claude-code".to_string())
        );

        // `npm i -g @openai/codex` under a mise Node: npm updates it, not mise.
        let codex = Path::new(
            "/home/me/.local/share/mise/installs/node/26.11.1/lib/node_modules/@openai/codex/bin/codex.js",
        );
        assert_eq!(
            mise_tool_for(&installed, codex, "codex-cli 0.163.0\n"),
            None
        );

        assert_eq!(
            mise_tool_for(&installed, Path::new("/opt/homebrew/bin/claude"), "2.1.308"),
            None
        );
    }

    #[test]
    fn matches_only_the_complete_reported_version() {
        assert!(reports_version("2.1.308 (Claude Code)\n", "2.1.308"));
        assert!(reports_version("opencode v2.1.4\n", "2.1.4"));
        assert!(!reports_version("2.1.30 (Claude Code)\n", "2.1.3"));
        assert!(!reports_version("codex-cli 0.163.0-alpha\n", "0.163.0"));
    }

    /// A fake mise with one Claude Code install and a shim for it. Like real
    /// mise, it runs the install when called through the shim.
    #[cfg(unix)]
    struct MiseFixture {
        root: PathBuf,
        mise: PathBuf,
        install: PathBuf,
        shim: PathBuf,
    }

    #[cfg(unix)]
    impl MiseFixture {
        fn new() -> Self {
            use std::os::unix::fs::PermissionsExt;

            let root = std::fs::canonicalize(std::env::temp_dir())
                .unwrap()
                .join(format!("monocode-mise-{}", uuid::Uuid::new_v4()));
            let dir = root.join("installs/claude/2.1.308");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::create_dir_all(root.join("shims")).unwrap();
            let root_str = root.to_string_lossy();
            let mise = root.join("mise");
            let install = dir.join("claude");
            let shim = root.join("shims/claude");
            let script = format!(
                r#"#!/bin/sh
if [ "$(basename "$0")" = claude ]; then exec '{install}' "$@"; fi
case "$1" in
  which) [ -f '{root_str}/which-fails' ] && {{ echo 'mise ERROR claude is not a mise bin' >&2; exit 1; }}
         echo '{install}' ;;
  ls) echo '{{"claude":[{{"version":"2.1.308","install_path":"{dir}"}}]}}' ;;
  *) echo "mise $*" >> '{root_str}/log' ;;
esac
"#,
                install = install.display(),
                dir = dir.display(),
            );
            std::fs::write(&mise, script).unwrap();
            std::fs::write(
                &install,
                format!(
                    "#!/bin/sh\nif [ \"$1\" = --version ]; then echo '2.1.308 (Claude Code)'; else echo \"claude $*\" >> '{root_str}/log'; fi\n"
                ),
            )
            .unwrap();
            for path in [&mise, &install] {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            std::os::unix::fs::symlink(&mise, &shim).unwrap();
            MiseFixture {
                root,
                mise,
                install,
                shim,
            }
        }

        fn update(
            &self,
            binary: &Path,
            shell_env: &Mutex<Option<HashMap<String, String>>>,
        ) -> Result<Output, String> {
            run_update(
                &binary.to_string_lossy(),
                &["update".to_string()],
                Some(&self.mise),
                shell_env,
            )
        }

        fn log(&self) -> String {
            std::fs::read_to_string(self.root.join("log")).unwrap_or_default()
        }
    }

    #[cfg(unix)]
    impl Drop for MiseFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[cfg(unix)]
    fn cached_shell_env() -> Mutex<Option<HashMap<String, String>>> {
        Mutex::new(Some(HashMap::from([(
            "PATH".to_string(),
            "/old/mise/installs/claude/2.1.300".to_string(),
        )])))
    }

    #[cfg(unix)]
    #[test]
    fn upgrades_a_mise_install_through_mise_and_rereads_path() {
        let mise = MiseFixture::new();
        for binary in [&mise.install, &mise.shim] {
            let shell_env = cached_shell_env();
            mise.update(binary, &shell_env).unwrap();
            assert!(shell_env.lock().unwrap().is_none(), "{}", binary.display());
        }
        assert_eq!(
            mise.log(),
            "mise upgrade --yes claude\nmise upgrade --yes claude\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn runs_the_cli_updater_for_an_install_mise_does_not_own() {
        let mise = MiseFixture::new();
        let other = mise.root.join("claude");
        std::fs::copy(&mise.install, &other).unwrap();
        let shell_env = cached_shell_env();
        mise.update(&other, &shell_env).unwrap();
        assert_eq!(mise.log(), "claude update\n");
        assert!(shell_env.lock().unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn reports_a_failed_mise_lookup_for_a_shim() {
        let mise = MiseFixture::new();
        std::fs::write(mise.root.join("which-fails"), "").unwrap();
        let error = mise.update(&mise.shim, &cached_shell_env()).unwrap_err();
        assert_eq!(
            error,
            "mise which claude: mise ERROR claude is not a mise bin"
        );
        assert_eq!(mise.log(), "");
    }

    #[test]
    fn reports_the_last_line_an_updater_printed() {
        assert_eq!(
            update_failure(
                b"checking\n",
                b"npm ERR! code EACCES\nnpm ERR! permission denied\n\n"
            ),
            "npm ERR! permission denied"
        );
        assert_eq!(update_failure(b"no write access\n", b""), "no write access");
        assert_eq!(update_failure(b"", b""), "Update failed");
    }

    #[test]
    fn reads_version_from_registry_payload() {
        assert_eq!(
            latest_version(&json!({ "name": "opencode-ai", "version": "1.18.33" })),
            Some("1.18.33".to_string())
        );
        assert_eq!(latest_version(&json!({ "version": " " })), None);
        assert_eq!(latest_version(&json!({})), None);
    }
}
