use std::error::Error;
use std::path::Path;

use tokio_postgres::Client;

use crate::config::{err_chain, PgConfig};

/// 对单个 dt_building_{code} 表执行步骤 1-7：重建表、校验、SRID 修正、拉伸、建索引、pg2b3dm 导出、删除中间表
pub async fn process_county(
    client: &Client,
    code: &str,
    pg: &PgConfig,
    home: &str,
) -> Result<(), Box<dyn Error>> {
    let src = format!("dt_building_{}", code);
    let dst = format!("extract_footprint_{}", code);
    // 表名仅含字母数字下划线，仍统一走标识符转义，防止注入
    let ident = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));

    // 0. 输出目录；已存在 tileset.json 说明该县此前已导出完成，直接跳过以支持断点续跑
    let (province, city, county) = (&code[0..2], &code[2..4], &code[4..6]);
    let out_dir = format!("{}/buildings/{}/{}/{}", home, province, city, county);
    if Path::new(&out_dir).join("tileset.json").exists() {
        println!("  已存在输出，跳过");
        return Ok(());
    }

    // 0.5 源表可能为空（无任何非空几何）：pg2b3dm 导不出瓦片，直接跳过该县
    let sql = format!(
        "SELECT COUNT(*) FROM {} WHERE the_geom IS NOT NULL",
        ident(&src)
    );
    let non_null: i64 = client.query_one(&sql, &[]).await?.get(0);
    if non_null == 0 {
        println!("  源表无有效几何（空表），跳过");
        return Ok(());
    }

    // 1. 幂等重建三维模型表
    let sql = format!("DROP TABLE IF EXISTS {}", ident(&dst));
    client.execute(&sql, &[]).await?;
    // 轮廓里有两类数据，拉伸后会在 Cesium 里变成横在街面上的碎裂尖刺：
    // 1. 相距约 1 厘米、或偏离墙线不到约 0.15 米的近共线顶点，三角化后是几十米长的薄片；
    // 2. 窄于约 4 米、长几十米的扁条，以及从主体伸出去的同样窄的尖翼。
    // 相邻重复点和多出来的首尾点必须在 ST_MakeValid 之前按约 1 厘米去掉，否则 GEOS 会把它们
    // 当成自相交并改写轮廓。正常闭合所需的那一个首尾点保留。
    // 开运算（先缩 2 米再胀回）削掉窄于 4 米的部分。整栋都被削没时：长边超过 20 米且至少是
    // 短边 6 倍的扁条丢弃，小而紧凑的房屋保留简化后的轮廓。
    let sql = format!(
        "CREATE TABLE {} AS \
         WITH cleaned AS ( \
           SELECT fid, \
             COALESCE(height, FLOOR(RANDOM() * 18) + 3) AS height, \
             ST_CollectionExtract(ST_MakeValid(ST_RemoveRepeatedPoints( \
               ST_SnapToGrid(ST_SetSRID(the_geom, \
                 CASE WHEN ST_SRID(the_geom) = 0 THEN 4326 ELSE ST_SRID(the_geom) END), 1e-7), \
               1e-7)), 3) AS g \
           FROM {} WHERE the_geom IS NOT NULL \
         ), \
         simp AS ( \
           SELECT fid, height, \
             ST_CollectionExtract(ST_MakeValid(ST_SimplifyPreserveTopology(g, 1.5e-6)), 3) AS g \
           FROM cleaned \
           WHERE g IS NOT NULL AND NOT ST_IsEmpty(g) \
         ), \
         opened AS ( \
           SELECT fid, height, g, \
             ST_CollectionExtract(ST_MakeValid( \
               ST_Buffer(ST_Buffer(g::geography, -2), 2)::geometry), 3) AS o \
           FROM simp \
         ) \
         SELECT fid, ST_Force3D(geom, 0) AS geom, height FROM ( \
           SELECT fid, height, \
             CASE \
               WHEN o IS NOT NULL AND NOT ST_IsEmpty(o) THEN o \
               WHEN blade.long_m > 20 AND blade.long_m >= 6 * blade.short_m THEN NULL \
               ELSE g \
             END AS geom \
           FROM opened \
           LEFT JOIN LATERAL ( \
             SELECT GREATEST(d1, d2) AS long_m, LEAST(d1, d2) AS short_m \
             FROM ( \
               SELECT \
                 ST_Distance(ST_PointN(ring, 1)::geography, ST_PointN(ring, 2)::geography) AS d1, \
                 ST_Distance(ST_PointN(ring, 2)::geography, ST_PointN(ring, 3)::geography) AS d2 \
               FROM (SELECT ST_ExteriorRing(ST_OrientedEnvelope(g)) AS ring) env \
               WHERE ST_GeometryType(ST_OrientedEnvelope(g)) = 'ST_Polygon' \
             ) sides \
           ) blade ON true \
         ) chosen \
         WHERE geom IS NOT NULL AND NOT ST_IsEmpty(geom)",
        ident(&dst),
        ident(&src)
    );
    client.execute(&sql, &[]).await?;

    // 2. 校验查询：仅执行以暴露错误，不打印结果
    let sql = format!(
        "SELECT ST_AsText(ST_Multi(ST_ForcePolygonCCW(geom))) FROM {}",
        ident(&dst)
    );
    let rows = client.query(&sql, &[]).await?;
    println!("  几何校验通过（{} 行）", rows.len());

    // 3. 将 SRID 为 0 的几何设置为 4326
    let sql = format!(
        "UPDATE {} SET geom = ST_SetSRID(geom, 4326) WHERE ST_SRID(geom) = 0",
        ident(&dst)
    );
    client.execute(&sql, &[]).await?;

    // 4. 按楼高拉伸，height 为 NULL 时取 3~15 之间随机整数。
    //    ST_Extrude 遇无效几何会报错中断，故仅处理有效几何（步骤 1 已清洗，此处兜底）
    let sql = format!(
        "SELECT COUNT(*) FROM {} WHERE geom IS NOT NULL AND NOT ST_IsValid(geom)",
        ident(&dst)
    );
    let invalid: i64 = client.query_one(&sql, &[]).await?.get(0);
    if invalid > 0 {
        println!("  警告: {} 个几何仍无效，拉伸时跳过", invalid);
    }
    let extrude = format!(
        "UPDATE {} SET geom = ST_Multi(ST_CollectionExtract(\
         ST_Extrude(geom, 0, 0, height), 3)) \
         WHERE geom IS NOT NULL AND ST_IsValid(geom)",
        ident(&dst)
    );
    if let Err(e) = client.execute(&extrude, &[]).await {
        // 整条 UPDATE 出错会全部回滚。定位拖后腿的行、跳过它们后重试，直到成功。
        println!("  批量拉伸失败，定位问题行后跳过重试（原因: {}）", err_chain(&e));
        extrude_skip_bad_rows(client, &dst, &extrude).await?;
    }

    // 5. 为 pg2b3dm 的四叉树瓦片划分建空间索引
    let sql = format!(
        "CREATE INDEX ON {} USING gist(st_centroid(st_envelope(geom)))",
        ident(&dst)
    );
    client.execute(&sql, &[]).await?;

    // 6. 调用 pg2b3dm 导出 3D Tiles 到 {dest}/buildings/{省}/{市}/{县}/
    std::fs::create_dir_all(&out_dir)?;
    run_pg2b3dm(&dst, pg, &out_dir).await?;

    // 6.5 pg2b3dm 输出的 tileset 标注 3D Tiles 1.1，但除版本号外内容完全符合 1.0；
    //     生产环境 Cesium 1.94.2 只接受 0.0/1.0，否则加载即抛 "Tileset version not supported"
    patch_tileset_asset_version(&out_dir)?;

    // 7. 导出完成后删除中间表，释放磁盘空间（再次处理该县时步骤 1 会重建）
    let sql = format!("DROP TABLE IF EXISTS {}", ident(&dst));
    client.execute(&sql, &[]).await?;
    println!("  已删除中间表 {}", dst);
    Ok(())
}

