//! DeepSeek 网页版 PoW（Proof-of-Work）—— 纯计算模块。
//! 移植自 apps/server/src/providers/deepseek-pow.ts（该模块源自浏览器插件）。
//!
//! DeepSeek 每次聊天请求都要求携带 `x-ds-pow-response` 头，其值 =
//! base64(JSON{algorithm, challenge, salt, answer, signature, target_path})。
//!
//! 求解：遍历 answer ∈ [0, difficulty)，使 DeepSeekHashV1(prefix + str(answer)) == challenge，
//! 其中 prefix = salt + "_" + expire_at + "_"。DeepSeekHashV1 = SHA3-256 但 Keccak-f[1600]
//! 跳过 round 0（只做 rounds 1..23），rate=136，padding=0x06+0x80，输出 32 字节。
//!
//! state 为 50 个 u32（lane i 的 [hi, lo] = [s[2i], s[2i+1]]），保持与 TS 版一致的 32 位双字实现。

/// rho 旋转偏移：((t+1)(t+2)/2) % 64，t=0..23
const ROT_LUT: [u32; 24] = [
    1, 3, 6, 10, 15, 21, 28, 36, 45, 55, 2, 14, 27, 41, 56, 8, 25, 43, 62, 18, 39, 61, 20, 44,
];

/// Iota 轮常量（高 32 位）
const RC_HI: [u32; 24] = [
    0x00000000, 0x00000000, 0x80000000, 0x80000000, 0x00000000, 0x00000000, 0x80000000, 0x80000000,
    0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x80000000, 0x80000000, 0x80000000,
    0x80000000, 0x80000000, 0x00000000, 0x80000000, 0x80000000, 0x80000000, 0x00000000, 0x80000000,
];

/// Iota 轮常量（低 32 位）
const RC_LO: [u32; 24] = [
    0x00000001, 0x00008082, 0x0000808a, 0x80008000, 0x0000808b, 0x80000001, 0x80008081, 0x00008009,
    0x0000008a, 0x00000088, 0x80008009, 0x8000000a, 0x8000808b, 0x0000008b, 0x00008089, 0x00008003,
    0x00008002, 0x00000080, 0x0000800a, 0x8000000a, 0x80008081, 0x00008080, 0x80000001, 0x80008008,
];

const RATE: usize = 136;

