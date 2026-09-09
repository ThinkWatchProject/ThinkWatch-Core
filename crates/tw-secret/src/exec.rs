//! `exec` 凭据：跑一个命令，拿它的 stdout 当密钥。
//!
//! 这是**不把密钥明文写进配置文件的唯一完整方案**（DESIGN.md §3.2）。
//! `${ENV}` 只覆盖「密钥已经在环境里」的情况；真正想把密钥留在
//! 1Password / pass / 钥匙串里的人，需要的是这个。
//!
//! 它同时也是这个程序里**最危险的一个字段** —— 配置文件里的一行字符串
//! 会变成一次进程执行。所以：
//!
//! - 命令来自配置文件，而配置文件是用户自己的（§3.1），不是从网上拉的；
//! - 但配置文件可能被同步、被分享、被 AI 改（§3.1 明确说了这些是目标
//!   场景），所以它进 §5.3 的扫描规则集，和 hook 里的 shell 命令同一档；
//! - **不过 shell**：`sh -c` 会让一个引号写错的路径变成命令注入。

use std::process::Command;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("exec 命令是空的")]
    Empty,
    /// **`NotFound` 单独说一句。**开机自启时 launchd 给的 `$PATH` 只有
    /// `/usr/bin:/bin:/usr/sbin:/sbin` —— `op`、`gcloud`、`gh` 全都找不到，
    /// 而手动启动时一切正常。这种时好时坏的 bug 最难查，所以错误信息
    /// 必须直接把那个原因说出来（§2.4）。
    #[error(
        "找不到 `{cmd}`。如果它装在 Homebrew / nvm / ~/.local/bin 里，注意开机自启的进程只有最小 PATH —— 写命令的绝对路径最稳（`which {bin}` 能查到）。"
    )]
    NotFound { cmd: String, bin: String },
    #[error("跑 `{cmd}` 失败：{source}")]
    Spawn { cmd: String, source: std::io::Error },
    #[error("`{cmd}` 退出码 {code}，stderr：{stderr}")]
    Failed {
        cmd: String,
        code: i32,
        stderr: String,
    },
    #[error("`{cmd}` 超过 {timeout:?} 还没结束")]
    Timeout { cmd: String, timeout: Duration },
    #[error("`{cmd}` 什么都没输出。密钥命令必须往 stdout 写点东西。")]
    NoOutput { cmd: String },
    #[error("`{cmd}` 的输出不是合法 UTF-8")]
    NotUtf8 { cmd: String },
}

