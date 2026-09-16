//! 原生统一入口：交互参数交给同目录 TUI，显式脚本子命令继续由 CLI 解析。
//! 不搜索 PATH，避免桌面内置版启动另一份安装或同名桌面程序。
use clap::Parser;
use std::{ffi::OsString, path::PathBuf};

#[derive(Parser)]
#[command(disable_help_flag = true, disable_version_flag = true)]
struct InteractiveArgs {
    #[arg(long)]
    cwd: Option<PathBuf>,
    #[arg(long)]
    daemon: bool,
    #[arg(long)]
    connect: Option<String>,
    #[arg(long)]
    resume: Option<String>,
    #[arg(long)]
    task_id: Option<String>,
    #[arg(long)]
    debug: bool,
    #[arg(long)]
    yolo: bool,
    #[arg(long)]
    execution_mode: Option<String>,
    #[arg(long)]
    tool_profile: Option<String>,
}

fn interactive_arguments(arguments: &[OsString]) -> Option<Vec<OsString>> {
    let mut forwarded = arguments.to_vec();
    // 显式 resume 子命令与 TUI 的 --resume 使用同一标识合同。
    if forwarded.get(1).is_some_and(|arg| arg == "resume") {
        forwarded[1] = "--resume".into();
    }
    InteractiveArgs::try_parse_from(&forwarded).ok()?;
    Some(forwarded.into_iter().skip(1).collect())
}

pub(super) fn launch_if_interactive() -> miette::Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    let Some(forwarded) = interactive_arguments(&args) else {
        return Ok(());
    };
    let executable = std::env::current_exe()
        .map_err(|error| miette::miette!("cannot locate Agent executable: {error}"))?;
    let sibling = executable.with_file_name(if cfg!(windows) {
        "golutra-agent-tui.exe"
    } else {
        "golutra-agent-tui"
    });
    let mut command = std::process::Command::new(&sibling);
    command.args(forwarded);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = command.exec();
        Err(miette::miette!(
            "cannot start {}: {error}; install the complete Agent native package",
            sibling.display()
        ))
    }
    #[cfg(not(unix))]
    {
        let status = command.status().map_err(|error| {
            miette::miette!(
                "cannot start {}: {error}; install the complete Agent native package",
                sibling.display()
            )
        })?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interactive_flags_preserve_argument_boundaries_and_cli_subcommands_stay_cli() {
        for args in [
            vec!["golutra-agent"],
            vec!["golutra-agent", "--cwd", "/a b/项目", "--yolo"],
            vec!["golutra-agent", "--resume", "thread-1"],
        ] {
            let input: Vec<OsString> = args.iter().map(OsString::from).collect();
            assert_eq!(interactive_arguments(&input).unwrap(), input[1..]);
        }
        let resume: Vec<OsString> = ["golutra-agent", "resume", "thread-1"]
            .iter()
            .map(OsString::from)
            .collect();
        assert_eq!(
            interactive_arguments(&resume).unwrap(),
            vec![OsString::from("--resume"), OsString::from("thread-1")]
        );
        for args in [
            vec!["golutra-agent", "--version"],
            vec!["golutra-agent", "--help"],
            vec!["golutra-agent", "exec", "--yolo", "task"],
            vec!["golutra-agent", "provider", "list"],
            vec!["golutra-agent", "--unknown"],
            vec!["golutra-agent", "resume"],
        ] {
            let input: Vec<OsString> = args.iter().map(OsString::from).collect();
            assert!(interactive_arguments(&input).is_none(), "{args:?}");
        }
    }
}
