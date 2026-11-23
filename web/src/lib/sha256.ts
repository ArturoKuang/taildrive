// FIPS 180-4 SHA-256 with Web Crypto fast-path and pure-JS incremental fallback
// (handles non-HTTPS origins like http://100.x.y.z:8787 where crypto.subtle is unavailable,
// as well as streaming large files without loading them fully into RAM).

const K = new Uint32Array([
  0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
  0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
  0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
  0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
  0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
  0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
  0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
  0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
]);

export class Sha256 {
  private h0 = 0x6a09e667;
  private h1 = 0xbb67ae85;
  private h2 = 0x3c6ef372;
  private h3 = 0xa54ff53a;
  private h4 = 0x510e527f;
  private h5 = 0x9b05688c;
  private h6 = 0x1f83d9ab;
  private h7 = 0x5be0cd19;

  private readonly block = new Uint8Array(64);
  private readonly w = new Uint32Array(64);
  private blockLen = 0;
  private totalBytes = 0;
  private finalized = false;

  update(data: Uint8Array): this {
    if (this.finalized) {
      throw new Error('Sha256 instance already finalized');
    }
    let offset = 0;
    const len = data.length;
    this.totalBytes += len;

    if (this.blockLen > 0) {
      const need = 64 - this.blockLen;
      if (len < need) {
        this.block.set(data, this.blockLen);
        this.blockLen += len;
        return this;
      }
      this.block.set(data.subarray(0, need), this.blockLen);
      this.compress(this.block, 0);
      this.blockLen = 0;
      offset = need;
    }

    while (offset + 64 <= len) {
      this.compress(data, offset);
      offset += 64;
    }

    if (offset < len) {
      const remaining = len - offset;
      this.block.set(data.subarray(offset), 0);
      this.blockLen = remaining;
    }

    return this;
  }

  private compress(buf: Uint8Array, offset: number): void {
    const w = this.w;
    for (let i = 0; i < 16; i++) {
      const j = offset + i * 4;
      w[i] = ((buf[j] << 24) | (buf[j + 1] << 16) | (buf[j + 2] << 8) | buf[j + 3]) >>> 0;
    }
    for (let i = 16; i < 64; i++) {
      const v15 = w[i - 15];
      const s0 = ((v15 >>> 7) | (v15 << 25)) ^ ((v15 >>> 18) | (v15 << 14)) ^ (v15 >>> 3);
      const v2 = w[i - 2];
      const s1 = ((v2 >>> 17) | (v2 << 15)) ^ ((v2 >>> 19) | (v2 << 13)) ^ (v2 >>> 10);
      w[i] = (w[i - 16] + s0 + w[i - 7] + s1) >>> 0;
    }

    let a = this.h0;
    let b = this.h1;
    let c = this.h2;
    let d = this.h3;
    let e = this.h4;
    let f = this.h5;
    let g = this.h6;
    let h = this.h7;

    for (let i = 0; i < 64; i++) {
      const S1 = ((e >>> 6) | (e << 26)) ^ ((e >>> 11) | (e << 21)) ^ ((e >>> 25) | (e << 7));
      const ch = (e & f) ^ (~e & g);
      const temp1 = (h + S1 + ch + K[i] + w[i]) >>> 0;
      const S0 = ((a >>> 2) | (a << 30)) ^ ((a >>> 13) | (a << 19)) ^ ((a >>> 22) | (a << 10));
      const maj = (a & b) ^ (a & c) ^ (b & c);
      const temp2 = (S0 + maj) >>> 0;

      h = g;
      g = f;
      f = e;
      e = (d + temp1) >>> 0;
      d = c;
      c = b;
      b = a;
      a = (temp1 + temp2) >>> 0;
    }

    this.h0 = (this.h0 + a) >>> 0;
    this.h1 = (this.h1 + b) >>> 0;
    this.h2 = (this.h2 + c) >>> 0;
    this.h3 = (this.h3 + d) >>> 0;
    this.h4 = (this.h4 + e) >>> 0;
    this.h5 = (this.h5 + f) >>> 0;
    this.h6 = (this.h6 + g) >>> 0;
    this.h7 = (this.h7 + h) >>> 0;
  }

