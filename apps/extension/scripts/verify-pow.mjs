// 验证 DeepSeekHashV1（SHA3-256 但 Keccak-f[1600] 跳过 round 0）与 PoW 求解流程。
// 运行：node apps/extension/scripts/verify-pow.mjs
import { createHash } from 'node:crypto';

/* ---------------- Keccak-f[1600]（BigInt 实现） ---------------- */
const RC = [
  0x0000000000000001n, 0x0000000000008082n, 0x800000000000808an, 0x8000000080008000n,
  0x000000000000808bn, 0x0000000080000001n, 0x8000000080008081n, 0x8000000000008009n,
  0x000000000000008an, 0x0000000000000088n, 0x0000000080008009n, 0x000000008000000an,
  0x000000008000808bn, 0x800000000000008bn, 0x8000000000008089n, 0x8000000000008003n,
  0x8000000000008002n, 0x8000000000000080n, 0x000000000000800an, 0x800000008000000an,
  0x8000000080008081n, 0x8000000000008080n, 0x0000000080000001n, 0x8000000080008008n,
];

const rotl = (x, n) => ((x << BigInt(n)) | (x >> BigInt(64 - n))) & 0xffffffffffffffffn;

function keccakF(state, startRound) {
  for (let round = startRound; round < 24; round++) {
    // Theta
    const c = new Array(5);
    for (let x = 0; x < 5; x++) c[x] = state[x] ^ state[x + 5] ^ state[x + 10] ^ state[x + 15] ^ state[x + 20];
    for (let x = 0; x < 5; x++) {
      const d = c[(x + 4) % 5] ^ rotl(c[(x + 1) % 5], 1);
      for (let y = 0; y < 25; y += 5) state[x + y] ^= d;
    }
    // Rho + Pi
    let last = state[1];
    let x = 1, y = 0;
    for (let t = 0; t < 24; t++) {
      const nx = y;
      const ny = (2 * x + 3 * y) % 5;
      const idx = nx + 5 * ny;
      const tmp = state[idx];
      state[idx] = rotl(last, (((t + 1) * (t + 2)) / 2) % 64);
      last = tmp;
      x = nx;
      y = ny;
    }
    // Chi
    for (let y = 0; y < 25; y += 5) {
      const a0 = state[y], a1 = state[y + 1], a2 = state[y + 2], a3 = state[y + 3], a4 = state[y + 4];
      state[y] = a0 ^ (~a1 & a2);
      state[y + 1] = a1 ^ (~a2 & a3);
      state[y + 2] = a2 ^ (~a3 & a4);
      state[y + 3] = a3 ^ (~a4 & a0);
      state[y + 4] = a4 ^ (~a0 & a1);
    }
    // Iota
    state[0] ^= RC[round];
  }
}

const RATE = 136;

function readLE64(b, off) {
  let v = 0n;
  for (let i = 7; i >= 0; i--) v = (v << 8n) | BigInt(b[off + i]);
  return v;
}

function writeLE64(v, out, off) {
  for (let i = 0; i < 8; i++) {
    out[off + i] = Number(v & 0xffn);
    v >>= 8n;
  }
}

const te = new TextEncoder();

/** 标准 SHA3-256（rounds 0..23），用于验证排列/填充正确性 */
function sha3_256(data) {
  return hashLike(data, 0);
}

/** DeepSeekHashV1 = SHA3-256 但 Keccak-f 跳过 round 0（rounds 1..23） */
function deepSeekHashV1(data) {
  return hashLike(data, 1);
}

function hashLike(data, startRound) {
  const state = new Array(25).fill(0n);
  let off = 0;
  for (; off + RATE <= data.length; off += RATE) {
    for (let i = 0; i < RATE / 8; i++) state[i] ^= readLE64(data, off + i * 8);
    keccakF(state, startRound);
  }
  const block = new Uint8Array(RATE);
  block.set(data.subarray(off));
  block[data.length - off] = 0x06;
  block[RATE - 1] |= 0x80;
  for (let i = 0; i < RATE / 8; i++) state[i] ^= readLE64(block, i * 8);
  keccakF(state, startRound);
  const out = new Uint8Array(32);
  for (let i = 0; i < 4; i++) writeLE64(state[i], out, i * 8);
  return out;
}

/* ---------------- 助手 ---------------- */
Uint8Array.prototype.hex = function () {
  return Array.from(this).map((b) => b.toString(16).padStart(2, '0')).join('');
};

/* ---------------- PoW 求解 ---------------- */
function hexToBytes(hex) {
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  return out;
}

