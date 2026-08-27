// 用真实捕获的 DeepSeek PoW 数据验证【优化版】DeepSeekHashV1（Number/32 位双字实现）。
// 真实数据（来自 chat.deepseek.com 网站真实请求头 x-ds-pow-response 解码）：
//   challenge=f68dc573e7bf4a0f46835ab406793a2a292ce7a3d85971fc1cd67c1d8e8d06be
//   salt=18bcc7ff8dfd454f8775  answer=29439  expireAt=1787752980770（已通过 BigInt 版反推）
// 若 hash(salt + "_" + expireAt + "_" + answer) == challenge 且 solvePow 反解出 answer=29439，
// 则优化版实现正确；再看性能（目标 >30 万 hash/s）。
import { createHash } from 'node:crypto';

/* ---------------- 优化版 Keccak-f[1600]（Number/32 位双字） ---------------- */
// state 为 50 个 32 位无符号整数：lane i 的 [hi, lo] = [s[2i], s[2i+1]]
const ROT_LUT = [1,3,6,10,15,21,28,36,45,55,2,14,27,41,56,8,25,43,62,18,39,61,20,44];
const RC_HI = [
  0x00000000, 0x00000000, 0x80000000, 0x80000000, 0x00000000, 0x00000000,
  0x80000000, 0x80000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000,
  0x00000000, 0x80000000, 0x80000000, 0x80000000, 0x80000000, 0x80000000,
  0x00000000, 0x80000000, 0x80000000, 0x80000000, 0x00000000, 0x80000000,
];
const RC_LO = [
  0x00000001, 0x00008082, 0x0000808a, 0x80008000, 0x0000808b, 0x80000001,
  0x80008081, 0x00008009, 0x0000008a, 0x00000088, 0x80008009, 0x8000000a,
  0x8000808b, 0x0000008b, 0x00008089, 0x00008003, 0x00008002, 0x00000080,
  0x0000800a, 0x8000000a, 0x80008081, 0x00008080, 0x80000001, 0x80008008,
];

