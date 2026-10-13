use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::error::Error;
use std::path::{Path, PathBuf};

// 写出（合并层），字段名 camelCase 对齐 3D Tiles 规范
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Tileset {
    asset: TilesetAsset,
    geometric_error: f64,
    root: Tile,
}

#[derive(Serialize)]
struct TilesetAsset {
    version: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Tile {
    bounding_volume: BoundingVolume,
    geometric_error: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    children: Option<Vec<Tile>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<TilesetContent>,
}

#[derive(Serialize)]
struct BoundingVolume {
    region: [f64; 6],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TilesetContent {
    uri: String,
}

// 读取（pg2b3dm 输出与合并层同构，serde 默认忽略 transform/refine 等未知字段）
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct SourceTileset {
    geometric_error: f64,
    root: SourceTile,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct SourceTile {
    bounding_volume: SourceBoundingVolume,
    geometric_error: f64,
}

#[derive(Deserialize, Default)]
struct SourceBoundingVolume {
    region: [f64; 6],
}

/// region 并集：[west, south, east, north, minHeight, maxHeight]
/// region 是 EPSG:4979 绝对地理坐标，不受 tile.transform 影响，可直接取并集
fn union_region(regions: &[[f64; 6]]) -> [f64; 6] {
    let mut r = regions[0];
    for x in &regions[1..] {
        r[0] = r[0].min(x[0]);
        r[1] = r[1].min(x[1]);
        r[2] = r[2].max(x[2]);
        r[3] = r[3].max(x[3]);
        r[4] = r[4].min(x[4]);
        r[5] = r[5].max(x[5]);
    }
    r
}

/// 读取 dir 下各 child_dir/tileset.json，生成合并后的 dir/tileset.json。
/// 子条目继承源根的 region 与 geometricError，经 content.uri 引用外部 tileset；
/// 合并层 region 取并集、geometricError 取子层最大值（3d-tiles-tools 通用做法）。
fn write_merged_tileset(dir: &Path, child_dirs: &[String]) -> Result<(), Box<dyn Error>> {
    let mut children = Vec::new();
    let mut regions = Vec::new();
    let mut max_root_ge = 0.0f64;
    let mut max_ts_ge = 0.0f64;
    for child in child_dirs {
        let path = dir.join(child).join("tileset.json");
        let data = match std::fs::read_to_string(&path) {
            Ok(d) => d,
            // 空县在流水线中被跳过、未生成 tileset.json，合并时同样略过
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("读取 {} 失败: {}", path.display(), e).into()),
        };
        let ts: SourceTileset = serde_json::from_str(&data)
            .map_err(|e| format!("解析 {} 失败: {}", path.display(), e))?;
        regions.push(ts.root.bounding_volume.region);
        max_root_ge = max_root_ge.max(ts.root.geometric_error);
        max_ts_ge = max_ts_ge.max(ts.geometric_error);
        children.push(Tile {
            bounding_volume: BoundingVolume {
                region: ts.root.bounding_volume.region,
            },
            geometric_error: ts.root.geometric_error,
            children: None,
            content: Some(TilesetContent {
                uri: format!("{}/tileset.json", child),
            }),
        });
    }
    // 子级全部被跳过时本级无可合并内容，直接返回（父级同样会略过本级）
    if children.is_empty() {
        return Ok(());
    }
    let merged = Tileset {
        asset: TilesetAsset { version: "1.0" },
        geometric_error: max_ts_ge,
        root: Tile {
            bounding_volume: BoundingVolume {
                region: union_region(&regions),
            },
            geometric_error: max_root_ge,
            children: Some(children),
            content: None,
        },
    };
    std::fs::write(dir.join("tileset.json"), serde_json::to_string_pretty(&merged)?)?;
    Ok(())
}

/// 自底向上合并：县（pg2b3dm 生成）→ 市 → 省 → 根 {dest}/buildings/tileset.json
pub fn merge_tilesets(codes: &[u64], home: &str) -> Result<(), Box<dyn Error>> {
    let base = PathBuf::from(home).join("buildings");
    // 省→市→县 三级目录树（目录名保持 2 位零填充，BTreeMap 保证有序、输出幂等）
    let mut tree: BTreeMap<String, BTreeMap<String, Vec<String>>> = BTreeMap::new();
    for code in codes {
        let s = format!("{:06}", code);
        tree.entry(s[0..2].to_string())
            .or_default()
            .entry(s[2..4].to_string())
            .or_default()
            .push(s[4..6].to_string());
    }
    for (province, cities) in &tree {
        // 县 → 市
        for (city, counties) in cities {
            write_merged_tileset(&base.join(province).join(city), counties)?;
        }
        // 市 → 省
        let city_dirs: Vec<String> = cities.keys().cloned().collect();
        write_merged_tileset(&base.join(province), &city_dirs)?;
    }
    // 省 → 根
    let province_dirs: Vec<String> = tree.keys().cloned().collect();
    write_merged_tileset(&base, &province_dirs)?;
    Ok(())
}