  digestHex(): string {
    if (!this.finalized) {
      this.finalized = true;
      const bitLenHigh = Math.floor(this.totalBytes / 0x20000000) >>> 0;
      const bitLenLow = (this.totalBytes * 8) >>> 0;

      this.block[this.blockLen++] = 0x80;
      if (this.blockLen > 56) {
        this.block.fill(0, this.blockLen, 64);
        this.compress(this.block, 0);
        this.blockLen = 0;
      }
      this.block.fill(0, this.blockLen, 56);

      const view = new DataView(this.block.buffer);
      view.setUint32(56, bitLenHigh, false);
      view.setUint32(60, bitLenLow, false);
      this.compress(this.block, 0);
    }

    return [this.h0, this.h1, this.h2, this.h3, this.h4, this.h5, this.h6, this.h7]
      .map((word) => word.toString(16).padStart(8, '0'))
      .join('');
  }
}

function bufferToHex(buf: ArrayBuffer): string {
  const bytes = new Uint8Array(buf);
  let out = '';
  for (let i = 0; i < bytes.length; i++) {
    out += bytes[i].toString(16).padStart(2, '0');
  }
  return out;
}

export function hasSubtleCrypto(): boolean {
  return (
    typeof globalThis !== 'undefined' &&
    typeof globalThis.crypto !== 'undefined' &&
    typeof globalThis.crypto.subtle !== 'undefined' &&
    typeof globalThis.crypto.subtle.digest === 'function'
  );
}

export async function sha256Buffer(buffer: ArrayBuffer): Promise<string> {
  if (hasSubtleCrypto()) {
    try {
      const digest = await globalThis.crypto.subtle.digest('SHA-256', buffer);
      return bufferToHex(digest);
    } catch {
      // Fall through to pure-JS implementation if SubtleCrypto throws in restricted contexts
    }
  }
  return new Sha256().update(new Uint8Array(buffer)).digestHex();
}

export async function sha256Blob(blob: Blob): Promise<string> {
  const buffer = await blob.arrayBuffer();
  return sha256Buffer(buffer);
}

const HASH_CHUNK_BYTES = 2 * 1024 * 1024;
const SUBTLE_FAST_PATH_LIMIT = 8 * 1024 * 1024;

export async function hashFile(
  file: File,
  onProgress?: (ratio: number) => void,
  signal?: AbortSignal,
): Promise<string> {
  if (signal?.aborted) {
    throw new DOMException('Hashing aborted', 'AbortError');
  }

  if (file.size <= SUBTLE_FAST_PATH_LIMIT && hasSubtleCrypto()) {
    onProgress?.(0.15);
    const buffer = await file.arrayBuffer();
    if (signal?.aborted) {
      throw new DOMException('Hashing aborted', 'AbortError');
    }
    const hex = await sha256Buffer(buffer);
    onProgress?.(1);
    return hex;
  }

  const hasher = new Sha256();
  if (file.size === 0) {
    onProgress?.(1);
    return hasher.digestHex();
  }

  let offset = 0;
  while (offset < file.size) {
    if (signal?.aborted) {
      throw new DOMException('Hashing aborted', 'AbortError');
    }
    const end = Math.min(offset + HASH_CHUNK_BYTES, file.size);
    const chunk = await file.slice(offset, end).arrayBuffer();
    hasher.update(new Uint8Array(chunk));
    offset = end;
    onProgress?.(offset / file.size);
    // Yield to keep UI responsive during large file hashing
    await new Promise((resolve) => setTimeout(resolve, 0));
  }

  return hasher.digestHex();
}
