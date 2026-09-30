// A reader of the Cap'n Proto wire format, with only what scene.ts needs: a
// message of one or more segments, structs, lists of structs, of bytes and
// of Float32, and text. It reads the message in place. A pointer that leaves
// its segment, an element size that does not match the field, or a walk
// longer than the message is damage, and throws.

// A struct in the message. A field past the data section reads as zero and
// a pointer past the pointer section reads as null, as for a struct of an
// older schema.
export interface Struct {
  seg: number;
  // Byte offsets in the message.
  data: number;
  dataBytes: number;
  ptrs: number;
  ptrCount: number;
}

// A list of structs, each `stride` bytes.
export interface StructList {
  seg: number;
  start: number;
  length: number;
  stride: number;
  dataBytes: number;
  ptrCount: number;
}

const EMPTY_STRUCT: Struct = {
  seg: 0,
  data: 0,
  dataBytes: 0,
  ptrs: 0,
  ptrCount: 0,
};
const EMPTY_LIST: StructList = {
  seg: 0,
  start: 0,
  length: 0,
  stride: 0,
  dataBytes: 0,
  ptrCount: 0,
};

// The element sizes of a list pointer.
const BYTE = 2;
const FOUR_BYTES = 4;
const COMPOSITE = 7;

const utf8 = new TextDecoder("utf-8", { fatal: true });
const scratch = new DataView(new ArrayBuffer(4));

export class Reader {
  #bytes: Uint8Array;
  #view: DataView;
  // The byte offset of the start and of the end of each segment.
  #starts: number[] = [];
  #ends: number[] = [];
  // The words that the walk may still read, the size of the message, so a
  // message whose pointers share their targets cannot make the walk longer
  // than the message.
  #budget: number;

  constructor(bytes: Uint8Array) {
    // A Float32Array view of a list needs the message at a multiple of 4.
    if (bytes.byteOffset % 4 !== 0) bytes = bytes.slice();
    this.#bytes = bytes;
    this.#view = new DataView(bytes.buffer, bytes.byteOffset, bytes.length);
    if (bytes.length < 8) throw damage("a message shorter than its header");
    const count = this.#view.getUint32(0, true) + 1;
    let at = 8 * Math.ceil((4 + 4 * count) / 8);
    if (at > bytes.length) throw damage("a segment table past the message");
    for (let i = 0; i < count; i++) {
      const size = this.#view.getUint32(4 + 4 * i, true) * 8;
      this.#starts.push(at);
      at += size;
      this.#ends.push(at);
    }
    if (at > bytes.length) throw damage("a segment past the message");
    this.#budget = bytes.length / 8;
  }

  root(): Struct {
    this.#within(0, this.#starts[0], 8);
    return this.#struct(0, this.#starts[0]);
  }

  u8(s: Struct, offset: number): number {
    return offset < s.dataBytes ? this.#bytes[s.data + offset] : 0;
  }

  u16(s: Struct, offset: number): number {
    return offset + 2 <= s.dataBytes
      ? this.#view.getUint16(s.data + offset, true)
      : 0;
  }

  u32(s: Struct, offset: number): number {
    return offset + 4 <= s.dataBytes
      ? this.#view.getUint32(s.data + offset, true)
      : 0;
  }

  // The Float32 at `offset`, stored XOR the bits of its default.
  f32(s: Struct, offset: number, defaultBits = 0): number {
    if (defaultBits === 0) {
      return offset + 4 <= s.dataBytes
        ? this.#view.getFloat32(s.data + offset, true)
        : 0;
    }
    scratch.setUint32(0, this.u32(s, offset) ^ defaultBits, true);
    return scratch.getFloat32(0, true);
  }