function keccakF23(s, startRound) {
  for (let round = startRound; round < 24; round++) {
    // Theta
    const c0h = (s[0]^s[10]^s[20]^s[30]^s[40]) >>> 0, c0l = (s[1]^s[11]^s[21]^s[31]^s[41]) >>> 0;
    const c1h = (s[2]^s[12]^s[22]^s[32]^s[42]) >>> 0, c1l = (s[3]^s[13]^s[23]^s[33]^s[43]) >>> 0;
    const c2h = (s[4]^s[14]^s[24]^s[34]^s[44]) >>> 0, c2l = (s[5]^s[15]^s[25]^s[35]^s[45]) >>> 0;
    const c3h = (s[6]^s[16]^s[26]^s[36]^s[46]) >>> 0, c3l = (s[7]^s[17]^s[27]^s[37]^s[47]) >>> 0;
    const c4h = (s[8]^s[18]^s[28]^s[38]^s[48]) >>> 0, c4l = (s[9]^s[19]^s[29]^s[39]^s[49]) >>> 0;
    const d0h = (c4h ^ ((c1h << 1) | (c1l >>> 31))) >>> 0, d0l = (c4l ^ ((c1l << 1) | (c1h >>> 31))) >>> 0;
    const d1h = (c0h ^ ((c2h << 1) | (c2l >>> 31))) >>> 0, d1l = (c0l ^ ((c2l << 1) | (c2h >>> 31))) >>> 0;
    const d2h = (c1h ^ ((c3h << 1) | (c3l >>> 31))) >>> 0, d2l = (c1l ^ ((c3l << 1) | (c3h >>> 31))) >>> 0;
    const d3h = (c2h ^ ((c4h << 1) | (c4l >>> 31))) >>> 0, d3l = (c2l ^ ((c4l << 1) | (c4h >>> 31))) >>> 0;
    const d4h = (c3h ^ ((c0h << 1) | (c0l >>> 31))) >>> 0, d4l = (c3l ^ ((c0l << 1) | (c0h >>> 31))) >>> 0;
    for (let y = 0; y < 25; y += 5) {
      const i = y * 2;
      s[i] = (s[i] ^ d0h) >>> 0; s[i + 1] = (s[i + 1] ^ d0l) >>> 0;
      s[i + 2] = (s[i + 2] ^ d1h) >>> 0; s[i + 3] = (s[i + 3] ^ d1l) >>> 0;
      s[i + 4] = (s[i + 4] ^ d2h) >>> 0; s[i + 5] = (s[i + 5] ^ d2l) >>> 0;
      s[i + 6] = (s[i + 6] ^ d3h) >>> 0; s[i + 7] = (s[i + 7] ^ d3l) >>> 0;
      s[i + 8] = (s[i + 8] ^ d4h) >>> 0; s[i + 9] = (s[i + 9] ^ d4l) >>> 0;
    }
    // Rho + Pi（紧凑遍历）
    let lh = s[2], ll = s[3];
    let x = 1, y = 0;
    for (let t = 0; t < 24; t++) {
      const nx = y;
      const ny = (2 * x + 3 * y) % 5;
      const idx = (nx + 5 * ny) * 2;
      const th = s[idx], tl = s[idx + 1];
      const r = ROT_LUT[t];
      let nh, nl;
      if (r < 32) {
        nh = ((lh << r) | (ll >>> (32 - r))) >>> 0;
        nl = ((ll << r) | (lh >>> (32 - r))) >>> 0;
      } else {
        const rr = r - 32;
        nh = ((ll << rr) | (lh >>> (32 - rr))) >>> 0;
        nl = ((lh << rr) | (ll >>> (32 - rr))) >>> 0;
      }
      s[idx] = nh; s[idx + 1] = nl;
      lh = th; ll = tl;
      x = nx; y = ny;
    }
    // Chi
    for (let y = 0; y < 25; y += 5) {
      const i = y * 2;
      const a0h = s[i], a0l = s[i + 1], a1h = s[i + 2], a1l = s[i + 3];
      const a2h = s[i + 4], a2l = s[i + 5], a3h = s[i + 6], a3l = s[i + 7];
      const a4h = s[i + 8], a4l = s[i + 9];
      s[i] = (a0h ^ (~a1h & a2h)) >>> 0; s[i + 1] = (a0l ^ (~a1l & a2l)) >>> 0;
      s[i + 2] = (a1h ^ (~a2h & a3h)) >>> 0; s[i + 3] = (a1l ^ (~a2l & a3l)) >>> 0;
      s[i + 4] = (a2h ^ (~a3h & a4h)) >>> 0; s[i + 5] = (a2l ^ (~a3l & a4l)) >>> 0;
      s[i + 6] = (a3h ^ (~a4h & a0h)) >>> 0; s[i + 7] = (a3l ^ (~a4l & a0l)) >>> 0;
      s[i + 8] = (a4h ^ (~a0h & a1h)) >>> 0; s[i + 9] = (a4l ^ (~a0l & a1l)) >>> 0;
    }
    // Iota
    s[0] = (s[0] ^ RC_HI[round]) >>> 0;
    s[1] = (s[1] ^ RC_LO[round]) >>> 0;
  }
}

const RATE = 136;
const te = new TextEncoder();

function absorbBlock(state, buf, len) {
  for (let i = 0; i < len / 8; i++) {
    const off = i * 8;
    const lo = (buf[off] | (buf[off + 1] << 8) | (buf[off + 2] << 16) | (buf[off + 3] << 24)) >>> 0;
    const hi = (buf[off + 4] | (buf[off + 5] << 8) | (buf[off + 6] << 16) | (buf[off + 7] << 24)) >>> 0;
    state[2 * i] = (state[2 * i] ^ hi) >>> 0;
    state[2 * i + 1] = (state[2 * i + 1] ^ lo) >>> 0;
  }
}

Uint8Array.prototype.hex = function () { return Array.from(this).map((b) => b.toString(16).padStart(2, '0')).join(''); };