/// 批量拉伸失败时：定位拖后腿的行、跳过它们后重试批量拉伸，直到成功（不退化为逐行模式）。
/// 定位分两步：
///   1) 特征识别——环周长不足 1e-8 度（约 1 毫米）的"退化环"，SFCGAL 会报 no length 让整条 UPDATE 回滚；
///   2) 特征识别不到时按 fid 二分——对区间试批量拉伸，失败就对半切，最终锁定到具体行。
/// 被跳过的行保持平面几何，其 fid 会打印到日志。
async fn extrude_skip_bad_rows(
    client: &Client,
    tbl: &str,
    extrude_sql: &str,
) -> Result<(), Box<dyn Error>> {
    let mut skipped: Vec<i64> = Vec::new();
    for attempt in 1..=10 {
        let mut bad = find_degenerate_rings(client, tbl, &skipped).await?;
        if bad.is_empty() {
            bad = find_rows_by_bisection(client, tbl, &skipped).await?;
        }
        bad.retain(|fid| !skipped.contains(fid));
        if bad.is_empty() {
            return Err("批量拉伸持续失败，但定位不到问题行".into());
        }
        println!("  第 {} 次重试：跳过 {} 行", attempt, bad.len());
        for fid in bad.iter().take(20) {
            println!("    跳过 fid = {}", fid);
        }
        if bad.len() > 20 {
            println!("    ...其余 {} 行省略", bad.len() - 20);
        }
        skipped.extend(bad);
        // ST_ZMax 条件避免重复拉伸：二分试探时已拉伸过的区间会被跳过
        let sql = format!(
            "{extrude_sql} AND (ST_ZMax(geom) IS NULL OR ST_ZMax(geom) = 0){}",
            skip_clause("fid", &skipped)
        );
        match client.execute(&sql, &[]).await {
            Ok(_) => {
                println!(
                    "  跳过 {} 行后批量拉伸成功（这些行保持平面）",
                    skipped.len()
                );
                return Ok(());
            }
            Err(e) => println!("  仍有行拉伸失败: {}", err_chain(&e)),
        }
    }
    Err("重试 10 次后仍无法完成批量拉伸".into())
}