/// Keccak-f[1600]。start_round=0 为标准 SHA3；DeepSeekHashV1 用 1（跳过 round 0）。
fn keccak_f23(s: &mut [u32; 50], start_round: usize) {
    for round in start_round..24 {
        // Theta
        let c0h = s[0] ^ s[10] ^ s[20] ^ s[30] ^ s[40];
        let c0l = s[1] ^ s[11] ^ s[21] ^ s[31] ^ s[41];
        let c1h = s[2] ^ s[12] ^ s[22] ^ s[32] ^ s[42];
        let c1l = s[3] ^ s[13] ^ s[23] ^ s[33] ^ s[43];
        let c2h = s[4] ^ s[14] ^ s[24] ^ s[34] ^ s[44];
        let c2l = s[5] ^ s[15] ^ s[25] ^ s[35] ^ s[45];
        let c3h = s[6] ^ s[16] ^ s[26] ^ s[36] ^ s[46];
        let c3l = s[7] ^ s[17] ^ s[27] ^ s[37] ^ s[47];
        let c4h = s[8] ^ s[18] ^ s[28] ^ s[38] ^ s[48];
        let c4l = s[9] ^ s[19] ^ s[29] ^ s[39] ^ s[49];
        let d0h = c4h ^ ((c1h << 1) | (c1l >> 31));
        let d0l = c4l ^ ((c1l << 1) | (c1h >> 31));
        let d1h = c0h ^ ((c2h << 1) | (c2l >> 31));
        let d1l = c0l ^ ((c2l << 1) | (c2h >> 31));
        let d2h = c1h ^ ((c3h << 1) | (c3l >> 31));
        let d2l = c1l ^ ((c3l << 1) | (c3h >> 31));
        let d3h = c2h ^ ((c4h << 1) | (c4l >> 31));
        let d3l = c2l ^ ((c4l << 1) | (c4h >> 31));
        let d4h = c3h ^ ((c0h << 1) | (c0l >> 31));
        let d4l = c3l ^ ((c0l << 1) | (c0h >> 31));
        let mut y = 0;
        while y < 25 {
            let i = y * 2;
            s[i] ^= d0h;
            s[i + 1] ^= d0l;
            s[i + 2] ^= d1h;
            s[i + 3] ^= d1l;
            s[i + 4] ^= d2h;
            s[i + 5] ^= d2l;
            s[i + 6] ^= d3h;
            s[i + 7] ^= d3l;
            s[i + 8] ^= d4h;
            s[i + 9] ^= d4l;
            y += 5;
        }
        // Rho + Pi（紧凑遍历）
        let mut lh = s[2];
        let mut ll = s[3];
        let mut x = 1usize;
        let mut y = 0usize;
        for t in 0..24 {
            let nx = y;
            let ny = (2 * x + 3 * y) % 5;
            let idx = (nx + 5 * ny) * 2;
            let th = s[idx];
            let tl = s[idx + 1];
            let r = ROT_LUT[t];
            let (nh, nl) = if r < 32 {
                (((lh << r) | (ll >> (32 - r))), ((ll << r) | (lh >> (32 - r))))
            } else {
                let rr = r - 32;
                (((ll << rr) | (lh >> (32 - rr))), ((lh << rr) | (ll >> (32 - rr))))
            };
            s[idx] = nh;
            s[idx + 1] = nl;
            lh = th;
            ll = tl;
            x = nx;
            y = ny;
        }
        // Chi
        let mut y = 0;
        while y < 25 {
            let i = y * 2;
            let a0h = s[i];
            let a0l = s[i + 1];
            let a1h = s[i + 2];
            let a1l = s[i + 3];
            let a2h = s[i + 4];
            let a2l = s[i + 5];
            let a3h = s[i + 6];
            let a3l = s[i + 7];
            let a4h = s[i + 8];
            let a4l = s[i + 9];
            s[i] = a0h ^ (!a1h & a2h);
            s[i + 1] = a0l ^ (!a1l & a2l);
            s[i + 2] = a1h ^ (!a2h & a3h);
            s[i + 3] = a1l ^ (!a2l & a3l);
            s[i + 4] = a2h ^ (!a3h & a4h);
            s[i + 5] = a2l ^ (!a3l & a4l);
            s[i + 6] = a3h ^ (!a4h & a0h);
            s[i + 7] = a3l ^ (!a4l & a0l);
            s[i + 8] = a4h ^ (!a0h & a1h);
            s[i + 9] = a4l ^ (!a0l & a1l);
            y += 5;
        }
        // Iota
        s[0] ^= RC_HI[round];
        s[1] ^= RC_LO[round];
    }
}

/// 将 buf[0..len) 中的整 8 字节块异或吸收进 state（len 须为 8 的倍数）
fn absorb_block(state: &mut [u32; 50], buf: &[u8], len: usize) {
    let mut i = 0;
    while i < len / 8 {
        let off = i * 8;
        let lo = (buf[off] as u32)
            | ((buf[off + 1] as u32) << 8)
            | ((buf[off + 2] as u32) << 16)
            | ((buf[off + 3] as u32) << 24);
        let hi = (buf[off + 4] as u32)
            | ((buf[off + 5] as u32) << 8)
            | ((buf[off + 6] as u32) << 16)
            | ((buf[off + 7] as u32) << 24);
        state[2 * i] ^= hi;
        state[2 * i + 1] ^= lo;
        i += 1;
    }
}

/// DeepSeekHashV1：data 的 32 字节摘要（Keccak-f 跳过 round 0）
#[allow(dead_code)]
pub fn deep_seek_hash_v1(data: &[u8]) -> [u8; 32] {
    let mut state = [0u32; 50];
    let mut off = 0usize;
    while off + RATE <= data.len() {
        absorb_block(&mut state, &data[off..off + RATE], RATE);
        keccak_f23(&mut state, 1);
        off += RATE;
    }
    let mut block = [0u8; RATE];
    block[..data.len() - off].copy_from_slice(&data[off..]);
    block[data.len() - off] = 0x06;
    block[RATE - 1] |= 0x80;
    absorb_block(&mut state, &block, RATE);
    keccak_f23(&mut state, 1);
    let mut out = [0u8; 32];
    for i in 0..4 {
        let lo = state[2 * i + 1];
        let hi = state[2 * i];
        let o = i * 8;
        out[o] = lo as u8;
        out[o + 1] = (lo >> 8) as u8;
        out[o + 2] = (lo >> 16) as u8;
        out[o + 3] = (lo >> 24) as u8;
        out[o + 4] = hi as u8;
        out[o + 5] = (hi >> 8) as u8;
        out[o + 6] = (hi >> 16) as u8;
        out[o + 7] = (hi >> 24) as u8;
    }
    out
}

