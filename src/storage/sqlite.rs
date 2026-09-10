//! SQLite 端口（§5.3）：**单写者 actor + 批量事务 + 高低优先双通道 + 1 写 N 读**。
//!
//! - 写连接独占在一个 actor 任务里，其它模块经 channel 提交写命令
//!   （§八 第 3 条「单一写者」—— 消除旧版 `check_monthly_reset` 类丢失更新）
//! - 批量提交：攒够 `batch_max_rows`（256）或距上次提交 > `batch_max_delay_ms`（100）
//!   → 1.7k RPS 逐条写降为 ≤ 20 事务/秒
//! - 高优先通道（额度/积分扣减）立即提交，不攒批 —— 额度判定实时性
//! - 读路径：WAL 下多读不阻塞写
//! - 调优清单（§5.2）在连接选项中落实
//!
//! 写命令用 `WriteQuery` 封装（SQL + 预绑定参数），repo 层构造后投递。

use crate::storage::schema::WritePriority;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Connection, Row};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sql(String),
    #[error("writer actor closed")]
    WriterClosed,
    #[error("row decode: {0}")]
    Decode(String),
}

impl From<sqlx::Error> for StoreError {
    fn from(e: sqlx::Error) -> Self {
        Self::Sql(e.to_string())
    }
}

pub type StoreResult<T> = Result<T, StoreError>;

/// 一条写命令：SQL + 已绑定参数 + 优先级 + 可选完成通知。
pub struct WriteCmd {
    pub sql: &'static str,
    pub args: Vec<SqliteArg>,
    pub priority: WritePriority,
    reply: Option<oneshot::Sender<StoreResult<u64>>>,
}

/// 可跨任务的 SQLite 参数值（`sqlx::SqliteValue` 非 Clone → 自建枚举）。
#[derive(Clone, Debug)]
pub enum SqliteArg {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl SqliteArg {
    /// 绑定到 query：所有变体都以 owned 值绑定（String clone），规避生命周期问题。
    fn bind_on<'q>(
        &self,
        q: sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>,
    ) -> sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>> {
        match self {
            Self::Null => q.bind(None::<i64>),
            Self::Int(v) => q.bind(*v),
            Self::Real(v) => q.bind(*v),
            Self::Text(v) => q.bind(v.clone()),
            Self::Blob(v) => q.bind(v.clone()),
        }
    }
}

impl WriteCmd {
    pub fn new(sql: &'static str, priority: WritePriority) -> Self {
        Self { sql, args: Vec::new(), priority, reply: None }
    }

    pub fn bind(mut self, a: impl Into<SqliteArg>) -> Self {
        self.args.push(a.into());
        self
    }

    /// 等待确认（高优先路径使用）。
    pub fn with_ack(mut self) -> (Self, oneshot::Receiver<StoreResult<u64>>) {
        let (tx, rx) = oneshot::channel();
        self.reply = Some(tx);
        (self, rx)
    }
}

impl From<i64> for SqliteArg {
    fn from(v: i64) -> Self {
        Self::Int(v)
    }
}
impl From<u64> for SqliteArg {
    fn from(v: u64) -> Self {
        Self::Int(v as i64)
    }
}
impl From<i32> for SqliteArg {
    fn from(v: i32) -> Self {
        Self::Int(i64::from(v))
    }
}
impl From<u32> for SqliteArg {
    fn from(v: u32) -> Self {
        Self::Int(i64::from(v))
    }
}
impl From<bool> for SqliteArg {
    fn from(v: bool) -> Self {
        Self::Int(i64::from(v))
    }
}
impl From<f64> for SqliteArg {
    fn from(v: f64) -> Self {
        Self::Real(v)
    }
}
impl From<String> for SqliteArg {
    fn from(v: String) -> Self {
        Self::Text(v)
    }
}
impl From<&str> for SqliteArg {
    fn from(v: &str) -> Self {
        Self::Text(v.to_owned())
    }
}
impl From<Vec<u8>> for SqliteArg {
    fn from(v: Vec<u8>) -> Self {
        Self::Blob(v)
    }
}

/// 批量提交配置（§5.3）。
#[derive(Clone, Copy, Debug)]
pub struct BatchConfig {
    pub batch_max_rows: usize,
    pub batch_max_delay_ms: u64,
}

impl Default for BatchConfig {
    fn default() -> Self {
        Self { batch_max_rows: 256, batch_max_delay_ms: 100 }
    }
}

/// 存储门面：`Store` 可克隆给各模块；写走单写者 actor，读走连接池。
#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
    writer_tx: mpsc::UnboundedSender<WriteCmd>,
}