/// 把 dir 下所有 tileset JSON 的 asset.version 从 1.1 改写为 1.0。
/// Cesium < 1.104 解析 tileset 时只接受 0.0/1.0，而 pg2b3dm（关闭 implicit tiling 后）
/// 输出的内容除版本号外完全符合 3D Tiles 1.0。tileset*.json 均在目录顶层，无需递归。
fn patch_tileset_asset_version(dir: &str) -> Result<(), Box<dyn Error>> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().map_or(false, |e| e == "json") {
            let text = std::fs::read_to_string(&path)?;
            let patched = text
                .replace("\"version\": \"1.1\"", "\"version\": \"1.0\"")
                .replace("\"version\":\"1.1\"", "\"version\":\"1.0\"");
            if patched != text {
                std::fs::write(&path, patched)?;
            }
        }
    }
    Ok(())
}

/// 特征识别：找出含"退化环"的行——环周长不足 1e-8 度时 SFCGAL 拉伸会报
/// "LineString is invalid : no length"。全表扫描实测 21 万行约 0.5 秒，仅在批量拉伸失败时执行。
async fn find_degenerate_rings(
    client: &Client,
    tbl: &str,
    skipped: &[i64],
) -> Result<Vec<i64>, Box<dyn Error>> {
    let ident = format!("\"{}\"", tbl.replace('"', "\"\""));
    // ST_DumpRings 只能作用于单个多边形，故先用 ST_Dump 把 MultiPolygon 拆开
    let sql = format!(
        "SELECT DISTINCT q.fid FROM (\
           SELECT d.fid, r.geom AS ring FROM (\
             SELECT fid, (ST_Dump(geom)).geom AS p FROM {ident} WHERE geom IS NOT NULL\
           ) d, LATERAL ST_DumpRings(d.p) r WHERE ST_GeometryType(d.p) = 'ST_Polygon'\
         ) q WHERE ST_Perimeter(q.ring) < 1e-8{}",
        skip_clause("q.fid", skipped)
    );
    Ok(client
        .query(&sql, &[])
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect())
}

