// .claude/workflows/scrollz-contract-probe.js
export const meta = {
  name: 'scrollz-contract-probe',
  description: '冻结 Workflow API 契约：meta 形状、args 全局、agent 位置参数、schema 返回',
  phases: [{ title: 'Probe', detail: '单 agent 结构化返回' }],
};

const SCHEMA = {
  type: 'object',
  required: ['echo', 'lens'],
  properties: {
    echo: { type: 'string' },
    lens: { type: 'string' },
  },
};

const token = args.token || 'missing';

const res = await agent(
  `只返回结构化结果：echo 字段填 "${token}"，lens 字段填 "roadmap"。不要读任何文件。`,
  { agentType: 'harness-finder-roadmap', phase: 'Probe', label: 'probe', schema: SCHEMA }
);

return { echo: res.echo, lens: res.lens, args_seen: token };
