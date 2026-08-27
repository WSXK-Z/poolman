/**
 * DeepSeek 网页版 PoW（Proof-of-Work）实现。
 *
 * DeepSeek 会要求每次聊天请求都携带 `x-ds-pow-response` 头（防机器人），
 * 其值 = base64(JSON{algorithm, challenge, salt, answer, signature, target_path})。
 *
 * 求解流程：
 *  1. POST /api/v0/chat/create_pow_challenge  body: {target_path: "/api/v0/chat/completion"}
 *  2. 返回 data.biz_data.challenge = {algorithm, challenge(hex), salt, expire_at, difficulty, signature, target_path}
 *  3. 遍历 answer ∈ [0, difficulty)，使得 DeepSeekHashV1(prefix + str(answer)) == challenge
 *     其中 prefix = salt + "_" + expire_at + "_"
 *  4. DeepSeekHashV1 = SHA3-256，但 Keccak-f[1600] 跳过 round 0（只做 rounds 1..23），
 *     rate=136，padding=0x06+0x80，输出 32 字节。
 *
 * 算法已用「真实捕获数据」验证：hash(salt_expireAt_answer) == challenge 完全一致。
 * 性能：Number/32 位双字 Keccak 实现 ≈20 万 hash/s（BigInt 版仅 ~1.3 万）。
 *
 * 参考：l-spaces/deepseek-api 的 Go 实现（pow/deepseek_hash.go、pow/deepseek_pow.go）。
 */
import { diag } from '~/background/diagnostics';

/* ------------------------------------------------------------------ */
/* DeepSeekHashV1（Keccak-f[1600] rounds 1..23，Number/32 位双字）       */
/* state 为 50 个 32 位无符号整数：lane i 的 [hi, lo] = [s[2i], s[2i+1]] */
/* ------------------------------------------------------------------ */

/** rho 旋转偏移：((t+1)(t+2)/2) % 64，t=0..23 */
const ROT_LUT = [1, 3, 6, 10, 15, 21, 28, 36, 45, 55, 2, 14, 27, 41, 56, 8, 25, 43, 62, 18, 39, 61, 20, 44];

/** Iota 轮常量（高 32 位） */
const RC_HI = [
  0x00000000, 0x00000000, 0x80000000, 0x80000000, 0x00000000, 0x00000000,
  0x80000000, 0x80000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000,
  0x00000000, 0x80000000, 0x80000000, 0x80000000, 0x80000000, 0x80000000,
  0x00000000, 0x80000000, 0x80000000, 0x80000000, 0x00000000, 0x80000000,
];

/** Iota 轮常量（低 32 位） */
const RC_LO = [
  0x00000001, 0x00008082, 0x0000808a, 0x80008000, 0x0000808b, 0x80000001,
  0x80008081, 0x00008009, 0x0000008a, 0x00000088, 0x80008009, 0x8000000a,
  0x8000808b, 0x0000008b, 0x00008089, 0x00008003, 0x00008002, 0x00000080,
  0x0000800a, 0x8000000a, 0x80008081, 0x00008080, 0x80000001, 0x80008008,
];

const RATE = 136;
const te = new TextEncoder();

