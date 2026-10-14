use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("数据库错误: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("输入输出错误: {0}")]
    Io(#[from] std::io::Error),
    #[error("序列化错误: {0}")]
    Json(#[from] serde_json::Error),
    #[error("推理进程: {0}")]
    Infer(String),
    #[error("模型下载: {0}")]
    Download(String),
    #[error("不支持的文件: {0}")]
    Unsupported(String),
    #[error("配置错误: {0}")]
    Config(String),
    #[error("已取消")]
    Cancelled,
}

pub type Result<T> = std::result::Result<T, Error>;
