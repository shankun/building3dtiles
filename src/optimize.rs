use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::error::Error;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const ROOT_GEOMETRIC_ERROR: f64 = 2000.0;

/// 检查根目录下的 tileset.json，按树深度把 geometricError 设为顶级 2000、每深一级除以 2。
/// 根目录或中途引用的 tileset.json 缺失时，自底向上补齐；已有文件只改误差，不改瓦片结构。
pub fn optimize_existing_tiles(root: &Path) -> Result<(), Box<dyn Error>> {
    if !root.is_dir() {
        return Err(format!("不是目录: {}", root.display()).into());
    }
    let tileset = root.join("tileset.json");
    if !tileset.is_file() {
        let mut created = 0usize;
        let mut visiting = HashSet::new();
        if !generate_missing_tilesets(root, &mut visiting, &mut created)? {
            return Err(format!(
                "未找到 tileset.json，且子目录中没有可合并的 tileset: {}",
                tileset.display()
            )
            .into());
        }
        println!("已生成 {} 个缺失的 tileset.json", created);
    }

    // pg2b3dm 2.27 把四叉树从中间层拆成外部 tileset 时，恰好落在拆分层、
    // 且没有再细分的叶子 b3dm 会写进 content/，但不会出现在任何 tileset.json 里。
    let linked = link_unreferenced_b3dm(root)?;
    if linked > 0 {
        println!("已挂回 {} 个未写入 tileset 的 b3dm", linked);
    }

    let mut seen = HashSet::new();
    rewrite_tileset(&tileset, ROOT_GEOMETRIC_ERROR, &mut seen)?;
    println!(
        "geometricError 已更新：{} 个 tileset，顶级 {}，每下一级为上一级的 1/2",
        seen.len(),
        ROOT_GEOMETRIC_ERROR
    );

    // compress_b3dm_tree(root)?;
    Ok(())
}

/// 自底向上补齐缺失的 tileset.json。目录里已经有 tileset.json 时不再向下生成，
/// 也不改写它的子瓦片结构。返回该目录在调用后是否拥有 tileset.json。
fn generate_missing_tilesets(
    dir: &Path,
    visiting: &mut HashSet<PathBuf>,
    created: &mut usize,
) -> Result<bool, Box<dyn Error>> {
    if dir.join("tileset.json").is_file() {
        return Ok(true);
    }
    let canon = dir.canonicalize().map_err(|e| {
        format!("读取 {} 失败: {}", dir.display(), e)
    })?;
    if !visiting.insert(canon) {
        return Err(format!("目录循环引用: {}", dir.display()).into());
    }

    let mut children = Vec::new();
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir)
        .map_err(|e| format!("读取目录 {} 失败: {}", dir.display(), e))?
    {
        entries.push(entry?.path());
    }
    entries.sort();
    for path in entries {
        if path.is_dir() && generate_missing_tilesets(&path, visiting, created)? {
            children.push(path);
        }
    }
    if children.is_empty() {
        return Ok(false);
    }
    write_directory_tileset(dir, &children)?;
    *created += 1;
    Ok(true)
}

/// 用子目录已有的 tileset.json 写出本层 tileset。region 取并集，
/// geometricError 先占位，随后由 rewrite_tileset 按层级覆盖。
fn write_directory_tileset(dir: &Path, child_dirs: &[PathBuf]) -> Result<(), Box<dyn Error>> {
    let mut children = Vec::new();
    let mut regions = Vec::new();
    for child in child_dirs {
        let path = child.join("tileset.json");
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("读取 {} 失败: {}", path.display(), e))?;
        let value: Value = serde_json::from_str(&text)
            .map_err(|e| format!("解析 {} 失败: {}", path.display(), e))?;
        let region = root_region(&value, &path)?;
        let name = child
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| format!("无法取得目录名: {}", child.display()))?;
        regions.push(region);
        children.push(json!({
            "boundingVolume": { "region": region },
            "geometricError": 0.0,
            "content": { "uri": format!("{name}/tileset.json") }
        }));
    }
    let merged = json!({
        "asset": { "version": "1.0" },
        "geometricError": 0.0,
        "root": {
            "boundingVolume": { "region": union_region(&regions) },
            "geometricError": 0.0,
            "children": children
        }
    });
    let mut out = serde_json::to_vec_pretty(&merged)?;
    out.push(b'\n');
    std::fs::write(dir.join("tileset.json"), out)?;
    Ok(())
}

