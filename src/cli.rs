//! 命令行入口。
//!
//! 不带参数时启动服务，带子命令时执行一次性操作。
//!
//! 目前只有密码重置：它是忘记密码后唯一的自救途径，所以只要求能读到数据库文件，
//! 不要求任何已有凭证——能碰到这台机器和库文件的人，本来就掌握着这个服务的一切。

use crate::config::Bootstrap;
use crate::{db, service};

/// 一次调用要做什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// 启动 HTTP 服务。
    Serve,
    /// 重置账号密码。
    ResetPassword,
    /// 打印帮助。
    Help,
}

impl Command {
    /// 解析命令行参数（不含程序名本身）。
    pub fn parse(args: &[String]) -> anyhow::Result<Self> {
        match args.first().map(String::as_str) {
            None => Ok(Self::Serve),
            Some("reset-password") => Ok(Self::ResetPassword),
            Some("-h" | "--help" | "help") => Ok(Self::Help),
            Some(other) => Err(anyhow::anyhow!(
                "未知的命令 `{other}`；用 `aequi help` 查看可用命令"
            )),
        }
    }
}

/// 打印帮助。
pub fn print_help() {
    println!(
        "Aequi —— LLM API 网关

用法:
  aequi                 启动服务
  aequi reset-password  重置账号密码（忘记密码时使用）
  aequi help            显示本帮助

环境变量:
  AEQUI_DATABASE_PATH        控制面数据库路径，默认 aequi.db
  AEQUI_DATA_DATABASE_PATH   数据面数据库路径，默认 aequi-data.db
  AEQUI_ADMIN_USERNAME       初始账号用户名，仅首次启动生效
  AEQUI_ADMIN_PASSWORD       初始账号密码，仅首次启动生效
  RUST_LOG                   日志级别，默认 info"
    );
}

/// 重置账号密码。
pub async fn reset_password(bootstrap: &Bootstrap) -> anyhow::Result<()> {
    // 先确认库已存在。连库会顺手把文件创建出来，那样下次正常启动就会被误判成
    // 「非首次」，从而跳过初始账号的初始化，把首次启动的环境变量也一并吞掉。
    if !bootstrap.database.exists() {
        anyhow::bail!("数据库尚未创建，请先启动一次服务完成初始化");
    }

    let store = db::Store::connect(&bootstrap.database, &bootstrap.data_database).await?;
    store.migrate().await?;

    let account = service::auth::sole_account(&store.control).await?;

    println!("将重置账号 {} 的密码。", account.username);
    let new_password = prompt_new_password()?;

    service::auth::reset_password(&store.control, account.id, &new_password).await?;

    println!("密码已更新，请用新密码登录。");
    Ok(())
}

/// 交互式读取新密码，要求输入两次且一致。
///
/// 输入不回显。不一致或为空时重来而不是直接退出——使用者此刻很可能正着急。
fn prompt_new_password() -> anyhow::Result<String> {
    loop {
        let first = rpassword::prompt_password("输入新密码: ")?;

        if first.is_empty() {
            println!("密码不能为空，请重试。");
            continue;
        }

        let second = rpassword::prompt_password("再次输入新密码: ")?;

        if first != second {
            println!("两次输入不一致，请重试。");
            continue;
        }

        return Ok(first);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn no_arguments_starts_the_server() {
        assert_eq!(Command::parse(&[]).unwrap(), Command::Serve);
    }

    #[test]
    fn parses_known_subcommands() {
        assert_eq!(
            Command::parse(&args(&["reset-password"])).unwrap(),
            Command::ResetPassword
        );
        assert_eq!(Command::parse(&args(&["help"])).unwrap(), Command::Help);
        assert_eq!(Command::parse(&args(&["--help"])).unwrap(), Command::Help);
        assert_eq!(Command::parse(&args(&["-h"])).unwrap(), Command::Help);
    }

    #[test]
    fn rejects_unknown_subcommands() {
        let error = Command::parse(&args(&["nope"])).expect_err("未知命令应当报错");
        assert!(error.to_string().contains("nope"));
    }
}
