mod config;
mod optimize;
mod pipeline;
mod tileset;

use std::error::Error;
use std::path::Path;

use tokio_postgres::NoTls;

use config::{err_chain, read_config};
use optimize::optimize_existing_tiles;
use pipeline::process_county;
use tileset::merge_tilesets;

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        // 逐层输出 error source 链（tokio-postgres 的 "db error" 详情在 source 里）
        eprintln!("Error: {}", e);
        let mut src = e.source();
        while let Some(s) = src {
            eprintln!("  原因: {}", s);
            src = s.source();
        }
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn Error>> {
    // 传入一个目录时，把它当作 3D Tiles 根：缺 tileset.json 就按层级补齐，再改 geometricError
    let mut args = std::env::args().skip(1);
    if let Some(root) = args.next() {
        if args.next().is_some() {
            return Err("只接受一个参数：已生成的 3D Tiles 根目录".into());
        }
        optimize_existing_tiles(Path::new(&root))?;
        return Ok(());
    }

    // 读取连接配置
    let cfg = read_config("connection.toml")?;

    // 构造连接字符串
    let conn_str = format!(
        "host={} port={} user={} password={} dbname={}",
        cfg.postgresql.server,
        cfg.postgresql.port,
        cfg.postgresql.user,
        cfg.postgresql.password,
        cfg.postgresql.database
    );

    let (client, connection) = tokio_postgres::connect(&conn_str, NoTls).await?;

    // 在后台线程运行连接
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("数据库连接错误: {}", e);
        }
    });

    // 查询所有以 dt_ 开头的表名
    let rows = client
        .query(
            "SELECT tablename FROM pg_tables WHERE schemaname='public'
               AND tablename LIKE 'dt_%'
             ORDER BY tablename;",
            &[],
        )
        .await?;

    // 提取后缀行政区划编码，并按数字大小排序后输出
    let mut codes: Vec<u64> = Vec::new();
    for row in &rows {
        let name: String = row.get(0);
        // 表名形如 dt_building_232722，取下划线最后一段作为行政区划编码
        if let Some(suffix) = name.strip_prefix("dt_") {
            let code_str = suffix.rsplit_once('_').map_or(suffix, |(_, tail)| tail);
            if let Ok(code) = code_str.parse::<u64>() {
                codes.push(code);
            }
        }
    }
    codes.sort_unstable();

    // 统计所有 dt_* 表中 height IS NULL 的记录总数与记录总数
    let mut missing: i64 = 0;
    let mut total: i64 = 0;
    for row in &rows {
        let name: String = row.get(0);
        // 双引号包裹标识符，并把内部的 " 转义为 ""，防止注入
        let ident = format!("\"{}\"", name.replace('"', "\"\""));
        let sql = format!(
            "SELECT COUNT(*), COUNT(*) FILTER (WHERE height IS NULL) FROM {}",
            ident
        );
        let r = client.query_one(&sql, &[]).await?;
        total += r.get::<_, i64>(0);
        missing += r.get::<_, i64>(1);
    }
    println!("缺少height字段的记录数：{} / {}", missing, total);

    // 3D Tiles 流水线：逐表处理并导出，合并后再按层级改写 geometricError
    let home = std::env::var("HOME")?;
    let n = codes.len();
    for (i, code) in codes.iter().enumerate() {
        let code_str = format!("{:06}", code);
        println!("[{}/{}] 处理 dt_building_{} ...", i + 1, n, code_str);
        // 用 err_chain 展开 source 链，避免只看到 "db error" 而丢失真实原因
        if let Err(e) = process_county(&client, &code_str, &cfg.postgresql, &home).await {
            return Err(format!("处理 dt_building_{} 失败: {}", code_str, err_chain(e.as_ref())).into());
        }
        println!("[{}/{}] dt_building_{} 完成", i + 1, n, code_str);
    }

    merge_tilesets(&codes, &home)?;
    let buildings = Path::new(&home).join("buildings");
    optimize_existing_tiles(&buildings)?;
    println!("根 tileset.json 已生成: {}/tileset.json", buildings.display());

    Ok(())
}