fn root_region(value: &Value, path: &Path) -> Result<[f64; 6], Box<dyn Error>> {
    let region = value
        .pointer("/root/boundingVolume/region")
        .and_then(|v| v.as_array())
        .ok_or_else(|| format!("{} 缺少 root.boundingVolume.region", path.display()))?;
    if region.len() != 6 {
        return Err(format!("{} 的 region 不是 6 个数", path.display()).into());
    }
    let mut out = [0.0; 6];
    for (i, n) in region.iter().enumerate() {
        out[i] = n
            .as_f64()
            .ok_or_else(|| format!("{} 的 region[{i}] 不是数字", path.display()))?;
    }
    Ok(out)
}

/// region 并集：[west, south, east, north, minHeight, maxHeight]
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

/// 把目录树里已经生成、但没有任何 tileset 引用的 b3dm 挂回所在 tileset 的根节点。
fn link_unreferenced_b3dm(root: &Path) -> Result<usize, Box<dyn Error>> {
    let mut linked = 0usize;
    let mut visiting = HashSet::new();
    link_b3dm_in_dir(root, &mut visiting, &mut linked)?;
    Ok(linked)
}

fn link_b3dm_in_dir(
    dir: &Path,
    visiting: &mut HashSet<PathBuf>,
    linked: &mut usize,
) -> Result<(), Box<dyn Error>> {
    let canon = dir.canonicalize().map_err(|e| {
        format!("读取 {} 失败: {}", dir.display(), e)
    })?;
    if !visiting.insert(canon) {
        return Ok(());
    }
    let tileset_path = dir.join("tileset.json");
    if tileset_path.is_file() {
        *linked += link_dir_orphans(dir, &tileset_path)?;
    }
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir)
        .map_err(|e| format!("读取目录 {} 失败: {}", dir.display(), e))?
    {
        let path = entry?.path();
        if path.is_dir() {
            entries.push(path);
        }
    }
    entries.sort();
    for path in entries {
        link_b3dm_in_dir(&path, visiting, linked)?;
    }
    Ok(())
}

fn link_dir_orphans(dir: &Path, tileset_path: &Path) -> Result<usize, Box<dyn Error>> {
    let content_dir = dir.join("content");
    if !content_dir.is_dir() {
        return Ok(0);
    }
    let text = std::fs::read_to_string(tileset_path)
        .map_err(|e| format!("读取 {} 失败: {}", tileset_path.display(), e))?;
    let mut value: Value = serde_json::from_str(&text)
        .map_err(|e| format!("解析 {} 失败: {}", tileset_path.display(), e))?;
    let mut referenced = HashSet::new();
    if let Some(root) = value.get("root") {
        collect_referenced_b3dm(dir, root, &mut referenced)?;
    }
    let mut orphans = Vec::new();
    for entry in std::fs::read_dir(&content_dir)
        .map_err(|e| format!("读取目录 {} 失败: {}", content_dir.display(), e))?
    {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("b3dm") {
            continue;
        }
        let key = path.canonicalize().unwrap_or(path.clone());
        if !referenced.contains(&key) {
            orphans.push(path);
        }
    }
    orphans.sort();
    if orphans.is_empty() {
        return Ok(0);
    }
    let translation = root_translation(&value, tileset_path)?;
    let mut added = Vec::new();
    for path in &orphans {
        let bytes = std::fs::read(path)
            .map_err(|e| format!("读取 {} 失败: {}", path.display(), e))?;
        let region = b3dm_region(&bytes, translation).map_err(|e| {
            format!("{}: {}", path.display(), e)
        })?;
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| format!("无法取得文件名: {}", path.display()))?;
        added.push((region, format!("content/{name}")));
    }
    let children = value["root"]
        .as_object_mut()
        .ok_or_else(|| format!("{} 缺少 root", tileset_path.display()))?
        .entry("children")
        .or_insert_with(|| json!([]));
    let children = children
        .as_array_mut()
        .ok_or("tileset root.children 不是数组")?;
    for (region, uri) in &added {
        children.push(json!({
            "boundingVolume": { "region": region },
            "geometricError": 0.0,
            "refine": "ADD",
            "content": { "uri": uri }
        }));
    }
    if let Some(existing) = value.pointer("/root/boundingVolume/region").and_then(|v| v.as_array()) {
        if existing.len() == 6 {
            let mut regions = vec![added[0].0];
            if let Ok(current) = root_region(&value, tileset_path) {
                regions.push(current);
            }
            for (region, _) in &added[1..] {
                regions.push(*region);
            }
            value["root"]["boundingVolume"]["region"] = json!(union_region(&regions));
        }
    }
    let mut out = serde_json::to_vec_pretty(&value)?;
    out.push(b'\n');
    std::fs::write(tileset_path, out)?;
    Ok(added.len())
}