/** 标准 SHA3-256（rounds 0..23），用于交叉验证排列/填充 */
function sha3_256(data) {
  const state = new Array(50).fill(0);
  let off = 0;
  for (; off + RATE <= data.length; off += RATE) {
    absorbBlock(state, data.subarray(off), RATE);
    keccakF23(state, 0);
  }
  const block = new Uint8Array(RATE);
  block.set(data.subarray(off));
  block[data.length - off] = 0x06;
  block[RATE - 1] |= 0x80;
  absorbBlock(state, block, RATE);
  keccakF23(state, 0);
  const out = new Uint8Array(32);
  for (let i = 0; i < 4; i++) {
    let lo = state[2 * i + 1], hi = state[2 * i], o = i * 8;
    out[o] = lo & 255; out[o + 1] = (lo >>> 8) & 255; out[o + 2] = (lo >>> 16) & 255; out[o + 3] = (lo >>> 24) & 255;
    out[o + 4] = hi & 255; out[o + 5] = (hi >>> 8) & 255; out[o + 6] = (hi >>> 16) & 255; out[o + 7] = (hi >>> 24) & 255;
  }
  return out;
}

/** DeepSeekHashV1：Keccak-f 跳过 round 0（rounds 1..23） */
function deepSeekHashV1(data) {
  const state = new Array(50).fill(0);
  let off = 0;
  for (; off + RATE <= data.length; off += RATE) {
    absorbBlock(state, data.subarray(off), RATE);
    keccakF23(state, 1);
  }
  const block = new Uint8Array(RATE);
  block.set(data.subarray(off));
  block[data.length - off] = 0x06;
  block[RATE - 1] |= 0x80;
  absorbBlock(state, block, RATE);
  keccakF23(state, 1);
  const out = new Uint8Array(32);
  for (let i = 0; i < 4; i++) {
    let lo = state[2 * i + 1], hi = state[2 * i], o = i * 8;
    out[o] = lo & 255; out[o + 1] = (lo >>> 8) & 255; out[o + 2] = (lo >>> 16) & 255; out[o + 3] = (lo >>> 24) & 255;
    out[o + 4] = hi & 255; out[o + 5] = (hi >>> 8) & 255; out[o + 6] = (hi >>> 16) & 255; out[o + 7] = (hi >>> 24) & 255;
  }
  return out;
}

/* ---------------- PoW 求解（优化版） ---------------- */
function solvePow(challengeHex, salt, expireAt, difficulty) {
  const target = new Uint8Array(32);
  for (let i = 0; i < 32; i++) target[i] = parseInt(challengeHex.slice(i * 2, i * 2 + 2), 16);
  const t0h = (target[4] | target[5] << 8 | target[6] << 16 | target[7] << 24) >>> 0;
  const t0l = (target[0] | target[1] << 8 | target[2] << 16 | target[3] << 24) >>> 0;
  const t1h = (target[12] | target[13] << 8 | target[14] << 16 | target[15] << 24) >>> 0;
  const t1l = (target[8] | target[9] << 8 | target[10] << 16 | target[11] << 24) >>> 0;
  const t2h = (target[20] | target[21] << 8 | target[22] << 16 | target[23] << 24) >>> 0;
  const t2l = (target[16] | target[17] << 8 | target[18] << 16 | target[19] << 24) >>> 0;
  const t3h = (target[28] | target[29] << 8 | target[30] << 16 | target[31] << 24) >>> 0;
  const t3l = (target[24] | target[25] << 8 | target[26] << 16 | target[27] << 24) >>> 0;

  const prefix = te.encode(`${salt}_${expireAt}_`);
  const state = new Array(50).fill(0);
  let off = 0;
  for (; off + RATE <= prefix.length; off += RATE) {
    absorbBlock(state, prefix.subarray(off), RATE);
    keccakF23(state, 1);
  }
  const tailBytes = prefix.subarray(off);
  const tailLen = prefix.length - off;

  const buf = new Uint8Array(RATE);
  const numBuf = new Uint8Array(20);

  for (let n = 0; n < difficulty; n++) {
    let v = n, pos = 20;
    if (v === 0) numBuf[--pos] = 48;
    else while (v > 0) { numBuf[--pos] = 48 + (v % 10); v = Math.floor(v / 10); }
    const numLen = 20 - pos;
    const total = tailLen + numLen;

    const s = state.slice();
    if (total < RATE) {
      buf.fill(0);
      buf.set(tailBytes, 0);
      buf.set(numBuf.subarray(pos, pos + numLen), tailLen);
      buf[total] = 0x06;
      buf[RATE - 1] |= 0x80;
      absorbBlock(s, buf, RATE);
      keccakF23(s, 1);
    } else {
      buf.fill(0);
      buf.set(tailBytes, 0);
      buf.set(numBuf.subarray(pos, pos + (RATE - tailLen)), tailLen);
      absorbBlock(s, buf, RATE);
      keccakF23(s, 1);
      buf.fill(0);
      buf.set(numBuf.subarray(pos + (RATE - tailLen), 20));
      const rem = total - RATE;
      buf[rem] = 0x06;
      buf[RATE - 1] |= 0x80;
      absorbBlock(s, buf, RATE);
      keccakF23(s, 1);
    }
    if (s[0] === t0h && s[1] === t0l && s[2] === t1h && s[3] === t1l &&
        s[4] === t2h && s[5] === t2l && s[6] === t3h && s[7] === t3l) return n;
  }
  return -1;
}