/** Keccak-f[1600]。startRound=0 为标准 SHA3；DeepSeekHashV1 用 startRound=1。 */
function keccakF23(s: Uint32Array, startRound: number): void {
  for (let round = startRound; round < 24; round++) {
    // Theta
    const c0h = (s[0] ^ s[10] ^ s[20] ^ s[30] ^ s[40]) >>> 0;
    const c0l = (s[1] ^ s[11] ^ s[21] ^ s[31] ^ s[41]) >>> 0;
    const c1h = (s[2] ^ s[12] ^ s[22] ^ s[32] ^ s[42]) >>> 0;
    const c1l = (s[3] ^ s[13] ^ s[23] ^ s[33] ^ s[43]) >>> 0;
    const c2h = (s[4] ^ s[14] ^ s[24] ^ s[34] ^ s[44]) >>> 0;
    const c2l = (s[5] ^ s[15] ^ s[25] ^ s[35] ^ s[45]) >>> 0;
    const c3h = (s[6] ^ s[16] ^ s[26] ^ s[36] ^ s[46]) >>> 0;
    const c3l = (s[7] ^ s[17] ^ s[27] ^ s[37] ^ s[47]) >>> 0;
    const c4h = (s[8] ^ s[18] ^ s[28] ^ s[38] ^ s[48]) >>> 0;
    const c4l = (s[9] ^ s[19] ^ s[29] ^ s[39] ^ s[49]) >>> 0;
    const d0h = (c4h ^ ((c1h << 1) | (c1l >>> 31))) >>> 0;
    const d0l = (c4l ^ ((c1l << 1) | (c1h >>> 31))) >>> 0;
    const d1h = (c0h ^ ((c2h << 1) | (c2l >>> 31))) >>> 0;
    const d1l = (c0l ^ ((c2l << 1) | (c2h >>> 31))) >>> 0;
    const d2h = (c1h ^ ((c3h << 1) | (c3l >>> 31))) >>> 0;
    const d2l = (c1l ^ ((c3l << 1) | (c3h >>> 31))) >>> 0;
    const d3h = (c2h ^ ((c4h << 1) | (c4l >>> 31))) >>> 0;
    const d3l = (c2l ^ ((c4l << 1) | (c4h >>> 31))) >>> 0;
    const d4h = (c3h ^ ((c0h << 1) | (c0l >>> 31))) >>> 0;
    const d4l = (c3l ^ ((c0l << 1) | (c0h >>> 31))) >>> 0;
    for (let y = 0; y < 25; y += 5) {
      const i = y * 2;
      s[i] = (s[i] ^ d0h) >>> 0;
      s[i + 1] = (s[i + 1] ^ d0l) >>> 0;
      s[i + 2] = (s[i + 2] ^ d1h) >>> 0;
      s[i + 3] = (s[i + 3] ^ d1l) >>> 0;
      s[i + 4] = (s[i + 4] ^ d2h) >>> 0;
      s[i + 5] = (s[i + 5] ^ d2l) >>> 0;
      s[i + 6] = (s[i + 6] ^ d3h) >>> 0;
      s[i + 7] = (s[i + 7] ^ d3l) >>> 0;
      s[i + 8] = (s[i + 8] ^ d4h) >>> 0;
      s[i + 9] = (s[i + 9] ^ d4l) >>> 0;
    }
    // Rho + Pi（紧凑遍历）
    let lh = s[2];
    let ll = s[3];
    let x = 1;
    let y = 0;
    for (let t = 0; t < 24; t++) {
      const nx = y;
      const ny = (2 * x + 3 * y) % 5;
      const idx = (nx + 5 * ny) * 2;
      const th = s[idx];
      const tl = s[idx + 1];
      const r = ROT_LUT[t];
      let nh: number;
      let nl: number;
      if (r < 32) {
        nh = ((lh << r) | (ll >>> (32 - r))) >>> 0;
        nl = ((ll << r) | (lh >>> (32 - r))) >>> 0;
      } else {
        const rr = r - 32;
        nh = ((ll << rr) | (lh >>> (32 - rr))) >>> 0;
        nl = ((lh << rr) | (ll >>> (32 - rr))) >>> 0;
      }
      s[idx] = nh;
      s[idx + 1] = nl;
      lh = th;
      ll = tl;
      x = nx;
      y = ny;
    }
    // Chi
    for (let y = 0; y < 25; y += 5) {
      const i = y * 2;
      const a0h = s[i];
      const a0l = s[i + 1];
      const a1h = s[i + 2];
      const a1l = s[i + 3];
      const a2h = s[i + 4];
      const a2l = s[i + 5];
      const a3h = s[i + 6];
      const a3l = s[i + 7];
      const a4h = s[i + 8];
      const a4l = s[i + 9];
      s[i] = (a0h ^ (~a1h & a2h)) >>> 0;
      s[i + 1] = (a0l ^ (~a1l & a2l)) >>> 0;
      s[i + 2] = (a1h ^ (~a2h & a3h)) >>> 0;
      s[i + 3] = (a1l ^ (~a2l & a3l)) >>> 0;
      s[i + 4] = (a2h ^ (~a3h & a4h)) >>> 0;
      s[i + 5] = (a2l ^ (~a3l & a4l)) >>> 0;
      s[i + 6] = (a3h ^ (~a4h & a0h)) >>> 0;
      s[i + 7] = (a3l ^ (~a4l & a0l)) >>> 0;
      s[i + 8] = (a4h ^ (~a0h & a1h)) >>> 0;
      s[i + 9] = (a4l ^ (~a0l & a1l)) >>> 0;
    }
    // Iota
    s[0] = (s[0] ^ RC_HI[round]) >>> 0;
    s[1] = (s[1] ^ RC_LO[round]) >>> 0;
  }
}