impl Store {
    /// 打开（或创建）数据库并启动单写者 actor。
    pub async fn open(path: &Path, batch: BatchConfig) -> StoreResult<Self> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                tokio::fs::create_dir_all(dir).await.map_err(|e| StoreError::Sql(e.to_string()))?;
            }
        }

        // 读连接池（WAL 多读）
        let read_opts = Self::connect_options(path)?;
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .min_connections(1)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(read_opts)
            .await?;

        // 写连接（独占，单写者 actor）
        let write_opts = Self::connect_options(path)?;
        let write_conn = sqlx::SqliteConnection::connect_with(&write_opts).await?;

        let (writer_tx, writer_rx) = mpsc::unbounded_channel::<WriteCmd>();
        tokio::spawn(writer_actor(write_conn, writer_rx, batch));

        Ok(Self { pool, writer_tx })
    }

    fn connect_options(path: &Path) -> StoreResult<SqliteConnectOptions> {
        Ok(SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(5))
            .foreign_keys(true)
            .auto_vacuum(sqlx::sqlite::SqliteAutoVacuum::Incremental)
            // §5.2 清单
            .pragma("wal_autocheckpoint", "1000")
            .pragma("temp_store", "MEMORY")
            .pragma("cache_size", "-16000")
            .pragma("mmap_size", "268435456"))
    }

    /// 应用 schema（幂等迁移脚本）。
    pub async fn migrate(&self) -> StoreResult<()> {
        let script = crate::storage::schema::SCHEMA_V1;
        let mut tx = self.pool.begin().await?;
        sqlx::raw_sql(script).execute(&mut *tx).await?;
        let now = i64::try_from(crate::core::config::now_ms()).unwrap_or(0);
        sqlx::query("INSERT OR REPLACE INTO schema_version (version, applied_ms) VALUES (1, ?1)")
            .bind(now)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// 提交写命令（fire-and-forget）。
    pub fn write(&self, cmd: WriteCmd) {
        let _ = self.writer_tx.send(cmd);
    }

    /// 提交写命令并等待确认（高优先路径，如额度扣减）。
    pub async fn write_ack(&self, cmd: WriteCmd) -> StoreResult<u64> {
        let (cmd, rx) = cmd.with_ack();
        self.writer_tx.send(cmd).map_err(|_| StoreError::WriterClosed)?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// WAL checkpoint（由 runtime/tasks 定时调用，§5.4）。
    pub async fn wal_checkpoint(&self) -> StoreResult<()> {
        let mut conn = self.pool.acquire().await?;
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)").execute(&mut *conn).await?;
        Ok(())
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

/// 单写者 actor：攒批提交（§5.3）。
async fn writer_actor(
    mut conn: sqlx::SqliteConnection,
    mut rx: mpsc::UnboundedReceiver<WriteCmd>,
    cfg: BatchConfig,
) {
    let mut buf: Vec<WriteCmd> = Vec::with_capacity(cfg.batch_max_rows);
    let mut last_commit = tokio::time::Instant::now();

    loop {
        let deadline = last_commit + Duration::from_millis(cfg.batch_max_delay_ms);
        let cmd = tokio::select! {
            c = rx.recv() => match c {
                Some(c) => c,
                None => break,
            },
            _ = tokio::time::sleep_until(deadline) => {
                if !buf.is_empty() {
                    commit(&mut conn, &mut buf).await;
                    last_commit = tokio::time::Instant::now();
                }
                continue;
            }
        };

        buf.push(cmd);

        // 提交条件（§5.3）：攒满 / 含高优先 / 含等待确认者
        let should_commit = buf.len() >= cfg.batch_max_rows
            || buf.iter().any(|c| c.priority == WritePriority::High)
            || buf.iter().any(|c| c.reply.is_some());

        if should_commit {
            commit(&mut conn, &mut buf).await;
            last_commit = tokio::time::Instant::now();
        }
    }
    if !buf.is_empty() {
        commit(&mut conn, &mut buf).await;
    }
    debug!("writer actor exited");
}

async fn commit(conn: &mut sqlx::SqliteConnection, buf: &mut Vec<WriteCmd>) {
    if buf.is_empty() {
        return;
    }
    let batch = std::mem::take(buf);
    let result: Result<Vec<oneshot::Sender<StoreResult<u64>>>, sqlx::Error> = (async {
        let mut tx = conn.begin().await?;
        let mut acks: Vec<oneshot::Sender<StoreResult<u64>>> = Vec::new();
        for mut cmd in batch {
            let mut q = sqlx::query(cmd.sql);
            for a in &cmd.args {
                q = a.bind_on(q);
            }
            let _r = q.execute(&mut *tx).await?;
            if let Some(reply) = cmd.reply.take() {
                acks.push(reply);
            }
        }
        tx.commit().await?;
        Ok(acks)
    })
    .await;

    match result {
        Ok(acks) => {
            for reply in acks {
                let _ = reply.send(Ok(0));
            }
        }
        Err(e) => {
            warn!(%e, "write batch failed");
            let msg = e.to_string();
            for mut cmd in buf.drain(..) {
                if let Some(reply) = cmd.reply.take() {
                    let _ = reply.send(Err(StoreError::Sql(msg.clone())));
                }
            }
        }
    }
}
/// 行读取辅助：`Option<i64>` 哨兵-free 读取。
pub fn row_i64(row: &sqlx::sqlite::SqliteRow, col: &str) -> StoreResult<Option<i64>> {
    use sqlx::Column;
    let exists = row.columns().iter().any(|c| c.name() == col);
    if !exists {
        return Err(StoreError::Decode(format!("missing column {col}")));
    }
    Ok(row.try_get::<Option<i64>, _>(col)?)
}

/// 共享错误（避免整批携带多个 String）。
pub type SharedErr = Arc<StoreError>;