function solvePow(challengeHex, salt, expireAt, difficulty) {
  const target = hexToBytes(challengeHex);
  const t0 = readLE64(target, 0);
  const t1 = readLE64(target, 8);
  const t2 = readLE64(target, 16);
  const t3 = readLE64(target, 24);

  const prefix = te.encode(`${salt}_${expireAt}_`);
  const state = new Array(25).fill(0n);
  let off = 0;
  for (; off + RATE <= prefix.length; off += RATE) {
    for (let i = 0; i < RATE / 8; i++) state[i] ^= readLE64(prefix, off + i * 8);
    keccakF(state, 1);
  }
  const tailBytes = prefix.subarray(off);
  const tailLen = prefix.length - off;

  const buf = new Uint8Array(RATE);
  const numBuf = new Uint8Array(20);

  for (let n = 0; n < difficulty; n++) {
    let v = n;
    let pos = 20;
    if (v === 0) {
      numBuf[--pos] = 48;
    } else {
      while (v > 0) {
        numBuf[--pos] = 48 + (v % 10);
        v = Math.floor(v / 10);
      }
    }
    const numLen = 20 - pos;
    const total = tailLen + numLen;

    const s = state.slice();
    if (total < RATE) {
      buf.fill(0);
      buf.set(tailBytes, 0);
      buf.set(numBuf.subarray(pos, pos + numLen), tailLen);
      buf[total] = 0x06;
      buf[RATE - 1] |= 0x80;
      for (let i = 0; i < RATE / 8; i++) s[i] ^= readLE64(buf, i * 8);
      keccakF(s, 1);
    } else {
      buf.fill(0);
      buf.set(tailBytes, 0);
      buf.set(numBuf.subarray(pos, pos + (RATE - tailLen)), tailLen);
      for (let i = 0; i < RATE / 8; i++) s[i] ^= readLE64(buf, i * 8);
      keccakF(s, 1);
      buf.fill(0);
      buf.set(numBuf.subarray(pos + (RATE - tailLen), 20));
      const rem = total - RATE;
      buf[rem] = 0x06;
      buf[RATE - 1] |= 0x80;
      for (let i = 0; i < RATE / 8; i++) s[i] ^= readLE64(buf, i * 8);
      keccakF(s, 1);
    }
    if (s[0] === t0 && s[1] === t1 && s[2] === t2 && s[3] === t3) return n;
  }
  return -1;
}

/* ---------------- 验证 ---------------- */
let ok = true;
function check(name, got, want) {
  const pass = got === want;
  if (!pass) ok = false;
  console.log(`${pass ? '✅' : '❌'} ${name}${pass ? '' : `\n   got : ${got}\n   want: ${want}`}`);
}

// 1) 标准 SHA3-256 测试向量（NIST）
check('SHA3-256("")', sha3_256(new Uint8Array(0)).hex(), 'a7ffc6f8bf1ed76651c14756a061d662f580ff4de43b49fa82d80a4b80f8434a');
check('SHA3-256("abc")', sha3_256(te.encode('abc')).hex(), '3a985da74fe225b2045c172d6bd390bd855f086e3e9d525b46bfe24511431532');
check('SHA3-256("hello")', sha3_256(te.encode('hello')).hex(), '3338be694f50c5f338814986cdf0686453a888b84f424d792af4b9202398f392');

// 2) 标准 SHA3-256 与 node crypto 交叉验证（随机长度）
for (const len of [1, 5, 63, 64, 135, 136, 137, 200, 1000]) {
  const data = te.encode('x'.repeat(len));
  const mine = sha3_256(data).hex();
  const node = createHash('sha3-256').update(data).digest('hex');
  check(`SHA3-256(${len} bytes) vs node`, mine, node);
}

// 3) 模拟 PoW 求解：随机 salt/expireAt/answer，构造 challenge，再反解
function makeChallengeHex(salt, expireAt, answer) {
  const input = te.encode(`${salt}_${expireAt}_${answer}`);
  return deepSeekHashV1(input).hex();
}

for (const answer of [0, 1, 42, 99999, 123456789]) {
  const salt = 'some_random_salt_value_here';
  const expireAt = 1760000000000;
  const challenge = makeChallengeHex(salt, expireAt, answer);
  const found = solvePow(challenge, salt, expireAt, answer + 1);
  check(`solvePow 找到 answer=${answer}`, found, answer);
}

// 4) 性能基准：difficulty 144000 求解耗时
{
  const salt = 'bench_salt';
  const expireAt = 1760000000000;
  const answer = 70000; // 让求解需要遍历到 ~7 万
  const challenge = makeChallengeHex(salt, expireAt, answer);
  const t0 = Date.now();
  const found = solvePow(challenge, salt, expireAt, answer + 1);
  const ms = Date.now() - t0;
  console.log(`⏱  difficulty=${answer + 1} 求解耗时 ${ms}ms（≈${Math.round((answer / ms) * 1000)} hash/s）`);
  check('性能求解正确', found, answer);
}

console.log(ok ? '\n🎉 全部通过' : '\n❌ 存在失败');
process.exit(ok ? 0 : 1);
