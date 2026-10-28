// Devices of Spindle's wasm Linux: the host half of arch/wasm's virtio transport
// (drivers/virtio_wasm.c). Each device is a device-tree node (`virtio,wasm`, its host id,
// virtio device id, features and config bytes); the kernel calls the `virtio.*` imports
// on the machine's own thread (it runs them there with run_on_main), and the device
// answers by writing the queue's used ring in the shared memory and raising the queue's
// interrupt (the kernel's `trigger_irq`). Queues are split rings (packed ones are not
// offered), laid out as the kernel's vring_create_virtqueue makes them: descriptors,
// then the driver's ring, then, at the next page (64 KiB on this kernel), the device's.

import { reaches } from './user.mjs';

export const VIRTIO_F_VERSION_1 = 1n << 32n;
const RING_ALIGN = 65536;
const DESC_NEXT = 1;
const DESC_WRITE = 2;
const DESC_INDIRECT = 4;
const AVAIL_NO_INTERRUPT = 1;

/** One request taken off a queue: what the driver gave to read, room to write, and
 * how to hand it back. It also has the shape v86's 9p server takes (`length_readable`,
 * `get_next_blob`, `set_next_blob`). */
class Request {
  constructor(queue, head, readable, writable) {
    this.queue = queue;
    this.head = head;
    this.readable = readable; // [{address, length}]
    this.writable = writable;
    this.length_readable = readable.reduce((sum, part) => sum + part.length, 0);
    this.length_writable = writable.reduce((sum, part) => sum + part.length, 0);
    this.written = 0;
  }

  /** All it gave to read, as one array (a copy). */
  read() {
    const out = new Uint8Array(this.length_readable);
    let at = 0;
    for (const { address, length } of this.readable) {
      if (!reaches(this.queue.memory, address + length)) throw new RangeError('a buffer outside memory');
      out.set(new Uint8Array(this.queue.memory.buffer, address, length), at);
      at += length;
    }
    return out;
  }

  /** Writes `bytes` after what was written so far; returns how many fit. */
  write(bytes) {
    let from = 0;
    let skip = this.written;
    for (const { address, length } of this.writable) {
      if (skip >= length) {
        skip -= length;
        continue;
      }
      const room = Math.min(length - skip, bytes.length - from);
      if (!reaches(this.queue.memory, address + skip + room)) throw new RangeError('a buffer outside memory');
      new Uint8Array(this.queue.memory.buffer, address + skip, room).set(bytes.subarray(from, from + room));
      from += room;
      skip = 0;
      if (from === bytes.length) break;
    }
    this.written += from;
    return from;
  }

  /** Back to the driver, with what was written. */
  done() {
    this.queue.complete(this.head, this.written);
  }

  get_next_blob(buffer) {
    buffer.set(this.read().subarray(0, buffer.length));
    return buffer.length;
  }

  set_next_blob(bytes) {
    return this.write(bytes);
  }
}

/** A split virtqueue the driver enabled: its rings in the kernel's memory. */
export class Queue {
  constructor(memory, raise, { size, desc, irq }) {
    this.memory = memory;
    this.raise = raise;
    this.size = size;
    this.desc = desc;
    this.avail = desc + 16 * size;
    this.used = Math.ceil((this.avail + 2 * (3 + size)) / RING_ALIGN) * RING_ALIGN;
    this.irq = irq;
    this.lastAvail = 0;
    this.pending = 0;
  }

  view() {
    reaches(this.memory, this.used + 4 + 8 * this.size + 2);
    return new DataView(this.memory.buffer);
  }

  /** The requests the driver made since the last look. */
  take() {
    const view = this.view();
    const taken = [];
    const availIdx = Atomics.load(new Uint16Array(this.memory.buffer, this.avail + 2, 1), 0);
    while (this.lastAvail !== availIdx) {
      const head = view.getUint16(this.avail + 4 + 2 * (this.lastAvail % this.size), true);
      this.lastAvail = (this.lastAvail + 1) & 0xffff;
      taken.push(this.chain(view, head));
    }
    return taken;
  }

  chain(view, head) {
    const readable = [];
    const writable = [];
    const walk = (table, index, count) => {
      for (let hops = 0; hops < count; hops++) {
        const at = table + 16 * index;
        const address = Number(view.getBigUint64(at, true));
        const length = view.getUint32(at + 8, true);
        const flags = view.getUint16(at + 12, true);
        if (flags & DESC_INDIRECT) walk(address, 0, length / 16);
        else (flags & DESC_WRITE ? writable : readable).push({ address, length });
        if (!(flags & DESC_NEXT)) return;
        index = view.getUint16(at + 14, true);
      }
    };
    walk(this.desc, head, this.size);
    return new Request(this, head, readable, writable);
  }

  /** Hands a request back (its head, the bytes written) and raises the queue's
   * interrupt, unless the driver asked for none. */
  complete(head, written) {
    const view = this.view();
    const idx = view.getUint16(this.used + 2, true);
    const at = this.used + 4 + 8 * (idx % this.size);
    view.setUint32(at, head, true);
    view.setUint32(at + 4, written, true);
    Atomics.store(new Uint16Array(this.memory.buffer, this.used + 2, 1), 0, (idx + 1) & 0xffff);
    if (!(view.getUint16(this.avail, true) & AVAIL_NO_INTERRUPT)) this.raise(this.irq);
  }
}

/** The machine's devices: their device-tree nodes and the `virtio` imports of its own
 * kernel instance. A device is {id (its virtio device id), features (bigint), config
 * (bytes), started(queues, machine), notify(index, queue), reset()}. */
export class Devices {
  constructor(devices = []) {
    this.devices = devices;
    this.memory = null;
    this.raise = () => {};
    this.state = devices.map(() => ({ queues: [], configIrq: 0, config: 0 }));
  }

  /** Connects them to the machine: its memory and how to raise an interrupt. */
  attach(memory, raise) {
    this.memory = memory;
    this.raise = raise;
  }

  /** Their device-tree nodes (children of the root). */
  nodes() {
    return Object.fromEntries(this.devices.map((device, id) => [`virtio${id}`, {
      compatible: 'virtio,wasm',
      'host-id': id,
      'virtio-device-id': device.id,
      features: device.features | VIRTIO_F_VERSION_1,
      config: device.config ?? new Uint8Array(0),
    }]));
  }

  imports() {
    const device = (id) => {
      const found = this.devices[id];
      if (!found) throw new Error(`no virtio device ${id}`);
      return found;
    };
    return {
      set_features: (id, features) => device(id).accepted?.(BigInt.asUintN(64, BigInt(features))),
      setup: (id, configIrq, config, length) => {
        Object.assign(this.state[id], { configIrq, config, length });
        device(id).setup?.(this);
      },
      reset: (id, done) => {
        this.state[id].queues = [];
        device(id).reset?.();
        Atomics.store(new Uint32Array(this.memory.buffer, done >>> 0, 1), 0, 1);
      },
      enable_vring: (id, index, size, desc, irq) => {
        const queue = new Queue(this.memory, this.raise, { size: size >>> 0, desc: Number(desc) >>> 0, irq: irq >>> 0 });
        this.state[id].queues[index] = queue;
        device(id).enabled?.(index, queue);
      },
      disable_vring: (id, index) => {
        this.state[id].queues[index] = null;
      },
      notify: (id, index) => {
        const queue = this.state[id].queues[index];
        if (queue) device(id).notify(index, queue);
      },
    };
  }
}
