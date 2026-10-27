//! Launch-only harness update detection. All I/O runs on a worker; installation
//! occurs only when the user chooses Update. Nothing is persisted.
use crate::scm::shell::{forget_login_shell_env, resolve_gui_binary, LOGIN_SHELL_ENV};
use crate::{
    catalog, cli,
    provider::{self, Kind},
};
use serde_json::Value;
use std::{
    cmp::Ordering,
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering as AtomicOrdering},
        Mutex,
    },
    time::Duration,
};

static CLAIMED: AtomicBool = AtomicBool::new(false);
pub fn claim_launch_check() -> bool {
    !CLAIMED.swap(true, AtomicOrdering::SeqCst)
}
#[derive(Clone, Debug)]
pub struct Update {
    pub harness: Kind,
    pub installed: String,
    pub latest: String,
}
fn package(kind: Kind) -> Option<&'static str> {
    match kind {
        Kind::Claude => Some("@anthropic-ai/claude-code"),
        Kind::Codex => Some("@openai/codex"),
        Kind::OpenCode => Some("opencode-ai"),
        Kind::Pi => Some("@earendil-works/pi-coding-agent"),
        _ => None,
    }
}
pub fn parse_version(text: &str) -> Option<String> {
    regex::Regex::new(r"\d+\.\d+\.\d+")
        .ok()?
        .find(text)
        .map(|m| m.as_str().to_owned())
}
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    let parts = |s: &str| {
        s.split('.')
            .map(|n| n.parse::<u64>().unwrap_or(0))
            .take(3)
            .collect::<Vec<_>>()
    };
    parts(a).cmp(&parts(b))
}
fn cwd() -> PathBuf {
    crate::accounts::home().unwrap_or_else(|| PathBuf::from("/"))
}
fn installed(kind: Kind) -> Result<String, String> {
    let path = provider::resolve(kind).ok_or("CLI could not be resolved")?;
    let output = cli::output(
        &path,
        &["--version".into()],
        &cwd(),
        Duration::from_secs(10),
    )?;
    parse_version(&output).ok_or("CLI returned no version".into())
}
pub fn latest_version(kind: Kind) -> Result<String, String> {
    let package = package(kind).ok_or("No update feed for harness")?;
    let text = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .build()
        .get(&format!("https://registry.npmjs.org/{package}/latest"))
        .set("User-Agent", "MonoCode")
        .set("Accept", "application/json")
        .call()
        .map_err(|e| e.to_string())?
        .into_string()
        .map_err(|e| e.to_string())?;
    let body: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    parse_version(body["version"].as_str().unwrap_or(""))
        .ok_or("npm registry returned no version".into())
}
/// Hidden and unresolved providers are excluded; failures are deliberately silent.
pub fn find_updates(hidden: &[String]) -> Vec<Update> {
    std::thread::scope(|scope| {
        let jobs = Kind::ALL
            .into_iter()
            .filter(|k| package(*k).is_some() && !hidden.iter().any(|id| id == k.id()))
            .map(|harness| {
                scope.spawn(move || {
                    let installed = installed(harness).ok()?;
                    let latest = latest_version(harness).ok()?;
                    (compare_versions(&latest, &installed).is_gt()).then_some(Update {
                        harness,
                        installed,
                        latest,
                    })
                })
            })
            .collect::<Vec<_>>();
        jobs.into_iter()
            .filter_map(|job| job.join().ok().flatten())
            .collect()
    })
}
/// Update the resolved CLI the way it was installed (see `Installer`), then
/// verify its actual version and refresh models before reporting success
/// (exit status alone is insufficient).
pub fn run_update(update: &Update) -> Result<String, String> {
    let args: &[&str] = match update.harness {
        Kind::Claude | Kind::Codex => &["update"],
        Kind::OpenCode => &["upgrade"],
        Kind::Pi => &["update", "--self"],
        _ => return Err("No updater for harness".into()),
    };
    let path = provider::resolve(update.harness).ok_or("CLI could not be resolved")?;
    let args = args.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let mise = resolve_gui_binary("mise");
    update_cli(&path, &args, mise.as_deref(), &LOGIN_SHELL_ENV)
        .map_err(|error| last_line(&error).unwrap_or("Update failed").to_owned())?;
    let version = installed(update.harness).unwrap_or_else(|_| update.installed.clone());
    if compare_versions(&version, &update.latest).is_lt() {
        return Err(format!("Still on {version} after updating."));
    }
    catalog::invalidate(update.harness, &crate::accounts::Profile::default());
    catalog::models(update.harness)?;
    Ok(version)
}

const UPDATE_TIMEOUT: Duration = Duration::from_secs(300);

/// Who installed the resolved CLI, which decides what updates it.
enum Installer {
    /// The CLI's own updater: native installs, and package managers the
    /// updater drives itself (Codex and OpenCode run `brew upgrade` or
    /// `npm install -g` on their own).
    Cli,
    /// No harness updater moves a mise install. Claude Code only prints
    /// `mise upgrade claude` and exits 0; the others don't know mise.
    Mise { mise: PathBuf, tool: String },
}

fn installer(binary: &Path, mise: Option<&Path>) -> Result<Installer, String> {
    if let Some(mise) = mise {
        if let Some(tool) = mise_tool(mise, binary)? {
            return Ok(Installer::Mise {
                mise: mise.to_path_buf(),
                tool,
            });
        }
    }
    Ok(Installer::Cli)
}

