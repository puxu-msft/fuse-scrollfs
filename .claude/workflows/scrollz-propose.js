// .claude/workflows/scrollz-propose.js
// 段 1：扫描 → 去重 → 对抗裁决 → 选一。不产生任何外部副作用。
//
// API 形状按 Workflow 工具 schema：
//   - 文件必须以 `export const meta = {...}` 纯字面量开头（不可引用变量）
//   - 其余为顶层 async 代码，`args` 是全局，不是函数参数
//   - agent(prompt, opts)：prompt 是第一个位置参数
//   - 传 schema 时直接返回已校验的结构化对象，**不要**自己解析文本
//   - 无文件系统访问、无 Date.now()/Math.random()

export const meta = {
  name: 'scrollz-propose',
  description: 'scrollz harness 段 1：四视角扫描、去重、三方对抗裁决、选出一个候选',
  // phases 是 {title, detail} 对象数组；title 必须与下面 agent(opts.phase)
  // 传的字符串**逐字相同**，否则进度分组对不上
  phases: [
    { title: 'Scan', detail: '四个视角并行扫描仓库，产出候选' },
    { title: 'Judge', detail: '三方对抗裁决，任一否决即淘汰' },
  ],
};

const CANDIDATE_SCHEMA = {
  type: 'object',
  required: ['candidates'],
  properties: {
    candidates: {
      type: 'array',
      maxItems: 3,
      items: {
        type: 'object',
        required: ['title', 'goal', 'invariant', 'primary_path', 'oracle',
                   'evidence', 'touched_paths', 'size', 'priority',
                   'needs_decision', 'body_md', 'slug'],
        properties: {
          title: { type: 'string' },
          goal: { type: 'string' },
          invariant: { type: 'string' },
          primary_path: { type: 'string' },
          oracle: { type: 'string' },
          evidence: { type: 'string' },
          touched_paths: { type: 'array', items: { type: 'string' } },
          size: { type: 'string', enum: ['S', 'M', 'L'] },
          priority: { type: 'string', enum: ['T0', 'T1', 'T2', 'T3', 'T4'] },
          needs_decision: { type: 'boolean' },
          body_md: { type: 'string' },
          slug: { type: 'string' },
        },
      },
    },
  },
};

const VERDICT_SCHEMA = {
  type: 'object',
  required: ['verdict', 'reason'],
  properties: {
    verdict: { type: 'string', enum: ['pass', 'reject', 'needs_decision'] },
    reason: { type: 'string' },
    evidence: { type: 'string' },
  },
};

const LENSES = [
  { agentType: 'harness-finder-roadmap', lane: 'roadmap' },
  { agentType: 'harness-finder-code', lane: 'defect' },
  { agentType: 'harness-finder-bench', lane: 'perf' },
  { agentType: 'harness-finder-hygiene', lane: 'hygiene' },
];

const JUDGES = [
  'harness-judge-completed',
  'harness-judge-redline',
  'harness-judge-oracle',
];

const PRIORITY_ORDER = { T0: 0, T1: 1, T2: 2, T3: 3, T4: 4 };
const SIZE_ORDER = { S: 0, M: 1, L: 2 };

// 与 Python 侧 queue.fingerprint 使用同一规范化协议：四字段以 \x1f 连接，
// 空白折叠 + 转小写。Python 侧再取 sha256[:32]；此处只做 key 归一，
// 真正的硬去重由控制器完成（脚本内无 crypto）。
function canonicalKey(c) {
  return [c.goal, c.invariant, c.primary_path, c.oracle]
    .map((x) => String(x || '').trim().toLowerCase().replace(/\s+/g, ' '))
    .join('\x1f');
}

const blockedLanes = args.blocked_lanes || [];
const knownKeys = new Set(args.known_keys || []);
const inflightPaths = args.inflight_paths || [];

const found = await parallel(
  LENSES.map((lens) => async () => {
    const res = await agent(
      '扫描本仓库，按你的视角给出候选。严格遵循输出 schema。',
      {
        agentType: lens.agentType,
        phase: 'Scan',
        label: lens.lane,
        schema: CANDIDATE_SCHEMA,
      }
    );
    const list = (res && res.candidates) || [];
    return list.map((c) => ({ ...c, lane: lens.lane }));
  })
);

const seen = new Set(knownKeys);
const deduped = [];
for (const c of found.flat()) {
  if (!c || !c.title || !c.oracle) continue;
  if (blockedLanes.includes(c.lane)) continue;
  const key = canonicalKey(c);
  if (seen.has(key)) continue;
  seen.add(key);
  deduped.push({ ...c, canonical_key: key });
}

if (deduped.length === 0) {
  return { candidates: [], reason: 'no-candidate-after-dedupe' };
}

const ranked = deduped.sort((a, b) => {
  const p = (PRIORITY_ORDER[a.priority] ?? 9) - (PRIORITY_ORDER[b.priority] ?? 9);
  if (p !== 0) return p;
  return (SIZE_ORDER[a.size] ?? 9) - (SIZE_ORDER[b.size] ?? 9);
});

const rejected = [];
for (const candidate of ranked.slice(0, 3)) {
  const verdicts = await parallel(
    JUDGES.map((judgeType) => async () => {
      const res = await agent(
        '裁决以下候选。在飞变更触碰面：' +
          JSON.stringify(inflightPaths) +
          '\n候选：' +
          JSON.stringify(candidate),
        {
          agentType: judgeType,
          phase: 'Judge',
          label: judgeType,
          schema: VERDICT_SCHEMA,
        }
      );
      return { judge: judgeType, ...res };
    })
  );

  if (verdicts.some((v) => v.verdict === 'reject')) {
    rejected.push({ title: candidate.title, verdicts });
    continue;
  }
  const needsDecision =
    candidate.needs_decision || verdicts.some((v) => v.verdict === 'needs_decision');
  return {
    candidates: [{ ...candidate, needs_decision: needsDecision, verdicts }],
    rejected,
  };
}

return { candidates: [], rejected };