/// 失败原因识别不出时的通用定位法：对 fid 区间整体试拉伸，失败就二分继续切，
/// 直到区间只剩一行——那一行就是问题行。试探成功的区间顺带完成了拉伸。
async fn find_rows_by_bisection(
    client: &Client,
    tbl: &str,
    skipped: &[i64],
) -> Result<Vec<i64>, Box<dyn Error>> {
    let ident = format!("\"{}\"", tbl.replace('"', "\"\""));
    let flat = "geom IS NOT NULL AND ST_IsValid(geom) AND (ST_ZMax(geom) IS NULL OR ST_ZMax(geom) = 0)";
    let sql = format!(
        "SELECT MIN(fid), MAX(fid) FROM {ident} WHERE {flat}{}",
        skip_clause("fid", skipped)
    );
    let row = client.query_one(&sql, &[]).await?;
    let (lo, hi): (Option<i64>, Option<i64>) = (row.get(0), row.get(1));
    let (lo, hi) = match (lo, hi) {
        (Some(a), Some(b)) => (a, b),
        _ => return Ok(Vec::new()),
    };
    // fid 上建索引，区间试探才走索引扫描而不是全表扫描
    let sql = format!("CREATE INDEX IF NOT EXISTS {tbl}_fid_idx ON {ident} (fid)");
    client.execute(&sql, &[]).await?;

    let mut bad = Vec::new();
    let mut stack = vec![(lo, hi)];
    while let Some((lo, hi)) = stack.pop() {
        let sql = format!(
            "UPDATE {ident} SET geom = ST_Multi(ST_CollectionExtract(\
             ST_Extrude(geom, 0, 0, COALESCE(height, FLOOR(RANDOM() * 12) + 3)), 3)) \
             WHERE {flat} AND fid BETWEEN {lo} AND {hi}{}",
            skip_clause("fid", skipped)
        );
        if let Err(e) = client.execute(&sql, &[]).await {
            if lo >= hi {
                println!("    唯一候选 fid = {} 无法拉伸: {}", lo, err_chain(&e));
                bad.push(lo);
            } else {
                let mid = lo + (hi - lo) / 2;
                stack.push((lo, mid));
                stack.push((mid + 1, hi));
            }
        }
    }
    Ok(bad)
}

/// 生成 " AND fid <> ALL(ARRAY[...])" 形式的排除条件；skipped 为空时返回空串。
/// fid 取自数据库的 bigint，直接内联拼写是安全的。
fn skip_clause(column: &str, skipped: &[i64]) -> String {
    if skipped.is_empty() {
        return String::new();
    }
    let list = skipped
        .iter()
        .map(|fid| fid.to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!(" AND {column} <> ALL(ARRAY[{list}]::bigint[])")
}

/// 调用与本程序同目录的 pg2b3dm 导出 3D Tiles，密码经 PGPASSWORD 环境变量传递
async fn run_pg2b3dm(table: &str, pg: &PgConfig, out_dir: &str) -> Result<(), Box<dyn Error>> {
    let exe_dir = std::env::current_exe()?
        .parent()
        .ok_or("无法获取可执行文件所在目录")?
        .to_path_buf();
    let pg2b3dm = exe_dir.join("pg2b3dm");
    if !pg2b3dm.exists() {
        return Err(format!(
            "未找到 pg2b3dm: {}（请以 cargo run --release 运行）",
            pg2b3dm.display()
        )
        .into());
    }
    let connection = format!(
        "Host={};Username={};Database={};CommandTimeOut=0",
        pg.server, pg.user, pg.database
    );
    // 子进程继承 stdio，pg2b3dm 自身的进度与错误直接透出
    let status = tokio::process::Command::new(&pg2b3dm)
        .arg("--connection")
        .arg(&connection)
        .arg("-c")
        .arg("geom")
        .arg("-t")
        .arg(table)
        .arg("-a").arg("height")
        .arg("--create_gltf").arg("false")
        .arg("--add_outlines").arg("true")
        // .arg("--keep_projection").arg("true")
        // .arg("-g").arg("1000")
        .arg("--use_implicit_tiling").arg("false")
        .arg("-o")
        .arg(format!("{}/", out_dir))
        .env("PGPASSWORD", &pg.password)
        .status()
        .await?;
    if !status.success() {
        return Err(format!("pg2b3dm 导出 {} 失败: {}", table, status).into());
    }
    Ok(())
}