fn collect_referenced_b3dm(
    dir: &Path,
    tile: &Value,
    out: &mut HashSet<PathBuf>,
) -> Result<(), Box<dyn Error>> {
    if let Some(uri) = content_uri(tile) {
        let uri = uri.replace('\\', "/");
        let path = dir.join(&uri);
        let lower = uri.to_ascii_lowercase();
        if lower.ends_with(".b3dm") {
            out.insert(path.canonicalize().unwrap_or(path));
        } else if lower.ends_with(".json") && path.is_file() {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("读取 {} 失败: {}", path.display(), e))?;
            let doc: Value = serde_json::from_str(&text)
                .map_err(|e| format!("解析 {} 失败: {}", path.display(), e))?;
            if let Some(root) = doc.get("root") {
                let parent = path.parent().unwrap_or(dir);
                collect_referenced_b3dm(parent, root, out)?;
            }
        }
    }
    if let Some(children) = tile.get("children").and_then(|c| c.as_array()) {
        for child in children {
            collect_referenced_b3dm(dir, child, out)?;
        }
    }
    Ok(())
}

fn root_translation(value: &Value, path: &Path) -> Result<[f64; 3], Box<dyn Error>> {
    let transform = value
        .pointer("/root/transform")
        .and_then(|v| v.as_array())
        .ok_or_else(|| format!("{} 缺少 root.transform，无法定位 b3dm", path.display()))?;
    if transform.len() != 16 {
        return Err(format!("{} 的 transform 不是 16 个数", path.display()).into());
    }
    let mut out = [0.0; 3];
    for i in 0..3 {
        out[i] = transform[12 + i]
            .as_f64()
            .ok_or_else(|| format!("{} 的 transform 平移不是数字", path.display()))?;
    }
    Ok(out)
}

/// 用顶点相对坐标加上 tileset 平移得到 ECEF，再换成 region。
/// glTF 节点上的 Y-up 矩阵不参与这次换算，顶点本身已经是 ECEF 偏移。
fn b3dm_region(data: &[u8], translation: [f64; 3]) -> Result<[f64; 6], Box<dyn Error>> {
    if data.len() < 28 || &data[0..4] != b"b3dm" {
        return Err("不是 b3dm".into());
    }
    let ft_json = u32::from_le_bytes(data[12..16].try_into().unwrap()) as usize;
    let ft_bin = u32::from_le_bytes(data[16..20].try_into().unwrap()) as usize;
    let bt_json = u32::from_le_bytes(data[20..24].try_into().unwrap()) as usize;
    let bt_bin = u32::from_le_bytes(data[24..28].try_into().unwrap()) as usize;
    let glb_at = 28 + ft_json + ft_bin + bt_json + bt_bin;
    if glb_at + 12 > data.len() || &data[glb_at..glb_at + 4] != b"glTF" {
        return Err("b3dm 内未找到 glb".into());
    }
    let glb_len = u32::from_le_bytes(data[glb_at + 8..glb_at + 12].try_into().unwrap()) as usize;
    let glb = data
        .get(glb_at..glb_at + glb_len)
        .ok_or("glb 长度超出文件")?;
    let (doc, bin) = glb_parts(glb)?;
    let mut region: Option<[f64; 6]> = None;
    let meshes = doc
        .get("meshes")
        .and_then(|m| m.as_array())
        .ok_or("glb 缺少 meshes")?;
    for mesh in meshes {
        let Some(prims) = mesh.get("primitives").and_then(|p| p.as_array()) else {
            continue;
        };
        for prim in prims {
            let Some(index) = prim.pointer("/attributes/POSITION").and_then(|v| v.as_u64()) else {
                continue;
            };
            for_each_position(&doc, bin, index as usize, &mut |x, y, z| {
                let (lon, lat, h) = ecef_to_geodetic(
                    f64::from(x) + translation[0],
                    f64::from(y) + translation[1],
                    f64::from(z) + translation[2],
                );
                match &mut region {
                    None => region = Some([lon, lat, lon, lat, h, h]),
                    Some(r) => {
                        r[0] = r[0].min(lon);
                        r[1] = r[1].min(lat);
                        r[2] = r[2].max(lon);
                        r[3] = r[3].max(lat);
                        r[4] = r[4].min(h);
                        r[5] = r[5].max(h);
                    }
                }
            })?;
        }
    }
    region.ok_or_else(|| "b3dm 没有 POSITION".into())
}

fn glb_parts(glb: &[u8]) -> Result<(Value, &[u8]), Box<dyn Error>> {
    let mut json = None;
    let mut bin = None;
    let mut offset = 12;
    while offset + 8 <= glb.len() {
        let len = u32::from_le_bytes(glb[offset..offset + 4].try_into().unwrap()) as usize;
        let kind = u32::from_le_bytes(glb[offset + 4..offset + 8].try_into().unwrap());
        let start = offset + 8;
        let end = start + len;
        if end > glb.len() {
            return Err("glb 块超出文件".into());
        }
        if kind == 0x4E4F_534A {
            json = Some(serde_json::from_slice(&glb[start..end])?);
        } else if kind == 0x004E_4942 {
            bin = Some(&glb[start..end]);
        }
        offset = end;
    }
    Ok((
        json.ok_or("glb 缺少 JSON")?,
        bin.ok_or("glb 缺少 BIN")?,
    ))
}

