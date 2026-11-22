//! Optional shell startup snippets that report the working directory with OSC 7.

use portable_pty::CommandBuilder;
use std::{ffi::OsStr, fs, path::Path, path::PathBuf, sync::OnceLock};

const BASH_RC: &str = r##"# voidtty shell integration
if [ -f "$HOME/.bashrc" ]; then
    . "$HOME/.bashrc"
fi

__voidtty_osc7() {
    printf '\033]7;file://%s%s\033\\' "${HOSTNAME:-localhost}" "$PWD"
}

if declare -p PROMPT_COMMAND 2>/dev/null | grep -q '^declare -a'; then
    PROMPT_COMMAND=(__voidtty_osc7 "${PROMPT_COMMAND[@]}")
else
    case "${PROMPT_COMMAND-}" in
        *__voidtty_osc7*) ;;
        *) PROMPT_COMMAND="__voidtty_osc7${PROMPT_COMMAND:+;$PROMPT_COMMAND}" ;;
    esac
    export PROMPT_COMMAND
fi
export -f __voidtty_osc7 2>/dev/null
"##;

const POSIX_ENV: &str = r##"# voidtty shell integration
if [ -n "${VOIDTTY_ORIGINAL_ENV-}" ] && [ "$VOIDTTY_ORIGINAL_ENV" != "$ENV" ] && [ -f "$VOIDTTY_ORIGINAL_ENV" ]; then
    . "$VOIDTTY_ORIGINAL_ENV"
fi

__voidtty_host=$(hostname 2>/dev/null || echo localhost)
__voidtty_osc7() {
    printf '\033]7;file://%s%s\033\\' "$__voidtty_host" "$PWD"
}

if [ -n "${PS1+x}" ]; then
    PS1='$(__voidtty_osc7)'"$PS1"
else
    case "$(id -u 2>/dev/null)" in
        0) PS1='$(__voidtty_osc7)# ' ;;
        *) PS1='$(__voidtty_osc7)$ ' ;;
    esac
fi
"##;

struct Paths {
    bash_rc: PathBuf,
    posix_env: PathBuf,
}

/// Configures a command so supported shells report their cwd through OSC 7.
pub fn configure(cmd: &mut CommandBuilder, shell: &str) {
    let name = Path::new(shell)
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or(shell);
    let posix = matches!(
        name,
        "sh" | "dash" | "ksh" | "ksh93" | "mksh" | "lksh" | "ash"
    );
    if name != "bash" && !posix {
        return;
    }
    let Some(paths) = paths() else {
        return;
    };
    if name == "bash" {
        cmd.arg("--rcfile");
        cmd.arg(&paths.bash_rc);
        return;
    }
    if let Some(original) = std::env::var_os("ENV") {
        cmd.env("VOIDTTY_ORIGINAL_ENV", original);
    }
    cmd.env("ENV", &paths.posix_env);
}

fn paths() -> Option<&'static Paths> {
    static PATHS: OnceLock<Option<Paths>> = OnceLock::new();
    PATHS.get_or_init(|| write_scripts().ok()).as_ref()
}

fn write_scripts() -> std::io::Result<Paths> {
    let base = if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        PathBuf::from(dir)
    } else if let Some(dir) = std::env::var_os("XDG_CACHE_HOME") {
        PathBuf::from(dir)
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".cache")
    } else {
        std::env::temp_dir()
    };
    let dir = base.join("voidtty").join("shell-integration");
    fs::create_dir_all(&dir)?;

    let bash_rc = dir.join("bashrc");
    let posix_env = dir.join("env.sh");
    write_atomic(&bash_rc, BASH_RC)?;
    write_atomic(&posix_env, POSIX_ENV)?;
    Ok(Paths { bash_rc, posix_env })
}

fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&tmp, contents)?;
    fs::rename(tmp, path)
}