/** 将 buf[0..len) 中的整 8 字节块异或吸收进 state（len 须为 8 的倍数） */
function absorbBlock(state: Uint32Array, buf: Uint8Array, len: number): void {
  for (let i = 0; i < len / 8; i++) {
    const off = i * 8;
    const lo = (buf[off] | (buf[off + 1] << 8) | (buf[off + 2] << 16) | (buf[off + 3] << 24)) >>> 0;
    const hi = (buf[off + 4] | (buf[off + 5] << 8) | (buf[off + 6] << 16) | (buf[off + 7] << 24)) >>> 0;
    state[2 * i] = (state[2 * i] ^ hi) >>> 0;
    state[2 * i + 1] = (state[2 * i + 1] ^ lo) >>> 0;
  }
}

/** DeepSeekHashV1：data 的 32 字节摘要（Keccak-f 跳过 round 0）。 */
export function deepSeekHashV1(data: Uint8Array): Uint8Array {
  const state = new Uint32Array(50);
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
    let lo = state[2 * i + 1];
    let hi = state[2 * i];
    const o = i * 8;
    out[o] = lo & 255;
    out[o + 1] = (lo >>> 8) & 255;
    out[o + 2] = (lo >>> 16) & 255;
    out[o + 3] = (lo >>> 24) & 255;
    out[o + 4] = hi & 255;
    out[o + 5] = (hi >>> 8) & 255;
    out[o + 6] = (hi >>> 16) & 255;
    out[o + 7] = (hi >>> 24) & 255;
  }
  return out;
}

/* ------------------------------------------------------------------ */
/* PoW 挑战与求解                                                       */
/* ------------------------------------------------------------------ */

export interface DeepSeekChallenge {
  algorithm: string;
  /** 目标哈希（64 位十六进制） */
  challenge: string;
  salt: string;
  expireAt: number;
  difficulty: number;
  signature: string;
  targetPath: string;
}

/** 遍历 answer ∈ [0, difficulty)，返回匹配的 answer；找不到返回 -1。 */
function solvePow(challengeHex: string, salt: string, expireAt: number, difficulty: number): number {
  const target = new Uint8Array(32);
  for (let i = 0; i < 32; i++) target[i] = parseInt(challengeHex.slice(i * 2, i * 2 + 2), 16);
  const t0h = (target[4] | (target[5] << 8) | (target[6] << 16) | (target[7] << 24)) >>> 0;
  const t0l = (target[0] | (target[1] << 8) | (target[2] << 16) | (target[3] << 24)) >>> 0;
  const t1h = (target[12] | (target[13] << 8) | (target[14] << 16) | (target[15] << 24)) >>> 0;
  const t1l = (target[8] | (target[9] << 8) | (target[10] << 16) | (target[11] << 24)) >>> 0;
  const t2h = (target[20] | (target[21] << 8) | (target[22] << 16) | (target[23] << 24)) >>> 0;
  const t2l = (target[16] | (target[17] << 8) | (target[18] << 16) | (target[19] << 24)) >>> 0;
  const t3h = (target[28] | (target[29] << 8) | (target[30] << 16) | (target[31] << 24)) >>> 0;
  const t3l = (target[24] | (target[25] << 8) | (target[26] << 16) | (target[27] << 24)) >>> 0;

  // prefix 预吸收进 state（循环内零分配优化）
  const prefix = te.encode(`${salt}_${expireAt}_`);
  const state = new Uint32Array(50);
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
      absorbBlock(s, buf, RATE);
      keccakF23(s, 1);
    } else {
      // 两块（tail + 数字超出 136 字节时才发生，数字最多 20 字节）
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

    if (
      s[0] === t0h && s[1] === t0l && s[2] === t1h && s[3] === t1l &&
      s[4] === t2h && s[5] === t2l && s[6] === t3h && s[7] === t3l
    ) {
      return n;
    }
  }
  return -1;
}