fn for_each_position(
    doc: &Value,
    bin: &[u8],
    index: usize,
    visit: &mut dyn FnMut(f32, f32, f32),
) -> Result<(), Box<dyn Error>> {
    let acc = doc
        .get("accessors")
        .and_then(|a| a.as_array())
        .and_then(|a| a.get(index))
        .ok_or("POSITION accessor 不存在")?;
    if acc.get("componentType").and_then(|v| v.as_u64()) != Some(5126)
        || acc.get("type").and_then(|v| v.as_str()) != Some("VEC3")
    {
        return Err("POSITION 不是 float VEC3".into());
    }
    let view_index = acc
        .get("bufferView")
        .and_then(|v| v.as_u64())
        .ok_or("POSITION 没有 bufferView，无法从压缩网格计算范围")? as usize;
    let view = doc
        .get("bufferViews")
        .and_then(|v| v.as_array())
        .and_then(|v| v.get(view_index))
        .ok_or("bufferView 不存在")?;
    let offset = view.get("byteOffset").and_then(|v| v.as_u64()).unwrap_or(0) as usize
        + acc.get("byteOffset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let stride = view.get("byteStride").and_then(|v| v.as_u64()).unwrap_or(12) as usize;
    let count = acc
        .get("count")
        .and_then(|v| v.as_u64())
        .ok_or("POSITION 缺少 count")? as usize;
    if stride < 12 {
        return Err("POSITION 步长无效".into());
    }
    for i in 0..count {
        let at = offset + i * stride;
        let bytes = bin.get(at..at + 12).ok_or("POSITION 超出缓冲区")?;
        let x = f32::from_le_bytes(bytes[0..4].try_into().unwrap());
        let y = f32::from_le_bytes(bytes[4..8].try_into().unwrap());
        let z = f32::from_le_bytes(bytes[8..12].try_into().unwrap());
        visit(x, y, z);
    }
    Ok(())
}

fn ecef_to_geodetic(x: f64, y: f64, z: f64) -> (f64, f64, f64) {
    let a = 6_378_137.0;
    let f = 1.0 / 298.257_223_563;
    let e2 = f * (2.0 - f);
    let lon = y.atan2(x);
    let p = x.hypot(y);
    let mut lat = z.atan2(p * (1.0 - e2));
    for _ in 0..8 {
        let s = lat.sin();
        let n = a / (1.0 - e2 * s * s).sqrt();
        lat = (z + e2 * n * s).atan2(p);
    }
    let s = lat.sin();
    let n = a / (1.0 - e2 * s * s).sqrt();
    let h = p / lat.cos() - n;
    (lon, lat, h)
}

/// 外部 tileset 的根与引用它的那一块是同一级，因此沿用同一 geometricError；
/// 它的 children 才是下一级。
fn rewrite_tileset(
    path: &Path,
    geometric_error: f64,
    seen: &mut HashSet<PathBuf>,
) -> Result<(), Box<dyn Error>> {
    let canon = path.canonicalize().map_err(|e| {
        format!("读取 {} 失败: {}", path.display(), e)
    })?;
    if !seen.insert(canon) {
        return Ok(());
    }

    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("读取 {} 失败: {}", path.display(), e))?;
    let mut value: Value = serde_json::from_str(&text)
        .map_err(|e| format!("解析 {} 失败: {}", path.display(), e))?;
    if !value.get("root").is_some_and(|r| r.is_object()) {
        return Err(format!("{} 缺少 root", path.display()).into());
    }
    value["geometricError"] = json!(geometric_error);
    let dir = path
        .parent()
        .ok_or_else(|| format!("无法取得目录: {}", path.display()))?
        .to_path_buf();
    assign_tile(&mut value["root"], geometric_error, &dir, seen)?;
    let mut out = serde_json::to_vec_pretty(&value)?;
    out.push(b'\n');
    std::fs::write(path, out)?;
    Ok(())
}

