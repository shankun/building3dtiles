use serde::Deserialize;
use std::error::Error;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub postgresql: PgConfig,
}

#[derive(Debug, Deserialize)]
pub struct PgConfig {
    pub server: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: String,
}

/// 把错误及其 source 链拼成一行：tokio-postgres 顶层只显示 "db error"，详情在 source 里
pub fn err_chain(e: &dyn Error) -> String {
    let mut msg = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        msg.push_str(" | ");
        msg.push_str(&s.to_string());
        src = s.source();
    }
    msg
}

pub fn read_config<P: AsRef<Path>>(path: P) -> Result<Config, Box<dyn Error>> {
    let content = std::fs::read_to_string(path)?;
    let cfg: Config = toml::from_str(&content)?;
    Ok(cfg)
}
