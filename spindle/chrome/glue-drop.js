
/** Lets go of this module's instance and the views of its memory (Spindle for Browser:
 * a fresh copy of this module takes over, engine-worker.mjs `renewEngine`). Nothing
 * in this module is used after. Appended to wasm-bindgen's web glue by chrome/build.sh. */
export function __spindleDrop() {
    wasm = undefined;
    wasmInstance = undefined;
    wasmModule = undefined;
    cachedDataViewMemory0 = null;
    cachedUint8ArrayMemory0 = null;
}