fn assign_tile(
    tile: &mut Value,
    geometric_error: f64,
    dir: &Path,
    seen: &mut HashSet<PathBuf>,
) -> Result<(), Box<dyn Error>> {
    let obj = tile
        .as_object_mut()
        .ok_or("tileset 中的 tile 不是对象")?;
    obj.insert("geometricError".to_string(), json!(geometric_error));

    if let Some(uri) = content_uri(tile) {
        if let Some(child) = referenced_tileset(dir, &uri)? {
            rewrite_tileset(&child, geometric_error, seen)?;
        }
    }

    let n = tile
        .get("children")
        .and_then(|c| c.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    for i in 0..n {
        assign_tile(&mut tile["children"][i], geometric_error / 2.0, dir, seen)?;
    }
    Ok(())
}

fn content_uri(tile: &Value) -> Option<String> {
    tile.get("content")
        .and_then(|c| c.get("uri"))
        .and_then(|u| u.as_str())
        .map(|s| s.to_string())
}

/// content.uri 指向另一份 tileset 时返回其路径。模板 uri（含 `{`）和 b3dm/glb 返回 None。
/// 文件不存在但所在目录还在时，先按子目录自底向上补出这一层，再继续改误差。
fn referenced_tileset(dir: &Path, uri: &str) -> Result<Option<PathBuf>, Box<dyn Error>> {
    if uri.contains('{') || !uri.to_ascii_lowercase().ends_with(".json") {
        return Ok(None);
    }
    let path = dir.join(uri.replace('\\', "/"));
    if !path.is_file() {
        if let Some(parent) = path.parent() {
            if parent.is_dir() {
                let mut visiting = HashSet::new();
                let mut created = 0usize;
                generate_missing_tilesets(parent, &mut visiting, &mut created)?;
                if created > 0 {
                    println!("已生成 {} 个缺失的 tileset.json", created);
                }
            }
        }
    }
    if !path.is_file() {
        return Err(format!("引用的 tileset 不存在: {}", path.display()).into());
    }
    Ok(Some(path))
}

#[allow(dead_code)]
fn collect_b3dm(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), Box<dyn Error>> {
    for entry in std::fs::read_dir(dir).map_err(|e| format!("读取目录 {} 失败: {}", dir.display(), e))? {
        let path = entry?.path();
        if path.is_dir() {
            collect_b3dm(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "b3dm") {
            out.push(path);
        }
    }
    Ok(())
}

#[allow(dead_code)]
fn compress_b3dm_tree(root: &Path) -> Result<(), Box<dyn Error>> {
    let mut files = Vec::new();
    collect_b3dm(root, &mut files)?;
    files.sort();
    let total = files.len();
    println!("压缩 b3dm（Draco + KTX2）：{} 个", total);
    if total == 0 {
        return Ok(());
    }

    let script = compress_script()?;
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 4);
    let queue = Arc::new(Mutex::new(files));
    let done = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(Mutex::new(Vec::<String>::new()));

    std::thread::scope(|scope| {
        for _ in 0..workers {
            let queue = Arc::clone(&queue);
            let done = Arc::clone(&done);
            let failed = Arc::clone(&failed);
            let script = script.clone();
            scope.spawn(move || {
                if let Err(e) = compress_worker(&script, &queue, &done, total, &failed) {
                    failed
                        .lock()
                        .expect("失败列表锁")
                        .push(e.to_string());
                }
            });
        }
    });

    let leftover = queue.lock().expect("任务队列锁").len();
    let failed = failed.lock().expect("失败列表锁");
    if leftover > 0 || !failed.is_empty() {
        for msg in failed.iter().take(20) {
            eprintln!("  {}", msg);
        }
        return Err(format!(
            "{} 个 b3dm 压缩失败，{} 个未处理",
            failed.len(),
            leftover
        )
        .into());
    }
    println!("b3dm 压缩完成：{}", total);
    Ok(())
}

#[allow(dead_code)]
fn compress_script() -> Result<PathBuf, Box<dyn Error>> {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tools/compress_glb.mjs");
    if !script.is_file() {
        return Err(format!("未找到压缩脚本: {}", script.display()).into());
    }
    let modules = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("node_modules/@gltf-transform/core");
    if !modules.is_dir() {
        return Err("未安装压缩依赖，请在项目目录执行 npm install".into());
    }
    Ok(script)
}

#[allow(dead_code)]
fn compress_worker(
    script: &Path,
    queue: &Mutex<Vec<PathBuf>>,
    done: &AtomicUsize,
    total: usize,
    failed: &Mutex<Vec<String>>,
) -> Result<(), Box<dyn Error>> {
    let mut child = Command::new("node")
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("启动 node 压缩进程失败: {}", e))?;
    let mut stdin = child.stdin.take().ok_or("压缩进程没有 stdin")?;
    let stdout = child.stdout.take().ok_or("压缩进程没有 stdout")?;
    let mut stdout = BufReader::new(stdout);

    loop {
        let path = {
            let mut q = queue.lock().expect("任务队列锁");
            q.pop()
        };
        let Some(path) = path else { break };

        let index = done.fetch_add(1, Ordering::Relaxed) + 1;
        println!("  [{}/{}] {}", index, total, path.display());
        if let Err(e) = compress_one_b3dm(&path, &mut stdin, &mut stdout) {
            failed
                .lock()
                .expect("失败列表锁")
                .push(format!("{}: {}", path.display(), e));
        }
    }

    drop(stdin);
    let status = child.wait()?;
    if !status.success() {
        return Err(format!("node 压缩进程退出异常: {}", status).into());
    }
    Ok(())
}