  bit(s: Struct, offset: number): boolean {
    const byte = offset >>> 3;
    return byte < s.dataBytes &&
      (this.#bytes[s.data + byte] & (1 << (offset & 7))) !== 0;
  }

  // The struct of pointer `index`, or an empty one for null.
  struct(s: Struct, index: number): Struct {
    if (index >= s.ptrCount) return EMPTY_STRUCT;
    return this.#struct(s.seg, s.ptrs + 8 * index);
  }

  structs(s: Struct, index: number): StructList {
    const t = this.#target(s, index);
    if (t === null) return EMPTY_LIST;
    if (t.size !== COMPOSITE) {
      throw damage("a list of structs is not composite");
    }
    const tag = t.at;
    if (tag + 8 > this.#ends[t.seg] || (this.#lo(tag) & 3) !== 0) {
      throw damage("a composite list with a bad tag");
    }
    const length = this.#lo(tag) >>> 2;
    const dataWords = this.#view.getUint16(tag + 4, true);
    const ptrCount = this.#view.getUint16(tag + 6, true);
    const stride = 8 * (dataWords + ptrCount);
    const start = tag + 8;
    if (length * stride > 8 * t.count) {
      throw damage("a composite list longer than its words");
    }
    this.#spend(Math.max(t.count, length));
    this.#within(t.seg, start, 8 * t.count);
    return {
      seg: t.seg,
      start,
      length,
      stride,
      dataBytes: 8 * dataWords,
      ptrCount,
    };
  }

  at(list: StructList, i: number): Struct {
    const data = list.start + i * list.stride;
    return {
      seg: list.seg,
      data,
      dataBytes: list.dataBytes,
      ptrs: data + list.dataBytes,
      ptrCount: list.ptrCount,
    };
  }

  // The bytes of a Data, a view into the message.
  data(s: Struct, index: number): Uint8Array {
    const t = this.#target(s, index);
    if (t === null) return new Uint8Array(0);
    if (t.size !== BYTE) throw damage("a Data is not a list of bytes");
    this.#spend(Math.ceil(t.count / 8));
    this.#within(t.seg, t.at, t.count);
    return this.#bytes.subarray(t.at, t.at + t.count);
  }

  // A List(Float32), a view into the message. The wire is little-endian, as
  // every browser.
  float32s(s: Struct, index: number): Float32Array {
    const t = this.#target(s, index);
    if (t === null) return new Float32Array(0);
    if (t.size !== FOUR_BYTES) throw damage("a List(Float32) of another size");
    this.#spend(Math.ceil(t.count / 2));
    this.#within(t.seg, t.at, 4 * t.count);
    return new Float32Array(
      this.#bytes.buffer,
      this.#bytes.byteOffset + t.at,
      t.count,
    );
  }

  // A Text, which has to end in NUL and to be UTF-8.
  text(s: Struct, index: number): string {
    const bytes = this.data(s, index);
    if (bytes.length === 0) return "";
    if (bytes[bytes.length - 1] !== 0) throw damage("a Text with no NUL");
    try {
      return utf8.decode(bytes.subarray(0, bytes.length - 1));
    } catch {
      throw damage("a Text that is not UTF-8");
    }
  }

  // The struct of the pointer at byte `at` of segment `seg`.
  #struct(seg: number, at: number): Struct {
    const p = this.#resolve(seg, at);
    if (p === null) return EMPTY_STRUCT;
    if ((p.lo & 3) !== 0) throw damage("a struct field holds another pointer");
    const dataBytes = 8 * this.#view.getUint16(p.hiAt, true);
    const ptrCount = this.#view.getUint16(p.hiAt + 2, true);
    this.#spend(Math.max(dataBytes / 8 + ptrCount, 1));
    this.#within(p.seg, p.at, dataBytes + 8 * ptrCount);
    return {
      seg: p.seg,
      data: p.at,
      dataBytes,
      ptrs: p.at + dataBytes,
      ptrCount,
    };
  }

  // The list of pointer `index` of `s`, as its element size, its count of
  // elements, or of words for a composite list, and the byte of its start,
  // or `null` for null.
  #target(s: Struct, index: number) {
    if (index >= s.ptrCount) return null;
    const p = this.#resolve(s.seg, s.ptrs + 8 * index);
    if (p === null) return null;
    if ((p.lo & 3) !== 1) throw damage("a list field holds another pointer");
    const hi = this.#view.getUint32(p.hiAt, true);
    return { seg: p.seg, at: p.at, size: hi & 7, count: hi >>> 3 };
  }

  // Follows the pointer at byte `at` of `seg` through its far pointers, and
  // returns the segment and the byte of its target, the low word of the
  // pointer that describes the target and the byte of its high word. `null`
  // for a null pointer.
  #resolve(seg: number, at: number) {
    const lo = this.#lo(at);
    const hiAt = at + 4;
    if (lo === 0 && this.#view.getUint32(hiAt, true) === 0) return null;
    switch (lo & 3) {
      case 0:
      case 1:
        return { seg, at: at + 8 + 8 * (lo >> 2), lo, hiAt };
      case 2: {
        const target = this.#view.getUint32(hiAt, true);
        if (target >= this.#starts.length) {
          throw damage("a far pointer to no segment");
        }
        const pad = this.#starts[target] + 8 * (lo >>> 3);
        const double = (lo & 4) !== 0;
        this.#within(target, pad, double ? 16 : 8);
        if (!double) {
          const inner = this.#lo(pad);
          if ((inner & 3) === 2) throw damage("a landing pad that is far");
          return {
            seg: target,
            at: pad + 8 + 8 * (inner >> 2),
            lo: inner,
            hiAt: pad + 4,
          };
        }
        // The pad is a far pointer to the start of the target and a tag
        // that describes it.
        const far = this.#lo(pad);
        const tagLo = this.#lo(pad + 8);
        if ((far & 7) !== 2 || (tagLo & 3) === 2 || tagLo >> 2 !== 0) {
          throw damage("a bad double far landing pad");
        }
        const next = this.#view.getUint32(pad + 4, true);
        if (next >= this.#starts.length) {
          throw damage("a far pointer to no segment");
        }
        return {
          seg: next,
          at: this.#starts[next] + 8 * (far >>> 3),
          lo: tagLo,
          hiAt: pad + 12,
        };
      }
      default:
        throw damage("a capability in a scene");
    }
  }

  #lo(at: number): number {
    return this.#view.getUint32(at, true);
  }

  #within(seg: number, at: number, bytes: number): void {
    if (at < this.#starts[seg] || at + bytes > this.#ends[seg]) {
      throw damage("a pointer out of its segment");
    }
  }

  #spend(words: number): void {
    this.#budget -= words;
    if (this.#budget < 0) throw damage("a walk longer than the message");
  }
}

function damage(what: string): Error {
  return new Error(`damaged message: ${what}`);
}