/// 遍历 answer ∈ [0, difficulty)，返回匹配的 answer；找不到返回 -1。
pub fn solve_pow(challenge_hex: &str, salt: &str, expire_at: u64, difficulty: u64) -> i64 {
    let mut target = [0u8; 32];
    for i in 0..32 {
        target[i] = u8::from_str_radix(&challenge_hex[i * 2..i * 2 + 2], 16).unwrap_or(0);
    }
    let t0h = u32::from_le_bytes([target[4], target[5], target[6], target[7]]);
    let t0l = u32::from_le_bytes([target[0], target[1], target[2], target[3]]);
    let t1h = u32::from_le_bytes([target[12], target[13], target[14], target[15]]);
    let t1l = u32::from_le_bytes([target[8], target[9], target[10], target[11]]);
    let t2h = u32::from_le_bytes([target[20], target[21], target[22], target[23]]);
    let t2l = u32::from_le_bytes([target[16], target[17], target[18], target[19]]);
    let t3h = u32::from_le_bytes([target[28], target[29], target[30], target[31]]);
    let t3l = u32::from_le_bytes([target[24], target[25], target[26], target[27]]);

    // prefix 预吸收进 state（循环内零分配优化）
    let prefix = format!("{}_{}_", salt, expire_at);
    let prefix_bytes = prefix.as_bytes();
    let mut state = [0u32; 50];
    let mut off = 0usize;
    while off + RATE <= prefix_bytes.len() {
        absorb_block(&mut state, &prefix_bytes[off..off + RATE], RATE);
        keccak_f23(&mut state, 1);
        off += RATE;
    }
    let tail_bytes = &prefix_bytes[off..];
    let tail_len = prefix_bytes.len() - off;

    let mut buf = [0u8; RATE];
    let mut num_buf = [0u8; 20];

    let mut n: u64 = 0;
    while n < difficulty {
        let mut v = n;
        let mut pos = 20usize;
        if v == 0 {
            pos -= 1;
            num_buf[pos] = 48;
        } else {
            while v > 0 {
                pos -= 1;
                num_buf[pos] = 48 + (v % 10) as u8;
                v /= 10;
            }
        }
        let num_len = 20 - pos;
        let total = tail_len + num_len;

        let mut s = state;
        if total < RATE {
            buf.fill(0);
            buf[..tail_len].copy_from_slice(tail_bytes);
            buf[tail_len..tail_len + num_len].copy_from_slice(&num_buf[pos..pos + num_len]);
            buf[total] = 0x06;
            buf[RATE - 1] |= 0x80;
            absorb_block(&mut s, &buf, RATE);
            keccak_f23(&mut s, 1);
        } else {
            // 两块（tail + 数字超出 136 字节时才发生，数字最多 20 字节）
            buf.fill(0);
            buf[..tail_len].copy_from_slice(tail_bytes);
            buf[tail_len..RATE].copy_from_slice(&num_buf[pos..pos + (RATE - tail_len)]);
            absorb_block(&mut s, &buf, RATE);
            keccak_f23(&mut s, 1);
            buf.fill(0);
            let second_len = 20 - pos - (RATE - tail_len);
            buf[..second_len].copy_from_slice(&num_buf[pos + (RATE - tail_len)..20]);
            let rem = total - RATE;
            buf[rem] = 0x06;
            buf[RATE - 1] |= 0x80;
            absorb_block(&mut s, &buf, RATE);
            keccak_f23(&mut s, 1);
        }

        if s[0] == t0h
            && s[1] == t0l
            && s[2] == t1h
            && s[3] == t1l
            && s[4] == t2h
            && s[5] == t2l
            && s[6] == t3h
            && s[7] == t3l
        {
            return n as i64;
        }
        n += 1;
    }
    -1
}

/// 序列化 PoW 应答头：base64(JSON{...})。不含 difficulty/expire_at。
pub fn build_pow_header(
    algorithm: &str,
    challenge: &str,
    salt: &str,
    answer: i64,
    signature: &str,
    target_path: &str,
) -> String {
    use base64::Engine;
    let obj = serde_json::json!({
        "algorithm": algorithm,
        "challenge": challenge,
        "salt": salt,
        "answer": answer,
        "signature": signature,
        "target_path": target_path,
    });
    let json = serde_json::to_string(&obj).unwrap_or_default();
    base64::engine::general_purpose::STANDARD.encode(json.as_bytes())
}
