// Generates scripts/app-icon.png (1024x1024 RGBA) with zero dependencies:
// a rounded blue square with a white crosshair, then `tauri icon` derives all
// platform icon formats from it.
import zlib from "node:zlib";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const SIZE = 1024;

// ---------- PNG encoding ----------
const CRC_TABLE = (() => {
  const t = new Int32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    t[n] = c;
  }
  return t;
})();

function crc32(buf) {
  let c = 0xffffffff;
  for (let i = 0; i < buf.length; i++) c = CRC_TABLE[(c ^ buf[i]) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length);
  const typeBuf = Buffer.from(type, "ascii");
  const crcBuf = Buffer.alloc(4);
  crcBuf.writeUInt32BE(crc32(Buffer.concat([typeBuf, data])));
  return Buffer.concat([len, typeBuf, data, crcBuf]);
}

function encodePng(width, height, rgba) {
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // color type RGBA
  // filter byte 0 per scanline
  const raw = Buffer.alloc((width * 4 + 1) * height);
  for (let y = 0; y < height; y++) {
    raw[y * (width * 4 + 1)] = 0;
    rgba.copy(raw, y * (width * 4 + 1) + 1, y * width * 4, (y + 1) * width * 4);
  }
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", ihdr),
    chunk("IDAT", zlib.deflateSync(raw, { level: 9 })),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

// ---------- Drawing ----------
function inRoundedRect(x, y, w, h, r) {
  if (x < r && y < r) return (x - r) ** 2 + (y - r) ** 2 <= r * r;
  if (x >= w - r && y < r) return (x - (w - r)) ** 2 + (y - r) ** 2 <= r * r;
  if (x < r && y >= h - r) return (x - r) ** 2 + (y - (h - r)) ** 2 <= r * r;
  if (x >= w - r && y >= h - r)
    return (x - (w - r)) ** 2 + (y - (h - r)) ** 2 <= r * r;
  return true;
}

const buf = Buffer.alloc(SIZE * SIZE * 4);
const M = 60; // margin
const W = SIZE - M * 2;
const R = 210; // corner radius

for (let y = 0; y < SIZE; y++) {
  for (let x = 0; x < SIZE; x++) {
    const i = (y * SIZE + x) * 4;
    if (!inRoundedRect(x, y, SIZE, SIZE, R)) continue;
    // Vertical gradient #2563EB → #1747a8
    const t = y / SIZE;
    const r = Math.round(37 + (23 - 37) * t);
    const g = Math.round(99 + (71 - 99) * t);
    const b = Math.round(235 + (168 - 235) * t);
    buf[i] = r;
    buf[i + 1] = g;
    buf[i + 2] = b;
    buf[i + 3] = 255;
  }
}

// White crosshair (a capture-style plus sign with a gap in the middle).
const cx = SIZE / 2;
const cy = SIZE / 2;
const arm = 250;
const thickness = 44;
const gap = 90;

function stroke(v, x0, y0, x1, y1) {
  if (x0 !== x1) {
    const [a, b] = x0 < x1 ? [x0, x1] : [x1, x0];
    for (let x = Math.max(0, a); x <= Math.min(SIZE - 1, b); x++) {
      const i = (y0 * SIZE + x) * 4;
      v.set([255, 255, 255, 255], i);
    }
  } else {
    const [a, b] = y0 < y1 ? [y0, y1] : [y1, y0];
    for (let y = Math.max(0, a); y <= Math.min(SIZE - 1, b); y++) {
      const i = (y * SIZE + cx) * 4;
      v.set([255, 255, 255, 255], i);
    }
  }
}

function rect(x0, y0, x1, y1) {
  for (let y = y0; y < y1; y++)
    for (let x = x0; x < x1; x++) {
      const i = (y * SIZE + x) * 4;
      v.set([255, 255, 255, 255], i);
    }
}

const v = buf;
// Horizontal bar (left half + right half, skipping the center gap).
rect(M, cy - thickness / 2, cx - gap / 2, cy + thickness / 2);
rect(cx + gap / 2, cy - thickness / 2, SIZE - M, cy + thickness / 2);
// Vertical bar.
rect(cx - thickness / 2, M, cx + thickness / 2, cy - gap / 2);
rect(cx - thickness / 2, cy + gap / 2, cx + thickness / 2, SIZE - M);

const out = path.join(__dirname, "app-icon.png");
fs.writeFileSync(out, encodePng(SIZE, SIZE, buf));
console.log(`wrote ${out} (${SIZE}x${SIZE})`);
