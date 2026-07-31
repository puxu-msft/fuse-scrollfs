// 降级记录去重的验收。跑法：node .claude/workflows/tests/degraded-dedup.test.mjs
//
// 为什么需要它：传输层故障会连续多次出现，而连续的同类故障之间**没有信息含量**
// ——第 3 条 "API Error: Server error mid-response" 不比第 1 条多告诉任何事，逐条
// 堆进 degraded 只会把真正有区别的失败淹掉。
//
// 本文件从 scrollz-propose.js 复制了 normalizeError/recordDegraded 两个纯函数。
// workflow 脚本没有模块导出（Workflow 工具要求首字节是 export const meta），无法
// import，因此**改了那两个函数就要同步改这里**。
//
// 这个测试证伪过一次真实缺陷：原正则用 `\b[0-9a-f]{8,}\b` 卡边界，而请求 ID 常以
// `req_9f3a…` 出现，下划线是词字符、`\b` 在 `_9` 处不成立，ID 原样留下、去重完全
// 失效。没有这个测试就会带着"已去重"的假象上线。

function normalizeError(err) {
  return String((err && err.message) || err)
    // 不能用 \b 卡边界：请求 ID 常以 `req_9f3a…` 形式出现，下划线是词字符，
    // `\b` 在 `_9` 处不成立，ID 会原样留下、去重随即失效（node 实测证伪过）。
    .replace(/\d{10,}/g, '<ts>')                // 时间戳（先于 ID，避免被吃掉）
    .replace(/[0-9a-f]{8,}/gi, '<id>')           // 请求 ID / trace ID
    .replace(/\s+/g, ' ')
    .trim()
    .slice(0, 300);
}

function recordDegraded(degraded, opts, error, attempts) {
  const hit = degraded.find(
    (d) => d.agentType === opts.agentType && d.label === opts.label && d.error === error
  );
  if (hit) {
    hit.occurrences += 1;
    hit.attempts += attempts;
    return;
  }
  degraded.push({
    label: opts.label,
    agentType: opts.agentType,
    error,
    occurrences: 1,
    attempts,
  });
}

async function safeAgent(prompt, opts, degraded) {
  let lastError = 'unknown';
  for (let attempt = 1; attempt <= MAX_AGENT_ATTEMPTS; attempt++) {
    try {
      return await agent(prompt, opts);
    } catch (err) {
      lastError = normalizeError(err);
    }
  }
  recordDegraded(degraded, opts, lastError, MAX_AGENT_ATTEMPTS);
  return null;
}

// --- 验收 ---
const degraded = [];
const opts = { agentType: 'harness-finder-roadmap', label: 'roadmap' };
// 同一段抖动里三条只有 ID/时间戳不同的样板错误
recordDegraded(degraded, opts, normalizeError(new Error('API Error: Server error mid-response. req_9f3a2b7c1d')), 3);
recordDegraded(degraded, opts, normalizeError(new Error('API Error: Server error mid-response. req_11ee44aa99')), 3);
recordDegraded(degraded, opts, normalizeError(new Error('API Error: Server error   mid-response. req_deadbeef42')), 3);
// 一条真正不同的错误
recordDegraded(degraded, opts, normalizeError(new Error('schema validation failed: candidates')), 3);
// 另一个 agent 的同样板错误 —— 不应与 roadmap 折叠
recordDegraded(degraded, { agentType: 'harness-judge-redline', label: 'harness-judge-redline' },
               normalizeError(new Error('API Error: Server error mid-response. req_777')), 3);

console.log(JSON.stringify(degraded, null, 1));
const ok = degraded.length === 3
  && degraded[0].occurrences === 3 && degraded[0].attempts === 9
  && degraded[1].occurrences === 1
  && degraded[2].agentType === 'harness-judge-redline';
console.log(ok ? 'PASS' : 'FAIL');
