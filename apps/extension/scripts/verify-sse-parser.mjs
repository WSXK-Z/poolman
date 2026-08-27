// 用真实 DeepSeek SSE 流验证解析器（含 RESPONSE 片段 + response/status SET 的 bug 场景）
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

// ---- 解析器（与 deepseek.ts readDeepSeekStream 一致） ----
const fragments = [];
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

for (const line of raw.split('\n')) {
  const t = line.trim();
  if (t.startsWith('data:')) {
    const jsonStr = t.slice(5).trim();
    if (jsonStr && jsonStr !== '[DONE]') handleData(jsonStr);
  }
}

const answer = fragments.filter((f) => f.type !== 'THINK').map((f) => f.content).join('');
const all = fragments.map((f) => f.content).join('');
const content = answer || all;

console.log('片段类型:', fragments.map((f) => f.type));
console.log('--- answer（非THINK）---');
console.log(JSON.stringify(answer));
console.log('--- allContent ---');
console.log(JSON.stringify(all));
console.log('--- content ---');
console.log(JSON.stringify(content));

let ok = true;
if (content === 'FINISHED') { console.log('❌ BUG：content 被 response/status 覆盖为 FINISHED'); ok = false; }
if (!content.startsWith('我是')) { console.log('❌ content 未以答案开头'); ok = false; }
else { console.log('✅ content 正确提取到答案'); }
console.log(ok ? '🎉 解析器正确' : '❌ 解析器有误');
process.exit(ok ? 0 : 1);