/// 默认超时。密钥命令通常是读一个文件或调一次本地 agent，秒级足够；
/// 而**卡住比失败更糟** —— 一个卡住的凭据命令会让每个请求都挂在这里，
/// 表现是网关整个没反应。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// 跑一条命令，把 stdout 当密钥。
///
/// `argv` 的第一个元素是程序，其余是参数。**不拼 shell 命令行**：
/// `sh -c "op read $PATH"` 里一个带空格的路径就能变成命令注入，而
/// 用户写这行的时候完全想不到这一点。
pub fn run_exec(argv: &[String], timeout: Duration) -> Result<String, ExecError> {
    let Some((program, args)) = argv.split_first() else {
        return Err(ExecError::Empty);
    };
    if program.trim().is_empty() {
        return Err(ExecError::Empty);
    }
    let cmd_desc = argv.join(" ");

    let mut child = Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                ExecError::NotFound {
                    cmd: cmd_desc.clone(),
                    bin: program.to_string(),
                }
            } else {
                ExecError::Spawn {
                    cmd: cmd_desc.clone(),
                    source,
                }
            }
        })?;

    // 轮询而不是 wait_timeout：不想为这一处引一个 crate。密钥命令的
    // 频率是「每次读配置一次」，50ms 的粒度绰绰有余。
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ExecError::Timeout {
                        cmd: cmd_desc,
                        timeout,
                    });
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(source) => {
                return Err(ExecError::Spawn {
                    cmd: cmd_desc,
                    source,
                });
            }
        }
    }

    let out = child
        .wait_with_output()
        .map_err(|source| ExecError::Spawn {
            cmd: cmd_desc.clone(),
            source,
        })?;

    if !out.status.success() {
        // stderr 带出来，但**截断**：一个失败的命令可能吐一整页，而这条
        // 错误会进日志和 UI。
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stderr = stderr.trim();
        let end = stderr
            .char_indices()
            .map(|(i, c)| i + c.len_utf8())
            .take_while(|&i| i <= 300)
            .last()
            .unwrap_or(0);
        return Err(ExecError::Failed {
            cmd: cmd_desc,
            code: out.status.code().unwrap_or(-1),
            stderr: stderr[..end].to_string(),
        });
    }

    let s = String::from_utf8(out.stdout).map_err(|_| ExecError::NotUtf8 {
        cmd: cmd_desc.clone(),
    })?;
    // 尾随换行是必然的（`echo`、`cat`、`op read` 都会给），必须吃掉 ——
    // 一个带 \n 的密钥发出去就是 401，而那条排查路径很长。
    let s = s.trim().to_string();
    if s.is_empty() {
        return Err(ExecError::NoOutput { cmd: cmd_desc });
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn takes_stdout_as_the_secret() {
        let s = run_exec(&argv(&["echo", "sk-from-command"]), DEFAULT_TIMEOUT).unwrap();
        assert_eq!(s, "sk-from-command");
    }

    #[test]
    fn trailing_newline_is_stripped() {
        // echo/cat/op read 都会给一个尾随换行。带 \n 的密钥发出去就是
        // 401，而那条排查路径很长。
        let s = run_exec(&argv(&["printf", "sk-x\\n\\n"]), DEFAULT_TIMEOUT).unwrap();
        assert_eq!(s, "sk-x");
    }

    #[test]
    fn no_shell_means_no_injection() {
        // 参数里的 shell 元字符必须原样传给程序，而不是被解释。
        // 用户写 `op read "op://vault/item; rm -rf ~"` 的时候，那个分号
        // 应该是路径的一部分，不是一条新命令。
        let s = run_exec(&argv(&["echo", "a; echo b"]), DEFAULT_TIMEOUT).unwrap();
        assert_eq!(s, "a; echo b");
    }

    #[test]
    fn a_failing_command_reports_code_and_stderr() {
        let e = run_exec(
            &argv(&["sh", "-c", "echo boom >&2; exit 3"]),
            DEFAULT_TIMEOUT,
        )
        .unwrap_err();
        match e {
            ExecError::Failed {
                code, ref stderr, ..
            } => {
                assert_eq!(code, 3);
                assert!(stderr.contains("boom"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_command_that_prints_nothing_is_an_error() {
        // 静默返回空串会让请求带着空 key 发出去，然后 401 —— 和 ${ENV}
        // 缺失时是同一条理由。
        let e = run_exec(&argv(&["true"]), DEFAULT_TIMEOUT).unwrap_err();
        assert!(matches!(e, ExecError::NoOutput { .. }));
    }

    #[test]
    fn a_hanging_command_times_out_instead_of_wedging_the_gateway() {
        // 卡住比失败更糟：每个请求都会挂在这里，表现是网关整个没反应。
        let e = run_exec(&argv(&["sleep", "30"]), Duration::from_millis(300)).unwrap_err();
        assert!(matches!(e, ExecError::Timeout { .. }));
    }

    #[test]
    fn a_missing_program_says_which_one() {
        let e = run_exec(
            &argv(&["definitely-not-a-real-program-xyz"]),
            DEFAULT_TIMEOUT,
        )
        .unwrap_err();
        assert!(format!("{e}").contains("definitely-not-a-real-program-xyz"));
    }

    #[test]
    fn an_empty_argv_is_rejected() {
        assert!(matches!(
            run_exec(&[], DEFAULT_TIMEOUT),
            Err(ExecError::Empty)
        ));
        assert!(matches!(
            run_exec(&argv(&["  "]), DEFAULT_TIMEOUT),
            Err(ExecError::Empty)
        ));
    }
}

#[cfg(test)]
mod notfound_tests {
    use super::*;

    #[test]
    fn a_missing_binary_points_at_the_path_problem_not_at_a_generic_io_error() {
        // 开机自启时 launchd 给的 PATH 只有四个目录，`op`/`gcloud`/`gh`
        // 全都找不到 —— 而手动启动时一切正常。这种时好时坏的 bug 最难查，
        // 所以那句「注意 PATH」必须在错误里，不能只在文档里（§2.4）。
        let e = run_exec(
            &["definitely-not-a-real-binary-xyz".into(), "read".into()],
            Duration::from_secs(2),
        )
        .unwrap_err();
        let m = e.to_string();
        assert!(m.contains("PATH"), "{m}");
        assert!(m.contains("definitely-not-a-real-binary-xyz"), "{m}");
        assert!(m.contains("绝对路径"), "{m}");
    }

    #[test]
    fn a_binary_that_exists_but_fails_is_a_different_error() {
        // 找不到和跑失败是两件事，指错方向的代价是查半天 PATH。
        let e = run_exec(&["false".into()], Duration::from_secs(5)).unwrap_err();
        assert!(!e.to_string().contains("PATH"), "{e}");
    }
}