const POW_CHALLENGE_URL = 'https://chat.deepseek.com/api/v0/chat/create_pow_challenge';

/** 请求并解析 PoW 挑战（响应路径 data.biz_data.challenge）。 */
export async function fetchPowChallenge(
  token: string,
  settingsToken?: string,
): Promise<DeepSeekChallenge> {
  const headers: Record<string, string> = {
    'x-client-bundle-id': 'com.deepseek.chat',
    'x-client-platform': 'web',
    'x-client-version': '2.4.0',
    'x-client-locale': 'zh_CN',
    'x-client-timezone-offset': '28800',
    'Content-Type': 'application/json',
    Authorization: `Bearer ${token}`,
  };
  if (settingsToken) headers['x-settings-token'] = settingsToken;

  const res = await fetch(POW_CHALLENGE_URL, {
    method: 'POST',
    headers,
    body: JSON.stringify({ target_path: '/api/v0/chat/completion' }),
  });
  const text = await res.text();
  if (!res.ok) {
    throw new Error(`create_pow_challenge HTTP ${res.status}: ${text.slice(0, 300)}`);
  }
  let json: any;
  try {
    json = JSON.parse(text);
  } catch {
    throw new Error(`create_pow_challenge 返回非 JSON: ${text.slice(0, 300)}`);
  }
  // 兼容多种响应包裹
  const ch = json?.data?.biz_data?.challenge ?? json?.data?.challenge ?? json?.challenge;
  if (!ch) {
    throw new Error(`create_pow_challenge 未返回 challenge: ${text.slice(0, 300)}`);
  }
  const difficulty = Number(ch.difficulty ?? 144000) || 144000;
  return {
    algorithm: String(ch.algorithm ?? 'DeepSeekHashV1'),
    challenge: String(ch.challenge ?? ''),
    salt: String(ch.salt ?? ''),
    expireAt: Number(ch.expire_at ?? ch.expireAt ?? ch.expires_at ?? 0),
    difficulty,
    signature: String(ch.signature ?? ''),
    targetPath: String(ch.target_path ?? '/api/v0/chat/completion'),
  };
}

/** 序列化 PoW 应答头：base64(JSON{...})。不含 difficulty/expire_at。 */
export function buildPowHeader(ch: DeepSeekChallenge, answer: number): string {
  const obj = {
    algorithm: ch.algorithm,
    challenge: ch.challenge,
    salt: ch.salt,
    answer,
    signature: ch.signature,
    target_path: ch.targetPath,
  };
  // background（service worker）环境有 btoa；JSON 全 ASCII，安全。
  return btoa(JSON.stringify(obj));
}

/** 端到端：拉取挑战 → 求解 → 组装 x-ds-pow-response 头；失败返回 null 并上报诊断。 */
export async function solvePowAndBuildHeader(
  token: string,
  settingsToken?: string,
): Promise<string | null> {
  try {
    const ch = await fetchPowChallenge(token, settingsToken);
    diag('info', 'deepseek', '获取 PoW 挑战', {
      algorithm: ch.algorithm,
      difficulty: ch.difficulty,
      expireAt: ch.expireAt,
      saltLen: ch.salt.length,
      challengeLen: ch.challenge.length,
    });
    const start = Date.now();
    const answer = solvePow(ch.challenge, ch.salt, ch.expireAt, ch.difficulty);
    const elapsedMs = Date.now() - start;
    if (answer < 0) {
      diag('error', 'deepseek', 'PoW 求解失败（difficulty 范围内未找到答案）', {
        difficulty: ch.difficulty,
        elapsedMs,
      });
      return null;
    }
    diag('info', 'deepseek', `PoW 求解成功 answer=${answer}`, {
      elapsedMs,
      difficulty: ch.difficulty,
    });
    return buildPowHeader(ch, answer);
  } catch (err) {
    diag('error', 'deepseek', 'PoW 流程失败', { error: String(err) });
    return null;
  }
}