let ok = true;
function check(name, got, want) { const pass = got === want; if (!pass) ok = false; console.log(`${pass ? '✅' : '❌'} ${name}${pass ? '' : `\n   got : ${got}\n   want: ${want}`}`); }

// 0) SHA3-256 交叉验证（确保排列/填充正确）
for (const [name, data, want] of [
  ['SHA3-256("")', new Uint8Array(0), 'a7ffc6f8bf1ed76651c14756a061d662f580ff4de43b49fa82d80a4b80f8434a'],
  ['SHA3-256("abc")', te.encode('abc'), '3a985da74fe225b2045c172d6bd390bd855f086e3e9d525b46bfe24511431532'],
  ['SHA3-256("hello")', te.encode('hello'), '3338be694f50c5f338814986cdf0686453a888b84f424d792af4b9202398f392'],
]) check(name, sha3_256(data).hex(), want);
for (const len of [1, 5, 63, 64, 135, 136, 137, 200, 1000]) {
  const data = te.encode('x'.repeat(len));
  check(`SHA3-256(${len} bytes) vs node`, sha3_256(data).hex(), createHash('sha3-256').update(data).digest('hex'));
}

// 1) 真实数据验证
const REAL = { salt: '18bcc7ff8dfd454f8775', answer: 29439, expireAt: 1787752980770, challenge: 'f68dc573e7bf4a0f46835ab406793a2a292ce7a3d85971fc1cd67c1d8e8d06be' };
check('真实数据 hash 匹配', deepSeekHashV1(te.encode(`${REAL.salt}_${REAL.expireAt}_${REAL.answer}`)).hex(), REAL.challenge);
const t0 = Date.now();
const ans = solvePow(REAL.challenge, REAL.salt, REAL.expireAt, 200000);
console.log(`⏱  真实数据 solvePow 反解（difficulty=200000，遍历 ${ans} 次）耗时 ${Date.now() - t0}ms`);
check('真实 solvePow 反解 answer=29439', ans, 29439);

// 2) 性能基准
for (const diff of [144000, 1000000]) {
  const salt = 'bench_salt_xyz';
  const expireAt = 1787752980770;
  const answer = Math.floor(diff * 0.7);
  const challenge = deepSeekHashV1(te.encode(`${salt}_${expireAt}_${answer}`)).hex();
  const t0b = Date.now();
  const found = solvePow(challenge, salt, expireAt, diff);
  const ms = Date.now() - t0b;
  console.log(`⏱  difficulty=${diff}（遍历 ${found} 次）求解 ${ms}ms → ≈${Math.round((found / Math.max(ms, 1)) * 1000)} hash/s`);
  check(`基准 difficulty=${diff}`, found, answer);
}

console.log(ok ? '\n🎉 全部通过' : '\n❌ 存在失败');
process.exit(ok ? 0 : 1);
