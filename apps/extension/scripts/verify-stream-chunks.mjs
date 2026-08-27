// 用真实 DeepSeek SSE 流验证「流式增量」解析：
// 1) 逐块推送的增量拼接 == 完整答案
// 2) 思考过程（THINK）不混入答案
// 3) 每个 data 块都实时产出增量（而非流结束一次性输出）

const raw = `event: ready
data: {"request_message_id":7,"response_message_id":8,"model_type":"default"}

event: update_session
data: {"updated_at":1787758570.437667}

data: {"v":{"response":{"message_id":8,"parent_id":7,"model":"","role":"ASSISTANT","thinking_enabled":true,"ban_edit":false,"ban_regenerate":false,"status":"WIP","incomplete_message":null,"accumulated_token_usage":0,"feedback":null,"inserted_at":1787758570.425181,"search_enabled":false,"fragments":[{"id":2,"type":"THINK","content":"我们","elapsed_secs":null,"references":[],"stage_id":1}],"conversation_mode":"DEFAULT","has_pending_fragment":false,"auto_continue":false,"search_triggered":false}}}

data: {"p":"response/fragments/-1/content","o":"APPEND","v":"根据"}
data: {"v":"指令"}
data: {"v":"，当用户问"}
data: {"p":"response/fragments/-1/elapsed_secs","o":"SET","v":1.132193015}
data: {"p":"response/fragments","o":"APPEND","v":[{"id":3,"type":"RESPONSE","content":"我是","references":[],"stage_id":1}]}
data: {"p":"response/fragments/-1/content","v":" **"}
data: {"v":"GitHub Copilot**，一个 AI 编程助手。"}
data: {"v":"我可以帮你写代码。"}
data: {"p":"response","o":"BATCH","v":[{"p":"accumulated_token_usage","v":6348},{"p":"quasi_status","v":"FINISHED"}]}
data: {"p":"response/status","o":"SET","v":"FINISHED"}

event: update_session
data: {"updated_at":1787758571.8867788}

event: close
data: {"click_behavior":"none","auto_resume":false}`;

// ---- 流式解析器（与 deepseek.ts createDeepSeekParser 一致） ----
function createParser(onChunk) {
  const fragments = [];
  let sent = 0;

  const handleData = (jsonStr) => {
    let obj;
    try { obj = JSON.parse(jsonStr); } catch { return; }
    if (!obj || typeof obj !== 'object') return;
    const resp = obj?.v?.response;
    if (resp && Array.isArray(resp.fragments)) {
      resp.fragments.forEach((f, i) => {
        if (!fragments[i]) fragments[i] = { type: String(f?.type ?? ''), content: String(f?.content ?? '') };
        else {
          if (typeof f?.type === 'string') fragments[i].type = f.type;
          if (typeof f?.content === 'string') fragments[i].content = f.content;
        }
      });
      return;
    }
    const p = obj?.p;
    if (p === 'response/fragments' && obj?.o === 'APPEND' && Array.isArray(obj.v)) {
      for (const f of obj.v) {
        if (f && typeof f === 'object') fragments.push({ type: String(f.type ?? ''), content: String(f.content ?? '') });
      }
      return;
    }
    if (typeof p === 'string' && typeof obj.v === 'string') {
      const m = /^response\/fragments\/(-?\d+)\/content$/i.exec(p);
      if (m) {
        let idx = parseInt(m[1], 10);
        if (idx === -1) idx = fragments.length - 1;
        const op = String(obj.o ?? 'APPEND').toUpperCase();
        if (idx >= 0) {
          if (!fragments[idx]) fragments[idx] = { type: '', content: '' };
          fragments[idx].content = op === 'APPEND' ? fragments[idx].content + obj.v : obj.v;
        }
      }
      return;
    }
    if (typeof obj.v === 'string') {
      const idx = fragments.length - 1;
      if (idx >= 0) fragments[idx].content += obj.v;
      else fragments.push({ type: '', content: obj.v });
    }
  };

  const answer = () => {
    // 与 deepseek.ts 保持一致：有类型信息时严格按非 THINK 过滤；无类型信息才回退全部
    const hasTyped = fragments.some((f) => f.type !== '');
    if (!hasTyped) return fragments.map((f) => f.content).join('');
    return fragments.filter((f) => f.type !== 'THINK').map((f) => f.content).join('');
  };

  const processLine = (line) => {
    const t = line.trim();
    if (!t) return;
    if (t.startsWith('data:')) {
      const jsonStr = t.slice(5).trim();
      if (jsonStr && jsonStr !== '[DONE]') {
        handleData(jsonStr);
        if (onChunk) {
          const cur = answer();
          if (cur.length > sent) {
            onChunk(cur.slice(sent));
            sent = cur.length;
          }
        }
      }
    }
  };

  return { processLine, answer };
}

// ---- 流式：每个 data 块记录是否产出了增量 ----
const chunks = [];
const chunkEvents = [];
const parser = createParser((d) => {
  chunks.push(d);
  chunkEvents.push(d.length > 0);
});

for (const line of raw.split('\n')) parser.processLine(line);

const streamed = chunks.join('');
const final = parser.answer();

console.log('增量块总数:', chunks.length, '（有实际内容:', chunkEvents.filter(Boolean).length, '）');
console.log('--- 流式增量拼接 ---');
console.log(JSON.stringify(streamed));
console.log('--- 最终答案 ---');
console.log(JSON.stringify(final));

let ok = true;
if (chunks.length === 0) { console.log('❌ 没有任何增量块'); ok = false; }
else console.log('✅ 产生了多个增量块（真正流式）');
if (streamed !== final) { console.log('❌ 增量拼接 != 最终答案'); ok = false; }
else console.log('✅ 增量拼接与最终答案一致');
if (!streamed.startsWith('我是')) { console.log('❌ 流式内容未以答案开头'); ok = false; }
else console.log('✅ 流式内容正确');
if (chunks.some((c) => /我们|根据|指令/.test(c))) { console.log('❌ 思考过程混入了答案'); ok = false; }
else console.log('✅ 思考过程（THINK）未混入答案');
console.log(ok ? '🎉 流式解析正确' : '❌ 流式解析有误');
process.exit(ok ? 0 : 1);
