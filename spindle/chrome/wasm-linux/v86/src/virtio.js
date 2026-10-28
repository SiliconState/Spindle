// Stand-in for v86's virtio.js (its PCI device): Spindle's wasm Linux carries 9p over
// its own transport (../../virtio.mjs), so v86's init_virtio is never called here.
export const VIRTIO_F_RING_INDIRECT_DESC = 28;
export const VIRTIO_F_RING_EVENT_IDX = 29;
export const VIRTIO_F_VERSION_1 = 32;
export function VirtIO() {
  throw new Error("v86's PCI virtio is not part of Spindle's wasm Linux");
}
