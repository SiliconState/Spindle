// The wasm Linux's entropy source (virtio-rng, its /dev/hwrng): random bytes from the
// host's crypto, so the kernel's random pool is ready at once (getrandom does not wait).

const VIRTIO_ID_RNG = 4;
const MOST = 65536;

export function entropyDevice() {
  return {
    id: VIRTIO_ID_RNG,
    features: 0n,
    config: new Uint8Array(0),
    notify(index, queue) {
      for (const request of queue.take()) {
        const bytes = new Uint8Array(Math.min(request.length_writable, MOST));
        crypto.getRandomValues(bytes);
        request.write(bytes);
        request.done();
      }
    },
  };
}