#[allow(dead_code)]
fn compress_one_b3dm(
    path: &Path,
    stdin: &mut impl Write,
    stdout: &mut impl BufRead,
) -> Result<(), Box<dyn Error>> {
    let data = std::fs::read(path)?;
    let (prefix, glb) = split_b3dm(&data)?;
    if glb_has_draco(&glb) {
        return Ok(());
    }

    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    let tmp = std::env::temp_dir().join(format!(
        "chimaera-{}-{:x}",
        std::process::id(),
        hasher.finish()
    ));
    let in_glb = tmp.with_extension("in.glb");
    let out_glb = tmp.with_extension("out.glb");
    std::fs::write(&in_glb, &glb)?;

    writeln!(stdin, "{}\t{}", in_glb.display(), out_glb.display())?;
    stdin.flush()?;
    let mut reply = String::new();
    let n = stdout.read_line(&mut reply)?;
    if n == 0 {
        return Err("压缩进程已关闭".into());
    }
    if !reply.starts_with("OK") {
        let _ = std::fs::remove_file(&in_glb);
        let _ = std::fs::remove_file(&out_glb);
        return Err(reply.trim().to_string().into());
    }

    let compressed = std::fs::read(&out_glb)?;
    let _ = std::fs::remove_file(&in_glb);
    let _ = std::fs::remove_file(&out_glb);
    if compressed.len() < 12 || &compressed[0..4] != b"glTF" {
        return Err("压缩结果不是 glb".into());
    }
    let packed = pack_b3dm(&prefix, &compressed);
    let tmp_b3dm = path.with_extension("b3dm.tmp");
    std::fs::write(&tmp_b3dm, &packed)?;
    std::fs::rename(&tmp_b3dm, path)?;
    Ok(())
}

#[allow(dead_code)]
fn glb_has_draco(glb: &[u8]) -> bool {
    if glb.len() < 20 || &glb[0..4] != b"glTF" {
        return false;
    }
    let json_len = u32::from_le_bytes(glb[12..16].try_into().unwrap()) as usize;
    let json_type = u32::from_le_bytes(glb[16..20].try_into().unwrap());
    if json_type != 0x4E4F_534A || 20 + json_len > glb.len() {
        return false;
    }
    glb[20..20 + json_len]
        .windows(b"KHR_draco_mesh_compression".len())
        .any(|w| w == b"KHR_draco_mesh_compression")
}

/// b3dm = 28 字节头 + feature/batch 表 + glb。glb 长度以 glb 头里的 byteLength 为准，丢掉尾部对齐填充。
#[allow(dead_code)]
fn split_b3dm(data: &[u8]) -> Result<(Vec<u8>, Vec<u8>), Box<dyn Error>> {
    if data.len() < 28 || &data[0..4] != b"b3dm" {
        return Err("不是 b3dm 文件".into());
    }
    let ft_json = u32::from_le_bytes(data[12..16].try_into().unwrap()) as usize;
    let ft_bin = u32::from_le_bytes(data[16..20].try_into().unwrap()) as usize;
    let bt_json = u32::from_le_bytes(data[20..24].try_into().unwrap()) as usize;
    let bt_bin = u32::from_le_bytes(data[24..28].try_into().unwrap()) as usize;
    let start = 28 + ft_json + ft_bin + bt_json + bt_bin;
    if start + 12 > data.len() {
        return Err("b3dm 头长度超出文件".into());
    }
    let glb_region = &data[start..];
    if &glb_region[0..4] != b"glTF" {
        return Err("b3dm 内未找到 glb".into());
    }
    let glb_len = u32::from_le_bytes(glb_region[8..12].try_into().unwrap()) as usize;
    if glb_len < 12 || start + glb_len > data.len() {
        return Err("glb 长度无效".into());
    }
    Ok((data[..start].to_vec(), glb_region[..glb_len].to_vec()))
}

