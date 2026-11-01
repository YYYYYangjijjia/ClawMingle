// Synthetic UI data only. Never load a running manager's state or credentials.
export function stateFixture() {
  const policy = {
    response_instructions: '认真回答测试问题。',
    formatting_instructions: '按需使用段落。',
    categories: Object.fromEntries(['casual', 'factual', 'explanation', 'comparison', 'troubleshooting', 'task', 'emotional', 'clarification']
      .map(id => [id, { name: id, instructions: '测试回答策略。' }])),
    modes: Object.fromEntries(['mode-1', 'mode-2', 'mode-3', 'mode-all']
      .map(id => [id, { name: id, participation_instructions: '根据上下文判断。', reply_instructions: `${id} 旧版测试文案` }])),
  };
  return {
    service: { active: false, substate: 'dead', restarts: 0, memory_bytes: 0 },
    behavior: {
      current: structuredClone(policy), defaults: structuredClone(policy),
      soul: { default: '默认测试性格', effective: '当前测试性格', uses_default: false },
      context_windows: { jev_messages: 25, jev_bot_messages: 8, codex_messages: 40, codex_bot_messages: 12 },
      max_bubbles: 3, max_bubble_chars: 2000,
    },
    config: {
      mode: 'mode-1', private_enabled: true, max_private_sessions: 16,
      private_allowed_users: [], command_admins: [], quiet_ms: 1200, max_wait_ms: 4000,
      model_presets: { demo: { model: 'fixture-model', reasoning_effort: 'high' } },
      qq_app_id: '', jev_model: 'fixture-jev', reply_format: 'text',
      soul: '当前测试性格', persona: { identity: '测试助手', files_enabled: true },
      web_search: 'disabled', enabled_tools: [],
      provider: { id: 'openai', model: 'fixture-model', reasoning_effort: 'high' },
      memory: { enabled: true, automatic_capture: true, archive_hour: 4, archive_minute: 0, archive_every_days: 1, short_term_days: 14 },
    },
    credentials: { qq: false, jev: false, provider: false },
    login: { authenticated: false, personal_available: false },
    groups: [{ id: 'group-fixture', name: '测试群聊', pending: 0, failed: 0, message_count: 4 },
      { id: 'group-second', name: '另一个测试群', pending: 0, failed: 0, message_count: 0 }],
    private_chats: [{ id: 'private-fixture', name: '测试私聊', pending: 0, failed: 0, message_count: 2 }],
    models: [{ id: 'fixture-model', model: 'fixture-model', display_name: '测试模型', reasoning_efforts: ['high'] }],
    model_list_source: 'configured', skills: [], health: {},
  };
}

export function memoryFixture(scope) {
  const entry = {
    revision: '1', category: 'profile', certainty: 'explicit', source_kind: 'user_requested',
    subjects: ['member-fixture'], created_at: 1800000000000, updated_at: 1800000000000,
    evidence: [], history: [],
  };
  return {
    ...scope, name: scope.private ? '测试私聊' : scope.target === 'group-second' ? '另一个测试群' : '测试群聊',
    legacy: {}, layout: {}, logical_layout: [],
    ledger: {
      drafts: [{ ...entry, id: '42', revision: '2', content: scope.private ? '私聊独立草稿' : '<img src=x onerror="window.injected=true">',
        status: 'buffered', disposition: 'retain', operation: { action: 'append' } }],
      records: [{ ...entry, id: '7', content: '原正式资料', status: 'active' },
        { ...entry, id: '8', content: '过期资料', status: 'expired' }],
      reviews: [], maintenance: {},
    },
  };
}
