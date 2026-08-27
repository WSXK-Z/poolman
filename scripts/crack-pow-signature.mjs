/**
 * 用捕获到的两组 x-ds-pow-response 样本反推 signature 公式。
 * 样本来自 DeepSeek 网页真实请求（服务端诊断日志）。
 */
import { createHash } from 'node:crypto';

const samples = [
  {
    algorithm: 'DeepSeekHashV1',
    challenge: '83597772387960cd3517a015cb0723baa119b1d4b3e777269436f51ae25a7bf2',
    salt: 'ba514708f78a22369c0c',
    answer: 66857,
    signature: '2ded8b31f9bd74f5d07315c9a0a3f5b7f2ee8cf09d8d2483534633c1c8305eeb',
    target_path: '/api/v0/chat/completion',
  },
  {
    algorithm: 'DeepSeekHashV1',
    challenge: 'acc801f04a76fec10b20f97a4b34f7775f629d4fec38c7c61f4cea74b27f03b9',
    salt: 'f55d70aef23c4b367132',
    answer: 16922,
    signature: '6367d41533d03239c8c2b92e4bee63beb1803064dc69deb8c9817232ff5bf59d',
    target_path: '/api/v0/chat/completion',
  },
];

const sha256 = (s) => createHash('sha256').update(s, 'utf8').digest('hex');
const isHex = (s) => /^[0-9a-f]+$/i.test(s);

const candidates = [
  ['utf8 c+s+a', (s) => s.challenge + s.salt + s.answer],
  ['utf8 c+s+aStr', (s) => s.challenge + s.salt + String(s.answer)],
  ['utf8 c+s+a+path', (s) => s.challenge + s.salt + s.answer + s.target_path],
  ['utf8 c+s+aStr+path', (s) => s.challenge + s.salt + String(s.answer) + s.target_path],
  ['utf8 c+a+s', (s) => s.challenge + s.answer + s.salt],
  ['utf8 s+c+a', (s) => s.salt + s.challenge + s.answer],
  ['utf8 c-s-a', (s) => `${s.challenge}-${s.salt}-${s.answer}`],
  ['utf8 c-s-aStr', (s) => `${s.challenge}-${s.salt}-${String(s.answer)}`],
  ['utf8 c|s|a', (s) => `${s.challenge}|${s.salt}|${s.answer}`],
  ['utf8 jsonNoSig', (s) => JSON.stringify({ algorithm: s.algorithm, challenge: s.challenge, salt: s.salt, answer: s.answer, target_path: s.target_path })],
  ['utf8 c+path+s+a', (s) => s.challenge + s.target_path + s.salt + s.answer],
  ['utf8 a+s+c', (s) => String(s.answer) + s.salt + s.challenge],
  ['utf8 a+c+s', (s) => String(s.answer) + s.challenge + s.salt],
  ['utf8 s+a+c', (s) => s.salt + String(s.answer) + s.challenge],
  ['utf8 c+s+hexA', (s) => s.challenge + s.salt + s.answer.toString(16)],
];

let any = false;
for (const [name, fn] of candidates) {
  const r1 = sha256(fn(samples[0]));
  const r2 = sha256(fn(samples[1]));
  const m1 = r1 === samples[0].signature;
  const m2 = r2 === samples[1].signature;
  if (m1 || m2) {
    console.log(`[MATCH] ${name}: s1=${m1} s2=${m2}`);
    any = true;
  }
}
if (!any) console.log('NO MATCH in utf8 candidates');

// hex 变体（仅当拼接结果是合法 hex 时）
for (const [name, fn] of candidates) {
  try {
    const c = fn(samples[0]);
    if (!isHex(c)) continue;
    const r1 = sha256(Buffer.from(c, 'hex'));
    if (r1 === samples[0].signature) console.log(`[MATCH-hex] ${name}: s1=true`);
  } catch {
    /* ignore */
  }
}
console.log('DONE');
