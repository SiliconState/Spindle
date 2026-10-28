// A virtio console of the wasm Linux (a /dev/hvcN in it): what the guest writes goes to
// `output(bytes)`; `send(bytes)` puts bytes in for it to read, in the buffers it lends
// (held until it does).

const VIRTIO_ID_CONSOLE = 3;
const RECEIVE = 0;
const TRANSMIT = 1;

export function consoleDevice({ output = () => {} } = {}) {
  let receive = null;
  let buffers = [];
  let waiting = [];
  const flush = () => {
    if (!receive) return;
    buffers.push(...receive.take());
    while (waiting.length && buffers.length) {
      const request = buffers.shift();
      let bytes = waiting.shift();
      const fit = request.write(bytes);
      if (fit < bytes.length) waiting.unshift(bytes.subarray(fit));
      request.done();
    }
  };
  return {
    id: VIRTIO_ID_CONSOLE,
    features: 0n,
    // cols, rows, max_nr_ports, emerg_wr: none of them offered.
    config: new Uint8Array(12),
    enabled(index, queue) {
      if (index === RECEIVE) {
        receive = queue;
        flush();
      }
    },
    notify(index, queue) {
      if (index === TRANSMIT) {
        for (const request of queue.take()) {
          output(request.read());
          request.done();
        }
      } else if (index === RECEIVE) {
        flush();
      }
    },
    reset() {
      receive = null;
      buffers = [];
    },
    /** Bytes for the guest to read. */
    send(bytes) {
      waiting.push(bytes instanceof Uint8Array ? bytes : new TextEncoder().encode(bytes));
      flush();
    },
  };
}