#[allow(dead_code)]
fn pack_b3dm(prefix: &[u8], glb: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(prefix.len() + glb.len() + 7);
    out.extend_from_slice(prefix);
    out.extend_from_slice(glb);
    while out.len() % 8 != 0 {
        out.push(0);
    }
    let len = u32::try_from(out.len()).expect("b3dm 超过 4GB");
    out[8..12].copy_from_slice(&len.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_leaf(dir: &Path, region: [f64; 6]) {
        std::fs::create_dir_all(dir).unwrap();
        let doc = json!({
            "asset": {"version": "1.0"},
            "geometricError": 1.0,
            "root": {
                "boundingVolume": {"region": region},
                "geometricError": 1.0,
                "refine": "ADD",
                "children": [{
                    "boundingVolume": {"region": region},
                    "geometricError": 1.0,
                    "content": {"uri": "content/0.b3dm"}
                }]
            }
        });
        std::fs::write(dir.join("tileset.json"), serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
    }

    fn ge(path: &Path, pointer: &str) -> f64 {
        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        value.pointer(pointer).and_then(|n| n.as_f64()).unwrap()
    }

    #[test]
    fn missing_root_tileset_is_generated_with_halved_error() {
        let root = std::env::temp_dir().join(format!("chimaera-ge-{}", std::process::id()));
        let _guard = TempDir(root.clone());
        let _ = std::fs::remove_dir_all(&root);
        write_leaf(&root.join("11/01/01"), [0.0, 0.0, 1.0, 1.0, 10.0, 20.0]);
        write_leaf(&root.join("12"), [2.0, 3.0, 4.0, 5.0, 0.0, 30.0]);

        optimize_existing_tiles(&root).unwrap();

        assert!(root.join("11/tileset.json").is_file());
        assert!(root.join("11/01/tileset.json").is_file());
        let leaf = std::fs::read_to_string(root.join("11/01/01/tileset.json")).unwrap();
        assert!(leaf.contains("ADD"));

        assert_eq!(ge(&root.join("tileset.json"), "/geometricError"), 2000.0);
        assert_eq!(ge(&root.join("tileset.json"), "/root/geometricError"), 2000.0);
        assert_eq!(ge(&root.join("tileset.json"), "/root/children/0/geometricError"), 1000.0);
        assert_eq!(ge(&root.join("11/tileset.json"), "/geometricError"), 1000.0);
        assert_eq!(ge(&root.join("11/01/tileset.json"), "/geometricError"), 500.0);
        assert_eq!(ge(&root.join("11/01/01/tileset.json"), "/geometricError"), 250.0);
        assert_eq!(ge(&root.join("11/01/01/tileset.json"), "/root/children/0/geometricError"), 125.0);
        assert_eq!(ge(&root.join("12/tileset.json"), "/geometricError"), 1000.0);
        assert_eq!(ge(&root.join("12/tileset.json"), "/root/children/0/geometricError"), 500.0);

        let root_doc: Value =
            serde_json::from_str(&std::fs::read_to_string(root.join("tileset.json")).unwrap()).unwrap();
        let region: Vec<f64> = root_doc
            .pointer("/root/boundingVolume/region")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_f64().unwrap())
            .collect();
        assert_eq!(region, vec![0.0, 0.0, 4.0, 5.0, 0.0, 30.0]);
    }

    fn geodetic_to_ecef(lon_deg: f64, lat_deg: f64, h: f64) -> [f64; 3] {
        let a = 6_378_137.0;
        let f = 1.0 / 298.257_223_563;
        let e2 = f * (2.0 - f);
        let lon = lon_deg.to_radians();
        let lat = lat_deg.to_radians();
        let n = a / (1.0 - e2 * lat.sin() * lat.sin()).sqrt();
        [
            (n + h) * lat.cos() * lon.cos(),
            (n + h) * lat.cos() * lon.sin(),
            (n * (1.0 - e2) + h) * lat.sin(),
        ]
    }

    fn minimal_b3dm(positions: &[[f32; 3]]) -> Vec<u8> {
        let mut bin = Vec::new();
        for p in positions {
            bin.extend_from_slice(&p[0].to_le_bytes());
            bin.extend_from_slice(&p[1].to_le_bytes());
            bin.extend_from_slice(&p[2].to_le_bytes());
        }
        let json = format!(
            r#"{{"asset":{{"version":"2.0"}},"meshes":[{{"primitives":[{{"attributes":{{"POSITION":0}}}}]}}],"accessors":[{{"bufferView":0,"componentType":5126,"count":{},"type":"VEC3"}}],"bufferViews":[{{"buffer":0,"byteLength":{}}}],"buffers":[{{"byteLength":{}}}]}}"#,
            positions.len(),
            bin.len(),
            bin.len()
        );
        let mut json = json.into_bytes();
        while json.len() % 4 != 0 {
            json.push(b' ');
        }
        let mut glb = Vec::new();
        glb.extend_from_slice(b"glTF");
        glb.extend_from_slice(&2u32.to_le_bytes());
        glb.extend_from_slice(&0u32.to_le_bytes());
        glb.extend_from_slice(&(json.len() as u32).to_le_bytes());
        glb.extend_from_slice(&0x4E4F_534Au32.to_le_bytes());
        glb.extend_from_slice(&json);
        glb.extend_from_slice(&(bin.len() as u32).to_le_bytes());
        glb.extend_from_slice(&0x004E_4942u32.to_le_bytes());
        glb.extend_from_slice(&bin);
        let total = glb.len() as u32;
        glb[8..12].copy_from_slice(&total.to_le_bytes());

        let mut b3dm = Vec::new();
        b3dm.extend_from_slice(b"b3dm");
        b3dm.extend_from_slice(&1u32.to_le_bytes());
        b3dm.extend_from_slice(&0u32.to_le_bytes());
        b3dm.extend_from_slice(&0u32.to_le_bytes());
        b3dm.extend_from_slice(&0u32.to_le_bytes());
        b3dm.extend_from_slice(&0u32.to_le_bytes());
        b3dm.extend_from_slice(&0u32.to_le_bytes());
        b3dm.extend_from_slice(&glb);
        let total = b3dm.len() as u32;
        b3dm[8..12].copy_from_slice(&total.to_le_bytes());
        b3dm
    }

    #[test]
    fn orphan_b3dm_is_attached_to_tileset() {
        let root = std::env::temp_dir().join(format!("chimaera-orphan-{}", std::process::id()));
        let _guard = TempDir(root.clone());
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("content")).unwrap();
        let origin = geodetic_to_ecef(116.356439218, 39.888679318, 27.0);
        let tileset = json!({
            "asset": {"version": "1.0"},
            "geometricError": 2000.0,
            "root": {
                "boundingVolume": {"region": [0.0, 0.0, 1.0, 1.0, 0.0, 100.0]},
                "geometricError": 2000.0,
                "transform": [1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, origin[0], origin[1], origin[2], 1.0],
                "children": []
            }
        });
        std::fs::write(root.join("tileset.json"), serde_json::to_vec_pretty(&tileset).unwrap()).unwrap();
        std::fs::write(
            root.join("content/4_7_3.b3dm"),
            minimal_b3dm(&[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]),
        )
        .unwrap();

        let linked = link_unreferenced_b3dm(&root).unwrap();
        assert_eq!(linked, 1);
        assert_eq!(link_unreferenced_b3dm(&root).unwrap(), 0);

        let doc: Value = serde_json::from_str(&std::fs::read_to_string(root.join("tileset.json")).unwrap()).unwrap();
        assert_eq!(
            doc.pointer("/root/children/0/content/uri").and_then(|v| v.as_str()),
            Some("content/4_7_3.b3dm")
        );
        let region: Vec<f64> = doc
            .pointer("/root/children/0/boundingVolume/region")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_f64().unwrap())
            .collect();
        let lon = 116.356439218_f64.to_radians();
        let lat = 39.888679318_f64.to_radians();
        assert!(region[0] <= lon && lon <= region[2], "{region:?}");
        assert!(region[1] <= lat && lat <= region[3], "{region:?}");
        assert!(region[4] <= 27.0 && 27.0 <= region[5], "{region:?}");
    }

    #[test]
    fn missing_intermediate_tileset_is_created_when_referenced() {
        let root = std::env::temp_dir().join(format!("chimaera-mid-{}", std::process::id()));
        let _guard = TempDir(root.clone());
        let _ = std::fs::remove_dir_all(&root);
        write_leaf(&root.join("11/01/01"), [0.0, 0.0, 1.0, 1.0, 0.0, 10.0]);
        let root_doc = json!({
            "asset": {"version": "1.0"},
            "geometricError": 1.0,
            "root": {
                "boundingVolume": {"region": [0.0, 0.0, 1.0, 1.0, 0.0, 10.0]},
                "geometricError": 1.0,
                "children": [{
                    "boundingVolume": {"region": [0.0, 0.0, 1.0, 1.0, 0.0, 10.0]},
                    "geometricError": 1.0,
                    "content": {"uri": "11/tileset.json"}
                }]
            }
        });
        std::fs::write(root.join("tileset.json"), serde_json::to_vec_pretty(&root_doc).unwrap()).unwrap();

        optimize_existing_tiles(&root).unwrap();

        assert!(root.join("11/tileset.json").is_file());
        assert!(root.join("11/01/tileset.json").is_file());
        assert_eq!(ge(&root.join("11/tileset.json"), "/geometricError"), 1000.0);
        assert_eq!(ge(&root.join("11/01/tileset.json"), "/geometricError"), 500.0);
        assert_eq!(ge(&root.join("11/01/01/tileset.json"), "/geometricError"), 250.0);
    }
}
