// 把 glb 做 Draco（保留顶点顺序、_BATCHID、CESIUM_primitive_outline）和 KTX2。
// 工作进程：stdin 每行 "输入.glb\t输出.glb"，stdout 回 "OK" 或 "ERR\t原因"。
import { createInterface } from 'node:readline';
import { NodeIO, Extension, ExtensionProperty, PropertyType } from '@gltf-transform/core';
import { KHRDracoMeshCompression, KHRTextureBasisu } from '@gltf-transform/extensions';
import draco3d from 'draco3dgltf';

const OUTLINE = 'CESIUM_primitive_outline';

class CesiumOutline extends ExtensionProperty {
  static EXTENSION_NAME = OUTLINE;

  init() {
    this.extensionName = OUTLINE;
    this.propertyType = 'CesiumOutline';
    this.parentTypes = [PropertyType.PRIMITIVE];
  }

  getDefaults() {
    return Object.assign(super.getDefaults(), { indices: null });
  }

  getIndices() {
    return this.getRef('indices');
  }

  setIndices(indices) {
    return this.setRef('indices', indices, { usage: 'ELEMENT_ARRAY_BUFFER' });
  }
}

class CesiumPrimitiveOutline extends Extension {
  extensionName = OUTLINE;
  static EXTENSION_NAME = OUTLINE;

  createOutline() {
    return new CesiumOutline(this.document.getGraph());
  }

  read(context) {
    const meshes = context.jsonDoc.json.meshes || [];
    meshes.forEach((meshDef, meshIndex) => {
      (meshDef.primitives || []).forEach((primDef, primIndex) => {
        const data = primDef.extensions && primDef.extensions[OUTLINE];
        if (!data || data.indices === undefined) return;
        const prim = context.meshes[meshIndex].listPrimitives()[primIndex];
        const accessor = context.accessors[data.indices];
        prim.setExtension(OUTLINE, this.createOutline().setIndices(accessor));
      });
    });
    return this;
  }

  write(context) {
    const json = context.jsonDoc.json;
    for (const mesh of this.document.getRoot().listMeshes()) {
      const meshIndex = context.meshIndexMap.get(mesh);
      if (meshIndex === undefined) continue;
      mesh.listPrimitives().forEach((prim, primIndex) => {
        const outline = prim.getExtension(OUTLINE);
        if (!outline) return;
        const accessor = outline.getIndices();
        const accessorIndex = context.accessorIndexMap.get(accessor);
        if (accessorIndex === undefined) {
          throw new Error('CESIUM_primitive_outline 的索引 accessor 没有写入');
        }
        const primDef = json.meshes[meshIndex].primitives[primIndex];
        primDef.extensions = primDef.extensions || {};
        primDef.extensions[OUTLINE] = { indices: accessorIndex };
      });
    }
    return this;
  }
}

// Draco 默认会去重顶点，轮廓索引就会对不上。顺序编码且关闭去重后，顶点下标保持不变。
function keepVertexOrder(encoderModule) {
  const proto = encoderModule.ExpertEncoder.prototype;
  if (proto.__chimaeraPatched) return;
  const orig = proto.EncodeToDracoBuffer;
  proto.EncodeToDracoBuffer = function (dedup, buffer) {
    return orig.call(this, false, buffer);
  };
  proto.__chimaeraPatched = true;
}

function snapshot(document) {
  const prims = [];
  for (const mesh of document.getRoot().listMeshes()) {
    for (const prim of mesh.listPrimitives()) {
      const batch = prim.getAttribute('_BATCHID');
      const position = prim.getAttribute('POSITION');
      const outline = prim.getExtension(OUTLINE);
      const indices = outline && outline.getIndices();
      prims.push({
        vertices: position ? position.getCount() : 0,
        batch: batch ? Array.from(batch.getArray()) : null,
        outline: indices ? Array.from(indices.getArray()) : null,
      });
    }
  }
  return prims;
}

function sameRounded(original, decoded) {
  if (original.length !== decoded.length) return false;
  for (let i = 0; i < original.length; i++) {
    if (Math.round(original[i]) !== Math.round(decoded[i])) return false;
    if (Math.abs(decoded[i] - original[i]) > 0.49) return false;
  }
  return true;
}

async function compressKtx2(document) {
  const textures = document.getRoot().listTextures();
  if (textures.length === 0) return;
  // KTX2 / Basis Universal 由 KTX-Software 的 ktx 命令完成（UASTC 给法线，ETC1S 给颜色）。
  let toktx;
  let Mode;
  let sharp;
  try {
    ({ toktx, Mode } = await import('@gltf-transform/cli'));
    sharp = (await import('sharp')).default;
  } catch (err) {
    throw new Error(`存在纹理但 KTX2 依赖未就绪（需要 ktx、sharp、@gltf-transform/cli）: ${err.message}`);
  }
  await document.transform(
    toktx({
      encoder: sharp,
      mode: Mode.UASTC,
      slots: /^(normalTexture|occlusionTexture|metallicRoughnessTexture)$/,
      level: 2,
      rdo: true,
      rdoLambda: 4,
    }),
    toktx({ encoder: sharp, mode: Mode.ETC1S, quality: 128 }),
  );
}

async function compressOne(io, input, output) {
  const document = await io.read(input);
  const before = snapshot(document);
  await compressKtx2(document);
  document
    .createExtension(KHRDracoMeshCompression)
    .setRequired(true)
    .setEncoderOptions({
      method: KHRDracoMeshCompression.EncoderMethod.SEQUENTIAL,
      encodeSpeed: 5,
      decodeSpeed: 5,
      quantizationBits: {
        POSITION: 14,
        NORMAL: 10,
        COLOR: 8,
        TEX_COORD: 12,
        GENERIC: 16,
      },
    });
  await io.write(output, document);

  const back = await io.read(output);
  const after = snapshot(back);
  if (before.length !== after.length) {
    throw new Error('压缩后 primitive 数量变化');
  }
  for (let i = 0; i < before.length; i++) {
    if (before[i].vertices !== after[i].vertices) {
      throw new Error('Draco 改变了顶点数量，轮廓索引会错位');
    }
    if (before[i].batch && !sameRounded(before[i].batch, after[i].batch || [])) {
      throw new Error('_BATCHID 经 Draco 量化后无法还原');
    }
    if (before[i].outline) {
      const a = before[i].outline;
      const b = after[i].outline;
      if (!b || a.length !== b.length || a.some((v, k) => v !== b[k])) {
        throw new Error('CESIUM_primitive_outline 索引未保持');
      }
    }
  }
}

const encoderModule = await draco3d.createEncoderModule();
const decoderModule = await draco3d.createDecoderModule();
keepVertexOrder(encoderModule);

const io = new NodeIO()
  .registerExtensions([KHRDracoMeshCompression, KHRTextureBasisu, CesiumPrimitiveOutline])
  .registerDependencies({
    'draco3d.encoder': encoderModule,
    'draco3d.decoder': decoderModule,
  });

const args = process.argv.slice(2);
if (args.length === 2) {
  await compressOne(io, args[0], args[1]);
} else {
  const rl = createInterface({ input: process.stdin });
  for await (const line of rl) {
    const tab = line.indexOf('\t');
    if (tab < 0) {
      process.stdout.write('ERR\t缺少制表符\n');
      continue;
    }
    const input = line.slice(0, tab);
    const output = line.slice(tab + 1);
    try {
      await compressOne(io, input, output);
      process.stdout.write('OK\n');
    } catch (err) {
      process.stdout.write(`ERR\t${err && err.message ? err.message : err}\n`);
    }
  }
}