fn update_cli(
    binary: &Path,
    args: &[String],
    mise: Option<&Path>,
    shell_env: &Mutex<Option<HashMap<String, String>>>,
) -> Result<String, String> {
    match installer(binary, mise)? {
        Installer::Cli => cli::output(binary, args, &cwd(), UPDATE_TIMEOUT),
        Installer::Mise { mise, tool } => {
            let output = cli::output(
                &mise,
                &["upgrade".into(), "--yes".into(), tool],
                &cwd(),
                UPDATE_TIMEOUT,
            )
            .map_err(|error| mise_reason(&error).to_owned())?;
            // A tool locked in mise.lock is on PATH by its version directory,
            // so the cached PATH still names the old install, and so would
            // the version check.
            forget_login_shell_env(shell_env);
            Ok(output)
        }
    }
}

/// The mise tool that installed `binary`. mise records where each tool
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
    let version = cli::output(binary, &["--version".into()], &cwd(), MISE_QUERY_TIMEOUT)?;
    Ok(mise_tool_for(&installed, real, &version))
}

const MISE_QUERY_TIMEOUT: Duration = Duration::from_secs(15);

/// mise runs from the home directory so it reads the same global config as
/// the login shell MonoCode resolved the binary from.
fn run_mise(mise: &Path, args: &[&str]) -> Result<String, String> {
    let owned = args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
    cli::output(mise, &owned, &cwd(), MISE_QUERY_TIMEOUT)
        .map_err(|error| format!("mise {}: {}", args.join(" "), mise_reason(&error)))
}

/// mise ends every error with `Version:` and `Run with --verbose` lines, so
/// the reason is the first `mise ERROR` line, not the last line.
fn mise_reason(error: &str) -> &str {
    error
        .lines()
        .map(str::trim)
        .find(|line| {
            line.starts_with("mise ERROR")
                && !line.starts_with("mise ERROR Version:")
                && !line.starts_with("mise ERROR Run with --verbose")
        })
        .or_else(|| last_line(error))
        .unwrap_or("failed")
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

/// Updaters print their reason last.
fn last_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).rfind(|line| !line.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
            mise_tool_for(&installed, claude, "2.1.308 (Claude Code)"),
            Some("aqua:anthropics/claude-code".to_string())
        );

        // `npm i -g @openai/codex` under a mise Node: npm updates it, not mise.
        let codex = Path::new(
            "/home/me/.local/share/mise/installs/node/26.11.1/lib/node_modules/@openai/codex/bin/codex.js",
        );
        assert_eq!(mise_tool_for(&installed, codex, "codex-cli 0.163.0"), None);

        assert_eq!(
            mise_tool_for(&installed, Path::new("/opt/homebrew/bin/claude"), "2.1.308"),
            None
        );
    }

    #[test]
    fn matches_only_the_complete_reported_version() {
        assert!(reports_version("2.1.308 (Claude Code)", "2.1.308"));
        assert!(reports_version("opencode v2.1.4", "2.1.4"));
        assert!(!reports_version("2.1.30 (Claude Code)", "2.1.3"));
        assert!(!reports_version("codex-cli 0.163.0-alpha", "0.163.0"));
    }

    /// A fake mise with one Claude Code install and a shim for it. Like real
    /// mise, it runs the install when called through the shim.
    #[cfg(unix)]
    struct MiseFixture {
        root: tempfile::TempDir,
        mise: PathBuf,
        install: PathBuf,
        shim: PathBuf,
    }

    #[cfg(unix)]
    impl MiseFixture {
        fn new() -> Self {
            use std::os::unix::fs::PermissionsExt;

            let root = tempfile::tempdir().unwrap();
            let base = std::fs::canonicalize(root.path()).unwrap();
            let dir = base.join("installs/claude/2.1.308");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::create_dir_all(base.join("shims")).unwrap();
            let base_str = base.to_string_lossy();
            let mise = base.join("mise");
            let install = dir.join("claude");
            let shim = base.join("shims/claude");
            let script = format!(
                r#"#!/bin/sh
if [ "$(basename "$0")" = claude ]; then exec '{install}' "$@"; fi
case "$1" in
  which) [ -f '{base_str}/which-fails' ] && {{ printf '%s\n' 'mise ERROR claude is not a mise bin. Perhaps you need to install it first.' 'mise ERROR Version: 2026.10.13 macos-arm64 (2026-10-17)' 'mise ERROR Run with --verbose or MISE_VERBOSE=1 for more information' >&2; exit 1; }}
         echo '{install}' ;;
  ls) echo '{{"claude":[{{"version":"2.1.308","install_path":"{dir}"}}]}}' ;;
  *) echo "mise $*" >> '{base_str}/log' ;;
esac
"#,
                install = install.display(),
                dir = dir.display(),
            );
            std::fs::write(&mise, script).unwrap();
            std::fs::write(
                &install,
                format!(
                    "#!/bin/sh\nif [ \"$1\" = --version ]; then echo '2.1.308 (Claude Code)'; else echo \"claude $*\" >> '{base_str}/log'; fi\n"
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
        ) -> Result<String, String> {
            update_cli(binary, &["update".to_string()], Some(&self.mise), shell_env)
        }

        fn log(&self) -> String {
            std::fs::read_to_string(self.root.path().join("log")).unwrap_or_default()
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
    fn runs_the_cli_updater_when_no_package_manager_owns_the_install() {
        let mise = MiseFixture::new();
        let other = mise.root.path().join("claude");
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
        std::fs::write(mise.root.path().join("which-fails"), "").unwrap();
        let error = mise.update(&mise.shim, &cached_shell_env()).unwrap_err();
        assert_eq!(
            error,
            "mise which claude: mise ERROR claude is not a mise bin. Perhaps you need to install it first."
        );
        assert_eq!(mise.log(), "");
    }
}
