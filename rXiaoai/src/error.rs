use thiserror::Error;

#[derive(Error, Debug)]
pub enum XiaoaiErr {
    #[error("Login failed: {0}")]
    Auth(String),
    #[error("Op failed: {0}")]
    Op(String),
    #[error("Record query failed: {0}")]
    Record(String),
}

pub type Result<T> = std::result::Result<T, XiaoaiErr>;
