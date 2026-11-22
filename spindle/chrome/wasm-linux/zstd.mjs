// zstd for the wasm Linux: its files and kernel are compressed with zstd, as the browser
// Linux's are, and decompressed with the same decoder v86 uses for those. v86.wasm carries
// zstd; it is instantiated here on its own, with nothing of the emulator running (as v86
// does for its own zstd worker), so the wasm Linux needs no other decoder.

/** A decoder: `(size, bytes) => ArrayBuffer`, `size` being what the bytes decompress to
 * (v86's filesystem storage calls it so). `v86wasm`: v86.wasm's bytes or its module. */
export async function zstdDecoder(v86wasm) {
  const module = v86wasm instanceof WebAssembly.Module ? v86wasm : await WebAssembly.compile(v86wasm);
  const env = {};
  for (const { name, kind } of WebAssembly.Module.imports(module)) {
    if (kind === 'function') env[name] = () => { throw new Error(`zstd: v86's ${name} was called`); };
  }
  env.__indirect_function_table = new WebAssembly.Table({ element: 'anyfunc', initial: 1024 });
  const { exports } = await WebAssembly.instantiate(module, { env });
  return (size, bytes) => {
    const context = exports.zstd_create_ctx(bytes.length);
    try {
      new Uint8Array(exports.memory.buffer).set(bytes, exports.zstd_get_src_ptr(context) >>> 0);
      const at = exports.zstd_read(context, size) >>> 0;
      const out = exports.memory.buffer.slice(at, at + size);
      exports.zstd_read_free(at, size);
      return out;
    } finally {
      exports.zstd_free_ctx(context);
    }
  };
}

/** The size a zstd frame says it decompresses to (its Frame_Content_Size), or null when
 * it does not say. */
export function contentSize(bytes) {
  if (bytes.length < 6 || bytes[0] !== 0x28 || bytes[1] !== 0xb5 || bytes[2] !== 0x2f || bytes[3] !== 0xfd) return null;
  const descriptor = bytes[4];
  const single = (descriptor >> 5) & 1;
  const fieldSize = [single ? 1 : 0, 2, 4, 8][descriptor >> 6];
  if (!fieldSize) return null;
  const at = 5 + (single ? 0 : 1) + [0, 1, 2, 4][descriptor & 3];
  if (bytes.length < at + fieldSize) return null;
  let size = 0;
  for (let i = fieldSize - 1; i >= 0; i--) size = size * 256 + bytes[at + i];
  return fieldSize === 2 ? size + 256 : size;
}
