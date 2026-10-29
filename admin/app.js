"use strict";

(() => {
  const $ = (selector, parent = document) => parent.querySelector(selector);
  const $$ = (selector, parent = document) => [...parent.querySelectorAll(selector)];
  const base = new URL("../", $("#app-script").src);
  const apiBase = new URL("api/", base);
  const views = {
    overview: ["总览", "连接、会话和运行状况，一目了然。"],
    integrations: ["连接与模型", "把需要的连接配好，让每次回应顺利抵达。"],
    groups: ["群聊管理", "为不同的群，留出不同的相处方式。"],
    documents: ["群在线文档", "让计划、清单与资料成为可以共同编辑的文档。"],
    memory: ["记忆管理", "群聊与私聊独立，缓冲、正式记忆与来源历史清楚可查。"],
    private: ["私聊管理", "独立的上下文，专注于眼前这段对话。"],
    login: ["登录与授权", "管理机器人独立使用的 Codex 登录。"],
    persona: ["人格与能力", "调整它的表达习惯，为它补充专门的工作方法。"],
    workflows: ["任务与扩展", "有进展、有追问，复杂任务也有清楚的交代。"],
    stickers: ["表情包", "有趣但不刷屏，用图片接住恰到好处的情绪。"],
  };
  const toolNames = ["workspace_exec", "time_now", "weather_current", "public_web_read", "message_lookup", "messages_list", "tasks_manage", "qq_info", "group_documents", "memory_manage", "task_update", "sticker_lookup", "sticker_manage", "identity_manage", "group_profile_manage"];
  const effortNames = { none: "不使用", minimal: "极低", low: "低", medium: "中", high: "高", xhigh: "最高", max: "最大" };
  let state = null;
  let csrfToken = null;
  let refreshing = false;
  let lastUpdated = null;
  let modalResolve = null;
  let device = null;
  let editingSkill = null;
  let currentView = null;
  let memoryGroupId = "";
  let memoryPrivate = false;
  const memoryEdits = new Map();
  let memoryData = null;
  let memoryRequest = 0;
  let memoryLoading = false;
  let memoryMaintaining = false;
  let stickerData = null, editingSticker = null, stickerLoading = false;
  let stickerPreviewRequest = 0, stickerImporting = false, stickerPreferenceRequest = 0;
  let stickerTaxonomy = {styles: [], emotions: []};
  let stickerDefaultInstructions = "";
  let capabilityData = null, capabilityLoading = false;
  let capabilityRefreshing = false;
  let documentNativeFlow = null, documentNativePolling = false;

  const element = (tag, className, text) => {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined) node.textContent = String(text);
    return node;
  };
  const number = value => Number.isFinite(Number(value)) ? Number(value) : 0;
  const active = service => service?.active === true || ["active", "running"].includes(service?.active);
  const modeName = mode => state?.behavior?.current?.modes?.[mode]?.name ? `${mode} · ${state.behavior.current.modes[mode].name}` : ({ "mode-1": "模式 1 · 仅被 @", "mode-2": "模式 2 · 按需帮助", "mode-3": "模式 3 · 积极参与", "mode-all": "全部回应" })[mode] || "跟随全局";
  const listInput = value => String(value || "").split(/[\s,，]+/u).map(item => item.trim()).filter(Boolean);
  const fields = form => Object.fromEntries(new FormData(form));
  const text = (selector, value) => { $(selector).textContent = String(value ?? "—"); };
  const cleanForm = form => { form.dataset.dirty = "false"; };
  const setField = (form, name, value) => {
    const input = form.elements.namedItem(name);
    if (!input || input.type === "password") return;
    if (input.type === "checkbox") input.checked = Boolean(value);
    else input.value = value ?? "";
  };

  function toast(message, error = false) {
    const item = element("div", `toast${error ? " error" : ""}`);
    item.setAttribute("role", error ? "alert" : "status");
    item.append(element("span", "toast-symbol", error ? "!" : "✓"), element("span", "", message));
    const close = element("button", "", "×");
    close.setAttribute("aria-label", "关闭提示");
    close.addEventListener("click", () => item.remove());
    item.append(close);
    $("#toast-stack").append(item);
    const limit = window.innerWidth < 700 ? 2 : 3;
    while ($("#toast-stack").children.length > limit) $("#toast-stack").firstElementChild.remove();
    setTimeout(() => item.remove(), error ? 9000 : 5500);
  }

  async function api(path, body) {
    if (body !== undefined && !csrfToken) throw new Error("管理权限尚未就绪，请返回 Dashboard 登录后刷新。");
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 65000);
    const options = { credentials: "same-origin", cache: "no-store", signal: controller.signal, headers: { Accept: "application/json" } };
    if (body !== undefined) {
      options.method = "POST";
      options.headers["Content-Type"] = "application/json";
      options.headers["X-CSRF-Token"] = csrfToken;
      options.body = JSON.stringify(body);
    }
    try {
      const response = await fetch(new URL(path, apiBase), options);
      let data;
      try { data = await response.json(); } catch { throw new Error("服务返回了无法识别的响应，请稍后重试。"); }
      if (!response.ok) {
        let message = typeof data.error === "string" ? data.error : data.error?.message;
        if (response.status === 409) message = message || "当前仍有请求正在处理，尚未应用修改。请空闲后重试。";
        if (response.status === 401 || response.status === 403) message = "管理登录已过期，请返回 Dashboard 登录后刷新。";
        throw new Error(message || "操作未完成，请稍后重试。");
      }
      return data;
    } catch (error) {
      if (error.name === "AbortError") throw new Error("操作等待超时，结果尚未确认。请刷新状态后再决定是否重试。");
      if (error instanceof TypeError) throw new Error("暂时无法连接管理服务，请检查连接后重试。");
      throw error;
    } finally { clearTimeout(timer); }
  }

  async function bootstrap() {
    try {
      const response = await fetch("/api/bootstrap", { credentials: "same-origin", cache: "no-store", headers: { Accept: "application/json" } });
      if (!response.ok) throw new Error("管理登录尚未就绪，请返回 Dashboard 登录后刷新。");
      const data = await response.json();
      csrfToken = typeof data.csrfToken === "string" && data.csrfToken ? data.csrfToken : null;
      if (currentView === "stickers" && stickerData === null) loadStickers();
      if (!csrfToken) throw new Error("请先登录 Dashboard 管理员账号，再使用工作台。");
      return true;
    } catch (error) {
      showConnectionError(error.message);
      return false;
    } finally { updateWriteAccess(); }
  }

  function showConnectionError(message) {
    text("#connection-banner", message);
    $("#connection-banner").hidden = false;
  }

  async function refreshState(manual = false) {
    if (refreshing || capabilityRefreshing) return;
    refreshing = true;
    try {
      state = await api("state");
      lastUpdated = new Date();
      if (csrfToken) $("#connection-banner").hidden = true;
      render();
      if (manual) toast("状态已更新。");
    } catch (error) {
      showConnectionError(`${error.message}${state ? " 当前显示上次成功读取的数据。" : ""}`);
      if (manual) toast(error.message, true);
    } finally { refreshing = false; }
  }

  async function perform(button, task) {
    if (button?.classList.contains("busy")) return;
    if (button) { button.classList.add("busy"); button.disabled = true; }
    try { await task(); } catch (error) { toast(error.message, true); }
    finally {
      if (button) { button.classList.remove("busy"); button.disabled = false; }
      updateWriteAccess();
    }
  }

  function updateWriteAccess() {
    $$('[data-write]').forEach(button => { button.disabled = !csrfToken || capabilityRefreshing || button.classList.contains("busy"); });
    if (!state) return;
    const running = active(state.service);
    $("#service-start").disabled ||= running;
    $("#service-stop").disabled ||= !running;
    $("#service-restart").disabled ||= !running;
    $("#login-reuse").disabled ||= !state.login?.personal_available;
    $("#login-logout").disabled ||= !state.login?.authenticated;
    $("#memory-kind-select").disabled = !csrfToken || memoryMaintaining;
    $("#memory-group-select").disabled = !csrfToken || !(memoryPrivate ? state.private_chats || [] : state.groups || []).length || memoryMaintaining;
    $("#memory-refresh").disabled = !csrfToken || !memoryGroupId || memoryLoading || memoryMaintaining;
    $("#memory-maintain").disabled = !csrfToken || !memoryGroupId || !memoryData || memoryLoading || memoryMaintaining;
    $$(".memory-entry [data-write]").forEach(button => { button.disabled ||= memoryLoading || memoryMaintaining; });
  }

  function badge(selector, label, kind) {
    const node = $(selector);
    node.textContent = label;
    node.className = `badge ${kind}`;
  }

  function shortTime(value) {
    if (!value) return "暂无活动";
    const raw = typeof value === "string" && !/^\d+$/u.test(value) ? value : number(value) < 1e12 ? number(value) * 1000 : number(value);
    const date = new Date(raw);
    if (!Number.isFinite(date.getTime())) return "暂无活动";
    const minutes = Math.max(0, Math.floor((Date.now() - date.getTime()) / 60000));
    if (minutes < 1) return "刚刚";
    if (minutes < 60) return `${minutes} 分钟前`;
    if (minutes < 1440) return `${Math.floor(minutes / 60)} 小时前`;
    return date.toLocaleDateString("zh-CN", { month: "short", day: "numeric" });
  }

  function render() {
    const config = state.config || {};
    const groups = state.groups || [];
    const privateChats = state.private_chats || [];
    const conversations = [...groups, ...privateChats];
    const running = active(state.service);
    text("#sidebar-service-label", running ? "服务运行中" : "服务已停止");
    $("#sidebar-service-dot").className = `status-dot ${running ? "online" : "offline"}`;
    text("#stat-service", running ? "运行中" : "已停止");
    text("#stat-service-detail", running ? "正在接收已绑定会话的消息" : "启动后恢复消息连接");
    badge("#service-badge", running ? "运行中" : "已停止", running ? "success" : "neutral");
    text("#service-detail", running ? "消息连接与对话流程正在运行。" : "配置就绪后，可以在这里启动机器人。");
    text("#service-substate", ({ running: "运行中", dead: "已停止", exited: "已退出", "auto-restart": "正在恢复", start: "启动中", stop: "停止中" })[state.service?.substate] || state.service?.substate || "—");
    text("#service-memory", number(state.service?.memory_bytes) ? `${(number(state.service.memory_bytes) / 1048576).toFixed(1)} MB` : "—");
    text("#service-restarts", number(state.service?.restarts));
    text("#stat-groups", groups.length);
    text("#stat-private", privateChats.length);
    text("#stat-private-detail", config.private_enabled ? `私聊已开启 · 上限 ${config.max_private_sessions ?? "—"}` : "私聊当前关闭");
    text("#stat-pending", conversations.reduce((sum, item) => sum + number(item.pending), 0));
    const failed = conversations.reduce((sum, item) => sum + number(item.failed), 0);
    text("#stat-failed-detail", failed ? `${failed} 个请求需要关注` : "暂无失败请求");
    text("#nav-group-count", groups.length);
    text("#nav-private-count", privateChats.length);
    text("#groups-total", groups.length);
    text("#private-total", privateChats.length);
    text("#hero-mode", `参与模式 · ${modeName(config.mode)}`);
    text("#hero-private", config.private_enabled ? "私聊 · 已开启" : "私聊 · 已关闭");
    badge("#private-enabled-badge", config.private_enabled ? "私聊已开启" : "私聊已关闭", config.private_enabled ? "success" : "neutral");
    for (const target of ["qq", "jev"]) {
      const configured = Boolean(state.credentials?.[target]);
      badge(`#ready-${target}`, configured ? "已配置" : "待配置", configured ? "success" : "warning");
      badge(`#${target}-credential-badge`, configured ? "密钥已保存" : "待配置", configured ? "success" : "warning");
    }
    for (const target of ["tinyfish", "tikhub"]) {
      const connection = config.connections?.[target];
      badge(`#${target}-credential-badge`, connection?.configured ? connection.enabled ? "已开启" : "已配置 · 已关闭" : "待授权或配置", connection?.configured && connection.enabled ? "success" : "neutral");
    }
    const codexReady = Boolean(state.login?.authenticated || state.credentials?.provider);
    badge("#ready-codex", codexReady ? "已就绪" : "待登录", codexReady ? "success" : "warning");
    badge("#provider-credential-badge", state.credentials?.provider ? "密钥已保存" : state.login?.authenticated ? "使用独立登录" : "待配置", codexReady ? "success" : "warning");
    const gateway = state.health?.qq_gateway;
    if (gateway) text("#health-detail", gateway.connected === true || gateway.ok === true ? "QQ 消息连接正常；模型测试仍由你手动触发。" : gateway.message || "消息连接状态已读取，必要时可手动测试连接。");
    text("#updated-label", `更新于 ${lastUpdated.toLocaleTimeString("zh-CN", { hour12: false })}`);
    fillForms();
    renderRecent(groups, privateChats);
    renderConversations(groups, "group");
    renderConversations(privateChats, "private");
    renderStickerScopes();
    renderLogin();
    renderSkills();
    renderMemoryGroups();
    renderIntegrationCatalogue();
    renderDocuments();
    updateWriteAccess();
  }

  function fillForms() {
    const config = state.config || {};
    const general = $("#general-form");
    if (general.dataset.dirty !== "true") {
      for (const key of ["mode", "reply_format", "private_enabled", "max_private_sessions", "quiet_ms", "max_wait_ms"]) setField(general, key, config[key]);
      for (const key of ["command_admins", "private_allowed_users"]) setField(general, key, (config[key] || []).join("\n"));
      renderPresets(config.model_presets || {});
    }
    const qq = $("#qq-form");
    if (qq.dataset.dirty !== "true") setField(qq, "app_id", config.qq_app_id);
    const jev = $("#jev-form");
    if (jev.dataset.dirty !== "true") setField(jev, "model", config.jev_model);
    const provider = $("#provider-form");
    if (provider.dataset.dirty !== "true") {
      setField(provider, "id", config.provider?.id || "openai");
      setField(provider, "base_url", config.provider?.base_url);
      populateModelPicker($("#provider-model-choice"), $("#provider-model-custom"), $("#provider-effort"), config.provider?.model || config.model_presets?.sol?.model || "", config.provider?.reasoning_effort || "high");
      providerTypeChanged();
    }
    for (const service of ["tinyfish", "tikhub"]) {
      const form = $(`#${service}-form`), connection = config.connections?.[service];
      if (form.dataset.dirty !== "true") {
        setField(form, "source", connection?.source || "codex");
        setField(form, "enabled", connection?.enabled);
      }
      integrationSourceChanged(service);
      $(`#${service}-key-clear`).hidden = !connection?.api_key_configured;
    }
    const behaviorForm = $("#behavior-form");
    if (behaviorForm.dataset.dirty !== "true" && state.behavior && behaviorForm.dataset.loaded !== JSON.stringify(state.behavior)) {
      renderBehavior(state.behavior.current);
      behaviorForm.dataset.loaded = JSON.stringify(state.behavior);
    }
    $$("select[name=mode] option").forEach(option => { if (option.value.startsWith("mode-")) option.textContent = modeName(option.value); });
    const persona = $("#persona-form");
    if (persona.dataset.dirty !== "true") {
      setField(persona, "identity", config.persona?.identity || "");
      setField(persona, "soul", config.soul || state.behavior?.soul?.effective || "");
      text("#identity-source-hint", config.persona?.files_enabled ? "当前读取 IDENTITY.md：名字、身份与角色。所有群聊和私聊共用。" : "当前身份仍包含在旧 Soul 中。确认名字与角色后保存，会建立独立的 IDENTITY.md。");
      text("#soul-source-hint", config.persona?.files_enabled ? "当前读取 SOUL.md：性格、价值取向和交流习惯。与身份分别保存，不能为空。" : "当前显示旧配置或默认人格。保存后迁入 SOUL.md；请保留已有性格内容，可将名字与角色移到上方。");
      setField(persona, "web_search", config.web_search || "disabled");
      for (const tool of toolNames) setField(persona, `tool_${tool}`, (config.enabled_tools || toolNames).includes(tool));
    }
    for (const [selector, settings] of [["#interactions-form", config.interactions], ["#sticker-settings-form", config.stickers]]) {
      const form = $(selector);
      if (form.dataset.dirty !== "true") for (const [key, value] of Object.entries(settings || {})) setField(form, key, value);
    }
    $$(".model-source").forEach(node => { node.textContent = state.model_list_source === "live" ? "已读取服务实际支持的模型与思考强度" : "当前为已配置列表，刷新后核对实际支持情况"; });
  }

  const categoryOrder = ["casual", "factual", "explanation", "comparison", "troubleshooting", "task", "emotional", "clarification"];
  const modeOrder = ["mode-1", "mode-2", "mode-3", "mode-all"];
  let archivedModeReplies = {};
  function policyField(parent, label, name, value, { multiline = false, max = 2000, min, hint } = {}) {
    const wrapper = element("label", "field");
    wrapper.append(element("span", "", label));
    const input = element(multiline ? "textarea" : "input");
    input.name = name;
    input.value = value;
    input.required = true;
    if (multiline) { input.rows = 5; input.maxLength = max; }
    else if (min !== undefined) { input.type = "number"; input.min = min; input.max = max; input.step = 1; }
    else input.maxLength = max;
    wrapper.append(input);
    if (hint) wrapper.append(element("small", "", hint));
    parent.append(wrapper);
  }
  function policyDetails(parent, title) {
    const details = element("details", "policy-details");
    details.append(element("summary", "", title));
    const content = element("div", "policy-fields");
    details.append(content);
    parent.append(details);
    return content;
  }
  function renderBehavior(policy) {
    const root = $("#behavior-editor");
    root.replaceChildren();
    archivedModeReplies = Object.fromEntries(modeOrder.map(id => [id, state?.behavior?.current?.modes?.[id]?.reply_instructions ?? policy.modes[id].reply_instructions ?? ""]));
    policyField(root, "总体回答策略", "response_instructions", policy.response_instructions, { multiline: true });
    policyField(root, "排版与气泡规则", "formatting_instructions", policy.formatting_instructions, { multiline: true, hint: "Markdown 发送格式在「连接与模型」的全局配置中选择。纯文本模式会使用普通段落与编号。" });
    root.append(element("h3", "", "按请求选择表达方式"));
    root.append(element("p", "muted", "由 Codex 结合上下文选择和组合；用户明确的详略要求优先，不额外调用 Jev。"));
    for (const id of categoryOrder) {
      const item = policy.categories[id];
      const body = policyDetails(root, item.name);
      policyField(body, "显示名称", `category_${id}_name`, item.name, { max: 40 });
      policyField(body, "适用情境与回答规则", `category_${id}_instructions`, item.instructions, { multiline: true });
    }
    root.append(element("h3", "", "参与模式：只决定何时接话"));
    root.append(element("p", "muted", "所有模式共用上方 Soul 和回答策略。这里仅设置显示名称与 Jev 的接话判断，不改变人格、语气或认真程度。"));
    for (const id of modeOrder) {
      const item = policy.modes[id];
      const body = policyDetails(root, `${id} · ${item.name}`);
      policyField(body, "显示名称", `mode_${id}_name`, item.name, { max: 40 });
      if (["mode-2", "mode-3"].includes(id)) policyField(body, "Jev 参与判断提示词", `mode_${id}_participation`, item.participation_instructions, { multiline: true, hint: "仅影响是否接话。真正 @ 机器人仍直接回应；引用机器人本身不代表必须回应。" });
      else body.append(element("p", "muted", id === "mode-1" ? "固定规则：只回应真正 @ 的消息，跳过 Jev。" : "固定规则：回应每条消息，跳过 Jev；私聊默认使用此模式。"));

    }
    if (modeOrder.some(id => archivedModeReplies[id])) {
      const archive = policyDetails(root, "旧版分模式回复文案 · 只读存档");
      archive.append(element("p", "muted", "以下旧配置仍原样保存，不再参与回答生成。保存当前规则或填入默认规则都不会删除它们；需要沿用的内容可自行整理进上方统一 Soul 或回答策略。"));
      for (const id of modeOrder.filter(id => archivedModeReplies[id])) {
        const wrapper = element("label", "field");
        wrapper.append(element("span", "", `${id} · 旧版回复文案`));
        const original = element("textarea");
        original.rows = 3;
        original.readOnly = true;
        original.value = archivedModeReplies[id];
        original.setAttribute("aria-label", `${id} 旧版回复文案（只读存档）`);
        wrapper.append(original);
        archive.append(wrapper);
      }
    }
    root.append(element("h3", "", "上下文窗口与消息容量"));
    root.append(element("p", "muted", "条数是自动附带上下文的上限，不是历史保留期限。当前请求、引用和必要状态另附；Jev 超出输入预算时会裁减旧上下文。Codex 可用历史工具检索更早消息。"));
    const grid = element("div", "form-grid");
    root.append(grid);
    for (const [id, label, max] of [["jev_messages", "Jev：之前的人类消息", 40], ["jev_bot_messages", "Jev：机器人回复气泡", 20], ["codex_messages", "Codex：最近的人类消息", 80], ["codex_bot_messages", "Codex：机器人回复气泡", 40]]) policyField(grid, label, id, state.behavior.context_windows[id], { min: 1, max });
    policyField(grid, "一次回复最多几个气泡／图片", "max_bubbles", state.behavior.max_bubbles, { min: 1, max: 3 });
    policyField(grid, "单个文本气泡最多字符数", "max_bubble_chars", state.behavior.max_bubble_chars, { min: 1, max: 2000 });
  }
  function collectBehavior(data) {
    const categories = Object.fromEntries(categoryOrder.map(id => [id, { name: data[`category_${id}_name`], instructions: data[`category_${id}_instructions`] }]));
    const modes = Object.fromEntries(modeOrder.map(id => [id, { name: data[`mode_${id}_name`], participation_instructions: data[`mode_${id}_participation`] || "", reply_instructions: archivedModeReplies[id] || "" }]));
    return { section: "behavior", behavior: { response_instructions: data.response_instructions, formatting_instructions: data.formatting_instructions, categories, modes }, context_windows: Object.fromEntries(["jev_messages", "jev_bot_messages", "codex_messages", "codex_bot_messages"].map(id => [id, number(data[id])])), max_bubbles: number(data.max_bubbles), max_bubble_chars: number(data.max_bubble_chars) };
  }

  const models = () => (state?.models || []).filter(item => item && (item.model || item.id));
  const modelId = model => String(model.model || model.id);
  function addOption(select, value, label) {
    const option = element("option", "", label);
    option.value = value;
    select.append(option);
  }
  function populateEfforts(select, model, current) {
    const metadata = models().find(item => modelId(item) === model);
    const values = metadata?.reasoning_efforts?.length ? metadata.reasoning_efforts : ["low", "medium", "high", "xhigh"];
    select.replaceChildren();
    values.forEach(value => addOption(select, value, effortNames[value] || value));
    select.value = values.includes(current) ? current : values.includes(metadata?.default_effort) ? metadata.default_effort : values[0];
  }
  function populateModelPicker(select, custom, effort, current, currentEffort) {
    select.replaceChildren();
    models().forEach(item => addOption(select, modelId(item), item.display_name || modelId(item)));
    addOption(select, "__custom__", "自定义模型名称…");
    const known = models().some(item => modelId(item) === current);
    select.value = known ? current : "__custom__";
    custom.hidden = known;
    custom.value = known ? "" : current;
    populateEfforts(effort, current, currentEffort);
    select.onchange = () => {
      custom.hidden = select.value !== "__custom__";
      populateEfforts(effort, custom.hidden ? select.value : custom.value, effort.value);
      if (!custom.hidden) custom.focus();
    };
    custom.onchange = () => populateEfforts(effort, custom.value, effort.value);
  }
  function selectedModel(select, custom) {
    const value = select.value === "__custom__" ? custom.value.trim() : select.value;
    if (!value) throw new Error("请选择模型，或填写自定义模型名称。");
    return value;
  }

  function presetRow(alias, preset) {
    const row = element("div", "preset-row");
    const aliasLabel = element("label", "field");
    aliasLabel.append(element("span", "", "别名"));
    const aliasInput = element("input");
    aliasInput.className = "preset-alias";
    aliasInput.value = alias;
    aliasInput.required = true;
    aliasInput.maxLength = 32;
    aliasInput.pattern = "[A-Za-z0-9_\\-]{1,32}";
    aliasInput.placeholder = "如 sol";
    aliasLabel.append(aliasInput);
    const modelLabel = element("label", "field");
    modelLabel.append(element("span", "", "模型"));
    const modelSelect = element("select", "preset-model");
    const custom = element("input", "preset-custom");
    custom.setAttribute("aria-label", "自定义模型名称");
    custom.placeholder = "自定义模型名称";
    modelLabel.append(modelSelect, custom);
    const effortLabel = element("label", "field");
    effortLabel.append(element("span", "", "思考强度"));
    const effort = element("select", "preset-effort");
    effortLabel.append(effort);
    populateModelPicker(modelSelect, custom, effort, preset.model, preset.reasoning_effort || "high");
    const remove = element("button", "preset-delete", "×");
    remove.type = "button";
    remove.setAttribute("aria-label", "删除此模型预设");
    remove.addEventListener("click", () => { row.remove(); $("#general-form").dataset.dirty = "true"; });
    row.append(aliasLabel, modelLabel, effortLabel, remove);
    return row;
  }
  function renderPresets(presets) {
    const editor = $("#preset-editor");
    editor.replaceChildren(...Object.entries(presets).map(([alias, preset]) => presetRow(alias, preset)));
    if (!editor.children.length) editor.append(element("p", "field-hint preset-empty", "还没有预设。添加后，可在会话中通过别名选择模型。"));
  }
  function collectPresets() {
    const result = Object.create(null);
    for (const row of $$(".preset-row")) {
      const alias = $(".preset-alias", row).value.trim();
      if (!/^[A-Za-z0-9_-]{1,32}$/u.test(alias)) throw new Error("模型别名限 1～32 位英文字母、数字、短横线或下划线。");
      if (Object.hasOwn(result, alias)) throw new Error("模型别名不能重复。");
      result[alias] = { model: selectedModel($(".preset-model", row), $(".preset-custom", row)), reasoning_effort: $(".preset-effort", row).value };
    }
    return result;
  }

  function renderRecent(groups, privateChats) {
    const items = [...groups.map(item => ({ ...item, scope: "group" })), ...privateChats.map(item => ({ ...item, scope: "private" }))];
    const timestamp = item => new Date(typeof item.last_activity === "number" && item.last_activity < 1e12 ? item.last_activity * 1000 : item.last_activity || 0).getTime() || 0;
    items.sort((a, b) => timestamp(b) - timestamp(a));
    const container = $("#recent-conversations");
    container.replaceChildren();
    if (!items.length) { container.append(emptyState("还没有会话", "先绑定测试群，或向机器人发送一条私聊消息。", true)); return; }
    items.slice(0, 4).forEach(item => {
      const row = element("div", "recent-row");
      row.append(element("span", "recent-avatar", item.scope === "group" ? "♧" : "↗"));
      const info = element("div");
      info.append(element("strong", "", item.name || (item.scope === "group" ? "群聊会话" : "私聊会话")), element("small", "", `${item.scope === "group" ? "群聊" : "私聊"} · ${number(item.message_count)} 条记录`));
      row.append(info, element("span", "", shortTime(item.last_activity)));
      container.append(row);
    });
  }

  function emptyState(title, description, compact = false) {
    const node = element("div", `empty-state${compact ? " compact-empty" : " card"}`);
    node.append(element("span", "empty-symbol", "✧"), element("strong", "", title), element("span", "", description));
    return node;
  }
  function groupProfileForm(item) {
    const form = element("form", "group-profile-form");
    form.dataset.dirty = "false";
    form.append(element("h3", "", "本群档案"), element("p", "muted", "只影响这个群的相处方式，优先于全局人设中的对应默认项；留空沿用全局。每项最多 600 字，合计 2000 字。支持 Markdown。"));
    const fields = {}, labels = {purpose: "群用途与背景", role: "AI 在群里的角色", style: "互动风格", boundaries: "相处边界"};
    const grid = element("div", "form-grid");
    for (const [key, label] of Object.entries(labels)) {
      const field = element("label", "field"), input = element("textarea");
      input.name = key; input.rows = 5; input.maxLength = 1200;
      input.value = item.group_profile?.fields?.[key] || "";
      field.append(element("span", "", label), input); grid.append(field); fields[key] = input;
    }
    const footer = element("div", "form-footer"), status = element("span", "", `版本 ${item.group_profile?.revision || "0"} · 重置聊天上下文会保留档案`);
    const save = element("button", "button primary compact", "保存群档案"); save.type = "submit"; save.dataset.write = "";
    const reset = element("button", "button secondary compact", "清空档案"); reset.type = "button"; reset.dataset.write = "";
    const reload = element("button", "text-button", "重新读取"); reload.type = "button";
    reload.addEventListener("click", async () => {
      if (form.dataset.dirty === "true" && !await confirmAction("放弃当前草稿？", "重新读取已保存的群档案。", "重新读取")) return;
      cleanForm(form); await refreshState();
    });
    const submit = async action => {
      await api("conversation", {scope: "group", id: item.id, action, expected_revision: item.group_profile?.revision || "0",
        ...(action === "profile" ? {profile: Object.fromEntries(Object.entries(fields).map(([k, input]) => [k, input.value]))} : {})});
      cleanForm(form); toast(action === "profile" ? "群档案已保存。" : "已清空群档案，恢复全局默认。"); await refreshState();
    };
    reset.addEventListener("click", () => perform(reset, async () => {
      if (await confirmAction("清空这个群的档案？", "恢复全局人设默认；称呼、记忆、聊天记录和工具授权会保留。", "清空档案")) await submit("profile_reset");
    }));
    form.addEventListener("input", () => { form.dataset.dirty = "true"; });
    form.addEventListener("submit", event => { event.preventDefault(); perform(save, async () => {
      const lengths = Object.values(fields).map(input => [...input.value.trim()].length);
      if (lengths.some(n => n > 600) || lengths.reduce((a, b) => a + b, 0) > 2000) throw new Error("每项最多 600 字，合计最多 2000 字。");
      await submit("profile");
    }); });
    footer.append(status, reload, reset, save); form.append(grid, footer); return form;
  }

  function renderConversations(items, scope) {
    const container = $(scope === "group" ? "#group-list" : "#private-list");
    if ($('form[data-dirty="true"]', container)) {
      // A draft in another form must not leave this saved profile on a stale revision.
      if (scope === "group") for (const item of items) {
        const card = [...container.children].find(node => node.dataset.conversationId === item.id);
        const profile = card && $(".group-profile-form", card);
        if (profile && profile.dataset.dirty !== "true") profile.replaceWith(groupProfileForm(item));
      }
      return;
    }
    container.replaceChildren();
    if (!items.length) {
      container.append(emptyState(scope === "group" ? "还没有绑定群聊" : "还没有私聊会话", scope === "group" ? "在上方填写群 ID，添加你的第一个测试群。" : "开启私聊后，收到的会话会出现在这里。"));
      return;
    }
    items.forEach(item => {
      const card = element("article", "card conversation-card");
      card.dataset.conversationId = item.id;
      const top = element("div", "conversation-top");
      const identity = element("div", "conversation-identity");
      const info = element("div");
      info.append(element("h3", "", item.name || (scope === "group" ? "群聊会话" : "私聊会话")));
      const identifier = element("span", "conversation-id", item.id);
      identifier.title = String(item.id || "");
      info.append(identifier);
      identity.append(element("span", "conversation-avatar", scope === "group" ? "♧" : "↗"), info);
      const status = element("span", `badge ${number(item.failed) ? "warning" : "success"}`, number(item.failed) ? "需要关注" : "已连接");
      top.append(identity, status);
      const meta = element("div", "conversation-meta");
      for (const [label, value] of [["消息记录", number(item.message_count)], ["待处理", number(item.pending)], ["等待补充", number(item.waiting_for_user)], ["待取回", number(item.awaiting_delivery)], ["失败", number(item.failed)], ["最近活动", shortTime(item.last_activity)]]) {
        const detail = element("span", "", label);
        detail.append(element("strong", "", value));
        meta.append(detail);
      }
      const form = element("form", "conversation-form");
      form.dataset.dirty = "false";
      let modeSelect;
      if (scope === "group") {
        const label = element("label", "field");
        label.append(element("span", "", "参与模式"));
        modeSelect = element("select");
        addOption(modeSelect, "", "跟随全局设置");
        ["mode-1", "mode-2", "mode-3", "mode-all"].forEach(mode => addOption(modeSelect, mode, modeName(mode)));
        modeSelect.value = item.mode || "";
        label.append(modeSelect);
        form.append(label);
      } else { form.append(element("span", "badge neutral", "私聊固定全部回应")); }
      const modelLabel = element("label", "field");
      modelLabel.append(element("span", "", "会话默认模型"));
      const modelSelect = element("select");
      addOption(modelSelect, "", "跟随全局默认");
      Object.keys(state.config?.model_presets || {}).forEach(alias => addOption(modelSelect, alias, alias));
      if (item.model_alias && ![...modelSelect.options].some(option => option.value === item.model_alias)) addOption(modelSelect, item.model_alias, `${item.model_alias}（预设已不可用）`);
      modelSelect.value = item.model_alias || "";
      modelLabel.append(modelSelect);
      const actions = element("div", "conversation-actions");
      const capabilities = element("fieldset", "tool-options conversation-capabilities");
      capabilities.append(element("legend", "", "本会话扩展授权"));
      const browserLabel = element("label"), browserInput = element("input");
      browserInput.type = "checkbox"; browserInput.checked = Boolean(item.browser_enabled);
      browserInput.disabled = !state.config.integrations?.browser?.configured;
      browserLabel.append(browserInput, element("span", "", browserInput.disabled ? "公共网页浏览（尚未配置）" : "匿名公共网页浏览与截图")); capabilities.append(browserLabel);
      const pluginInputs = new Map();
      const plugins = [...(state.config.integrations?.plugins || [])];
      for (const id of item.plugin_grants || []) {
        if (!plugins.some(plugin => plugin.id === id)) {
          plugins.push({id, name: id, unavailable: true});
        }
      }
      for (const plugin of plugins) {
        const label = element("label"), input = element("input"); input.type = "checkbox"; input.checked = (item.plugin_grants || []).includes(plugin.id);
        label.append(input, element("span", "", `${plugin.name} · ${plugin.unavailable ? "连接已关闭，授权保留" : "只读查询"}`)); capabilities.append(label); pluginInputs.set(plugin.id, input);
      }
      capabilities.append(element("small", "", "仅作用于这个会话；授权不导入个人登录。全局工具开关也须开启。"));
      const save = element("button", "button primary compact", "保存设置");
      save.type = "submit";
      save.dataset.write = "";
      const reset = element("button", "button secondary compact", "重置上下文");
      reset.type = "button";
      reset.dataset.write = "";
      reset.addEventListener("click", () => perform(reset, async () => {
        if (!await confirmAction("重置这段会话？", "下一次请求会开始新的上下文。已有消息记录会保留；有请求正在处理时暂不能重置。", "重置上下文")) return;
        await api("conversation", { scope, id: item.id, action: "reset" });
        toast("会话上下文已重置。");
        await refreshState();
      }));
      actions.append(save, reset);
      if (scope === "group") {
        const unbind = element("button", "button quiet compact danger-text", "解除绑定");
        unbind.type = "button";
        unbind.dataset.write = "";
        unbind.addEventListener("click", () => perform(unbind, async () => {
          if (!await confirmAction("解除这个群的绑定？", "机器人将不再接收这个群的新消息，已有会话记录会保留。", "解除绑定")) return;
          await api("conversation", { scope, id: item.id, action: "unbind" });
          toast("已解除群聊绑定。");
          await refreshState();
        }));
        actions.append(unbind);
      }
      form.append(modelLabel, capabilities, actions);
      form.addEventListener("input", () => { form.dataset.dirty = "true"; });
      form.addEventListener("change", () => { form.dataset.dirty = "true"; });
      form.addEventListener("submit", event => {
        event.preventDefault();
        perform(save, async () => {
          await api("conversation", { scope, id: item.id, action: "settings", mode: scope === "private" ? "mode-all" : modeSelect.value, model_alias: modelSelect.value, browser_enabled: browserInput.checked && !browserInput.disabled, plugin_grants: [...pluginInputs].filter(([, input]) => input.checked).map(([id]) => id) });
          cleanForm(form);
          toast("会话设置已保存。");
          await refreshState();
        });
      });
      card.append(top, meta, form);
      if (scope === "group") card.append(groupProfileForm(item));
      container.append(card);
    });
  }

  function renderLogin() {
    const login = state.login || {};
    const authenticated = Boolean(login.authenticated);
    text("#login-status-title", authenticated ? "机器人已登录" : "机器人尚未登录");
    const loginLabel = login.kind === "api_key" ? " API Key" : login.kind === "chatgpt" ? "独立 ChatGPT 账号" : "独立账号";
    text("#login-status-detail", authenticated ? `当前使用${loginLabel}连接。` : "你可以使用已有登录，或为机器人完成一次设备授权。");
    badge("#login-badge", authenticated ? "授权有效" : "等待登录", authenticated ? "success" : "warning");
    text("#personal-available", login.personal_available ? "已找到可用于授权的个人登录。" : "暂未发现可复用的个人登录，请使用设备授权。");
    if (login.device !== undefined) device = login.device;
    renderDevice();
  }
  function renderDevice() {
    const visible = device && (device.url || device.code) && !["cancelled", "completed", "success"].includes(device.status);
    $("#device-card").hidden = !visible;
    if (!visible) return;
    text("#device-code", device.code || "等待生成");
    text("#device-status", ({ pending: "打开授权页面并输入以下代码。", waiting: "正在等待你完成授权。", expired: "本次授权已过期，请重新开始设备登录。", failed: "本次设备授权未完成，请重新开始。" })[device.status] || "打开授权页面，输入设备代码后返回这里检查状态。");
    const link = $("#device-url");
    try {
      const url = new URL(device.url);
      if (url.protocol !== "https:") throw new Error("invalid_url");
      link.href = url.href;
      link.hidden = false;
    } catch { link.removeAttribute("href"); link.hidden = true; }
    $("#copy-device-code").disabled = !device.code;
  }

  function renderSkills() {
    const skills = state.skills || [];
    text("#skills-total", skills.length);
    const container = $("#skill-list");
    container.replaceChildren();
    if (!skills.length) { container.append(emptyState("还没有 Skill", "在右侧写下第一份专门的文字指令。", true)); return; }
    skills.forEach(skill => {
      const card = element("article", "skill-item");
      const heading = element("div", "skill-item-heading");
      heading.append(element("h3", "", skill.name), element("span", `badge ${skill.enabled ? "success" : "neutral"}`, skill.enabled ? "全局已启用" : "全局未启用"));
      if (skill.read_only) {
        const version = skill.package?.valid ? skill.package.commit.slice(0, 12) : "版本记录待检查";
        const scope = [skill.enabled ? "全局" : "", ...(skill.scopes || [])].filter(Boolean).join("、") || "暂无启用会话";
        card.append(heading, element("p", "", skill.description || "完整受限技能包"),
          element("p", "muted", `只读技能包 · ${version} · ${skill.package?.files || 0} 个文件`),
          element("p", "muted", `来源：${skill.package?.source || "未取得有效来源记录"}`),
          element("p", "muted", `启用范围：${scope}。请在对应聊天使用 /skill on|off ${skill.name}。`));
        container.append(card);
        return;
      }
      const actions = element("div", "button-row");
      const edit = element("button", "text-button", "编辑");
      edit.addEventListener("click", () => {
        const form = $("#skill-form");
        if (form.dataset.dirty === "true") { confirmAction("放弃尚未保存的编辑？", "切换后，当前编辑框中尚未保存的文字将被替换。", "切换编辑").then(ok => { if (ok) loadSkill(skill); }); }
        else loadSkill(skill);
      });
      const toggle = element("button", "text-button", skill.enabled ? "全局停用" : "全局启用");
      toggle.dataset.write = "";
      toggle.addEventListener("click", () => perform(toggle, async () => {
        await api("config", { section: "skill", action: "save", name: skill.name, description: skill.description, instructions: skill.instructions, enabled: !skill.enabled });
        toast(skill.enabled ? "Skill 已全局停用，单个会话的选择保持独立。" : "Skill 已全局启用。");
        await refreshState();
      }));
      const remove = element("button", "text-button danger-text", "删除");
      remove.dataset.write = "";
      remove.addEventListener("click", () => perform(remove, async () => {
        if (!await confirmAction("删除这个 Skill？", "这会移除机器人独立保存的这份文字指令，并应用配置。", "删除 Skill")) return;
        await api("config", { section: "skill", action: "delete", name: skill.name });
        if (editingSkill === skill.name) loadSkill(null);
        toast("Skill 已删除。");
        await refreshState();
      }));
      actions.append(edit, toggle, remove);
      card.append(heading, element("p", "", skill.description || "未填写简介"), actions);
      container.append(card);
    });
  }
  function loadSkill(skill) {
    const form = $("#skill-form");
    editingSkill = skill?.name || null;
    for (const name of ["name", "description", "instructions"]) setField(form, name, skill?.[name] || "");
    form.elements.name.readOnly = Boolean(skill);
    setField(form, "enabled", skill ? skill.enabled : true);
    cleanForm(form);
    text("#skill-editor-title", skill ? "编辑 Skill" : "新建 Skill");
    form.elements.name.focus();
  }

  function memoryScope() { return { target: memoryGroupId, private: memoryPrivate }; }
  function memoryKey(kind, id) { return `${memoryPrivate ? "private" : "group"}:${memoryGroupId}:${kind}:${String(id)}`; }
  const memoryCategories = { profile: "个人资料", relationship: "人物关系", group_preference: "会话偏好", short_term: "短期信息" };
  const memoryStatuses = { buffered: "缓冲待归档", notice_pending: "旧版待审", reviewing: "旧版审核中", applied: "已入库", rejected: "已拒绝" };
  const memoryCertainty = { explicit: "明确表达", inferred: "推断", uncertain: "尚不确定" };
  function renderMemorySettings() {
    const form = $("#memory-settings-form");
    if (form.dataset.dirty === "true") return;
    const settings = state?.config?.memory || { enabled: true, automatic_capture: true, archive_hour: 4, archive_minute: 0, archive_every_days: 1, short_term_days: 14 };
    for (const [key, value] of Object.entries(settings)) if (form.elements.namedItem(key)) setField(form, key, value);
  }
  function renderMemoryGroups() {
    renderMemorySettings();
    const groups = memoryPrivate ? state.private_chats || [] : state.groups || [];
    const select = $("#memory-group-select");
    const previous = memoryGroupId;
    if (!groups.some(group => group.id === memoryGroupId)) memoryGroupId = groups[0]?.id || "";
    const signature = JSON.stringify([memoryPrivate, groups.map(group => [group.id, group.name])]);
    if (select.dataset.signature !== signature) {
      select.replaceChildren();
      if (!groups.length) select.append(new Option(memoryPrivate ? "尚无已接入私聊" : "尚未绑定群聊", ""));
      groups.forEach((group, index) => select.append(new Option(group.name || `${memoryPrivate ? "私聊" : "群聊"} ${index + 1}`, group.id)));
      select.dataset.signature = signature;
    }
    select.value = memoryGroupId;
    if (previous !== memoryGroupId) {
      memoryData = null;
      memoryRequest++;
      memoryLoading = false;
      $("#memory-error").hidden = true;
      if (currentView === "memory" && memoryGroupId) loadMemory();
    }
    if (!memoryData) renderMemory();
    else { renderMemorySchedule(); updateWriteAccess(); }
  }
  function memoryTime(value) {
    if (!value) return "未提供";
    const raw = typeof value === "string" && !/^\d+$/u.test(value) ? value : number(value) < 1e12 ? number(value) * 1000 : number(value);
    const date = new Date(raw);
    return Number.isFinite(date.getTime()) ? date.toLocaleString("zh-CN", { timeZone: "Asia/Shanghai", hour12: false }) : "未提供";
  }
  function memoryAudit(parent, label, rows) {
    if (!rows?.length) return;
    const details = element("details", "memory-audit");
    details.append(element("summary", "", `${label} · ${rows.length} 条`));
    for (const row of rows) {
      const block = element("div", "memory-audit-row");
      if (typeof row === "string") block.append(element("p", "memory-entry-meta", row));
      else {
        const labels = { revision: "版本", content: "内容", changed_at: "修改时间", applied_at: "入库时间", timestamp: "来源时间", applied_by: "入库方式", message_ref: "来源消息", message_hash: "来源校验", author: "来源成员", category: "分类", subjects: "涉及成员", certainty: "确定性", source_kind: "来源方式", draft_id: "草稿编号", draft_revision: "草稿版本", expires_at: "到期时间", operation: "操作", status: "状态", selection_reason: "保留原因" };
        for (const [key, value] of Object.entries(row || {})) {
          if (value === null || value === undefined) continue;
          let rendered = key.endsWith("_at") || key === "timestamp" ? memoryTime(value) : typeof value === "object" ? JSON.stringify(value, null, 2) : String(value);
          block.append(element("strong", "memory-entry-meta", labels[key] || key), element("p", "memory-audit-value", rendered));
        }
      }
      details.append(block);
    }
    parent.append(details);
  }
  function memoryEntry(entry, kind) {
    const draft = kind === "draft";
    const key = memoryKey(kind, entry.id);
    const item = element("article", "memory-entry");
    item.dataset.memoryId = String(entry.id);
    item.dataset.memoryKind = kind;
    const heading = element("div", "memory-entry-heading");
    heading.append(element("strong", "memory-subject", `${memoryCategories[entry.category] || entry.category || "未分类"} · #${entry.id}`), element("span", `badge ${draft ? "warning" : "success"}`, draft ? memoryStatuses[entry.status] || entry.status : ({ active: "正式有效", superseded: "已被更正", expired: "已过期" })[entry.status] || "正式记录"));
    item.append(heading, element("p", "memory-entry-text", entry.content));
    item.append(element("div", "memory-entry-meta", `版本 ${entry.revision} · ${memoryCertainty[entry.certainty] || entry.certainty || "未注明确定性"} · ${entry.source_kind === "automatic" ? "自动筛选" : entry.source_kind === "user_requested" ? "用户明确提出" : entry.source_kind || "未注明来源"}`));
    item.append(element("div", "memory-entry-meta", `涉及成员：${(entry.subjects || []).join("、") || "当前会话整体"} · 有效期：${entry.expires_at ? memoryTime(entry.expires_at) : "未设置到期"}`));
    item.append(element("div", "memory-entry-meta", `创建 ${memoryTime(entry.created_at)} · 更新 ${memoryTime(entry.updated_at)}`));
    if (!draft && entry.superseded_by) item.append(element("div", "memory-entry-meta", `后续更正记录：#${entry.superseded_by}；旧内容仅作历史保留。`));
    if (draft) {
      const rewrite = entry.operation?.action === "rewrite";
      item.append(element("div", "memory-entry-meta", rewrite ? `改写正式记录 #${entry.operation.record_id} · 预期版本 ${entry.operation.revision ?? entry.target_revision ?? "未知"}，需要明确确认。` : `追加记录${entry.operation?.supersedes ? ` · 补充/更正 #${entry.operation.supersedes}${entry.target_revision ? ` · 目标版本 ${entry.target_revision}` : ""}` : ""} · ${({ retain: "建议保留", context_only: "仅作上下文", discard: "不建议保留" })[entry.disposition] || entry.disposition || "待筛选"}`));
      if (entry.selection_reason) item.append(element("p", "memory-entry-meta", `筛选依据：${entry.selection_reason}`));
    }
    memoryAudit(item, "来源证据", entry.evidence);
    memoryAudit(item, "修订历史", entry.history);
    const editable = !draft || !["applied", "rejected"].includes(entry.status);
    if (!editable) return item;
    const actions = element("div", "button-row memory-entry-actions");
    const edit = element("button", "text-button", draft ? "编辑草稿" : "提出更正");
    edit.type = "button";
    edit.addEventListener("click", () => {
      if (!memoryEdits.has(key)) memoryEdits.set(key, { content: entry.content, revision: String(entry.revision), dirty: false });
      renderMemory();
    });
    actions.append(edit);
    if (draft) {
      const reject = element("button", "text-button danger-text", "拒绝草稿");
      reject.type = "button"; reject.dataset.write = "";
      reject.addEventListener("click", () => memoryMutate(reject, "reject", entry, kind));
      const apply = element("button", "text-button", "确认入库");
      apply.type = "button"; apply.dataset.write = "";
      apply.addEventListener("click", () => memoryMutate(apply, "apply", entry, kind));
      actions.append(reject, apply);
    }
    item.append(actions);
    const editing = memoryEdits.get(key);
    if (editing) {
      const editor = element("div", "memory-inline-editor");
      const label = element("label", "field");
      label.append(element("span", "", draft ? "编辑草稿内容" : "更正内容（先保存为改写草稿）"));
      const input = element("textarea"); input.rows = 5; input.maxLength = 500; input.value = editing.content;
      input.setAttribute("aria-label", `${draft ? "草稿" : "正式记录"} ${entry.id} 的编辑内容`);
      input.addEventListener("input", () => { editing.content = input.value; editing.dirty = true; });
      label.append(input); editor.append(label);
      editor.append(element("p", "field-hint", `以版本 ${editing.revision} 为基础${editing.revision !== String(entry.revision) ? `；当前已为版本 ${entry.revision}，请核对变化后再保存。` : "；只编辑内容，来源、分类与确定性保持原记录。"}`));
      const controls = element("div", "button-row");
      const save = element("button", "button secondary", draft ? "保存草稿" : "创建更正草稿"); save.type = "button"; save.dataset.write = "";
      save.addEventListener("click", () => memoryMutate(save, draft ? "edit" : "correct", { ...entry, revision: editing.revision, content: editing.content }, kind));
      const cancel = element("button", "text-button", "收起编辑"); cancel.type = "button";
      cancel.addEventListener("click", async () => {
        if (editing.dirty && !await confirmAction("放弃尚未保存的文字？", "记录保持原样，只清除这次编辑。", "放弃编辑")) return;
        memoryEdits.delete(key); renderMemory();
      });
      controls.append(save, cancel);
      if (editing.revision !== String(entry.revision)) {
        const rebase = element("button", "text-button", "核对并采用最新版本"); rebase.type = "button";
        rebase.addEventListener("click", async () => {
          if (!await confirmAction("保留你的编辑，改用最新版本？", `当前记录版本 ${entry.revision}，当前内容：\n${entry.content}\n\n你的未保存文字：\n${editing.content}\n\n仅更新本次编辑的基准版本；点击保存后才写入。`, "采用最新版本")) return;
          editing.revision = String(entry.revision); renderMemory();
        });
        controls.append(rebase);
      }
      editor.append(controls); item.append(editor);
    }
    return item;
  }
  function renderMemorySchedule() {
    const ledger = memoryData?.ledger;
    const settings = state?.config?.memory || memoryData?.settings;
    const time = `${String(settings?.archive_hour ?? 4).padStart(2, "0")}:${String(settings?.archive_minute ?? 0).padStart(2, "0")}`;
    text("#memory-schedule-title", `北京时间 ${time} · 每 ${settings?.archive_every_days || 1} 天归档`);
    text("#memory-schedule-detail", "Asia/Shanghai。只追加符合条件的缓冲草稿，不自动批准正式改写，不发送周期提醒。");
    badge("#memory-schedule-badge", !settings?.enabled ? "记忆已停用" : active(state?.service) ? "按设置维护" : "服务未运行", settings?.enabled && active(state?.service) ? "success" : "neutral");
    text("#memory-last-maintained", ledger?.maintenance?.updated_at ? `最近维护状态更新：${memoryTime(ledger.maintenance.updated_at)}` : "暂无维护记录。");
  }
  function renderMemory() {
    const data = memoryData, ledger = data?.ledger;
    const records = ledger?.records || [], drafts = ledger?.drafts || [];
    const buffered = drafts.filter(row => !["applied", "rejected"].includes(row.status));
    const legacy = data?.legacy?.entries || [];
    text("#memory-facts", data ? records.length + legacy.length : "—");
    text("#memory-pending", data ? buffered.length : "—");
    text("#memory-confirmed", data ? records.reduce((total, row) => total + (row.history || []).length, 0) : "—");
    text("#memory-revision", ledger?.revision ?? "—");
    $("#memory-content").setAttribute("aria-busy", String(memoryLoading || memoryMaintaining));
    renderMemorySchedule();
    text("#memory-group-label", data?.name || "请选择群聊或私聊；每个会话的内容独立保存。");
    const authority = $("#memory-authority-list"), changes = $("#memory-change-list");
    authority.replaceChildren(); changes.replaceChildren();
    text("#memory-changes-count", data ? `${drafts.length} 条` : "— 条");
    if (!data) {
      authority.append(emptyState(memoryLoading ? "正在读取记忆" : "选择会话后加载", "正式账本与草稿分开显示。", true));
      changes.append(emptyState("草稿尚未加载", "未保存的本地编辑会保留。", true));
    } else {
      if (!records.length && !legacy.length) authority.append(emptyState("还没有正式记忆", "从有价值、可溯源的内容开始。", true));
      records.forEach(row => authority.append(memoryEntry(row, "record")));
      legacy.forEach(row => {
        const item = element("article", "memory-entry");
        item.append(element("strong", "memory-subject", `旧版正式记录 · ${row.subject || "成员"}`), element("p", "memory-entry-text", row.text), element("p", "memory-entry-meta", `版本 ${row.revision} · 保留原确认记录，只读展示。`));
        authority.append(item);
      });
      if (!drafts.length) changes.append(emptyState("暂时没有草稿", "自动筛选或明确要求记住的内容会进入缓冲。", true));
      drafts.forEach(row => changes.append(memoryEntry(row, "draft")));
      memoryAudit(changes, "旧版变更档案（保留原流程，仅查看）", data.legacy?.changes);
    }
    const layout = $("#memory-layout-list"); layout.replaceChildren();
    const labels = { canonical: "权威账本 · canonical", legacy_authority: "旧版正式记忆 · canonical", drafts_json: "结构化草稿 · buffer", drafts_markdown: "可读草稿 · derived", revisions_json: "修订审计 · archive" };
    for (const [key, label] of Object.entries(labels)) if (data?.layout?.[key]) {
      const row = element("div", "memory-file-row"); row.append(element("span", "", label), element("code", "", data.layout[key])); layout.append(row);
    }
    for (const category of data?.layout?.categories || []) {
      const row = element("div", "memory-file-row"); row.append(element("span", "", `${memoryCategories[category.category] || category.category} · derived`), element("code", "", category.path)); layout.append(row);
    }
    for (const section of data?.logical_layout || []) {
      const row = element("div", "memory-file-row"); row.append(element("span", "", section.role === "workspace" ? "会话工作区 · workspace" : "聊天历史 · archive"), element("code", "", section.path)); layout.append(row);
      layout.append(element("p", "field-hint", section.note));
    }
    layout.append(element("p", "field-hint", "会话工作区 workspace 与聊天历史 archive 独立维护；模型可用历史工具检索，不将它们当作正式记忆文件。"));
    const reviews = $("#memory-review-list"); reviews.replaceChildren();
    if (ledger) { memoryAudit(reviews, "维护状态", [ledger.maintenance]); memoryAudit(reviews, "既有审核记录（仅历史，不发送新提醒）", ledger.reviews); }
    updateWriteAccess();
  }
  async function loadMemory() {
    if (!memoryGroupId || !csrfToken || currentView !== "memory" || memoryMaintaining) return;
    const scope = memoryScope(), request = ++memoryRequest;
    memoryLoading = true; $("#memory-error").hidden = true; renderMemory();
    try {
      const data = await api("memory", { action: "view", ...scope });
      if (request !== memoryRequest || scope.target !== memoryGroupId || scope.private !== memoryPrivate) return;
      memoryData = data;
      text("#memory-loaded-at", `读取于 ${memoryTime(Date.now())}（北京时间）；未保存文字会保留，保存时核对原版本。`);
    } catch (error) {
      if (request !== memoryRequest) return;
      text("#memory-error", `${error.message}${memoryData ? " 保留上次记录和未保存编辑，当前内容可能已过时。" : " 请刷新重试。"}`);
      $("#memory-error").hidden = false;
    } finally { if (request === memoryRequest) { memoryLoading = false; renderMemory(); } }
  }
  async function memoryMutate(button, action, entry, kind) {
    return perform(button, async () => {
      if (memoryLoading || memoryMaintaining || !memoryGroupId) return;
      const key = entry ? memoryKey(kind, entry.id) : null;
      const editing = key ? memoryEdits.get(key) : null;
      if (action === "apply" && editing?.dirty) throw new Error("请先保存编辑后的草稿，再核对内容并确认入库。");
      if (["edit", "correct"].includes(action) && (!entry.content.trim() || [...entry.content].length > 500)) throw new Error("记忆内容须为 1–500 字。");
      if (action === "apply" && !await confirmAction(entry.operation?.action === "rewrite" ? "确认改写正式记忆？" : "确认将这条草稿入库？", `草稿 #${entry.id} · 版本 ${entry.revision}\n${entry.operation?.action === "rewrite" ? `将改写正式记录 #${entry.operation.record_id}，预期版本 ${entry.operation.revision ?? entry.target_revision ?? "未知"}。` : "将进入当前会话的正式记忆。"}\n\n实际内容：\n${entry.content}\n\n请确认来源和不确定性；此操作保留审计历史。`, "确认入库")) return;
      if (action === "reject" && !await confirmAction("拒绝这条草稿？", `草稿 #${entry.id} · 版本 ${entry.revision}。不会改动正式记忆；拒绝结果会保留。`, "拒绝草稿")) return;
      const scope = memoryScope(); const payload = { action, ...scope };
      if (entry) { payload.id = String(entry.id); payload.revision = String(entry.revision); }
      if (["edit", "correct"].includes(action)) payload.content = entry.content;
      memoryMaintaining = true; updateWriteAccess();
      try {
        const data = await api("memory", payload);
        if (key) memoryEdits.delete(key);
        if (scope.target === memoryGroupId && scope.private === memoryPrivate) memoryData = data;
        $("#memory-error").hidden = true;
        toast(({ edit: "草稿已保存。", correct: "已创建改写草稿；正式记录尚未改变，请核对后明确确认入库。", reject: "草稿已拒绝。", apply: "已明确确认入库，并保留修订历史。", archive: "已归档符合条件的草稿；正式改写仍需明确确认。" })[action]);
      } catch (error) {
        text("#memory-error", `${error.message} 未保存编辑仍保留；如记录版本变化，请刷新核对。`); $("#memory-error").hidden = false;
        throw error;
      } finally { memoryMaintaining = false; renderMemory(); }
    });
  }

  function renderDocuments() {
    const settings = state.config.documents || {};
    text("#documents-status", settings.enabled && settings.authorized ? "已授权" : settings.authorized ? "已暂停" : "待授权");
    const catalogue = $("#documents-capabilities");
    catalogue.replaceChildren(...(settings.document_types || []).map(type => {
      const row = element("div", "connection-row");
      const content = element("div");
      content.append(element("strong", "", type.name), element("small", "", type.operations));
      if (type.note) content.append(element("small", "", type.note));
      row.append(content, element("span", "badge neutral", type.level));
      return row;
    }));
    if (!catalogue.children.length) catalogue.append(element("p", "field-hint", "当前服务未提供文档能力列表，请刷新页面或检查服务版本。"));
    const select = $("#documents-group");
    const groups = state.groups || [];
    const signature = JSON.stringify(groups.map(group => [group.id, group.name]));
    if (select.dataset.signature !== signature) {
      const current = select.value;
      select.replaceChildren(...groups.map(group => new Option(group.name, group.id)));
      if (groups.some(group => group.id === current)) select.value = current;
      select.dataset.signature = signature;
    }
    const binding = settings.bindings?.[select.value] || "";
    if ($("#documents-bind-form").dataset.dirty !== "true") $("#documents-folder").value = binding;
    text("#documents-binding-note", binding ? `当前目录：${binding}` : "此群尚未绑定目录");
    const native = settings.qq_native || {};
    const groupCode = native.groups?.[select.value] || "";
    if ($("#documents-native-form").dataset.dirty !== "true") $("#documents-native-code").value = groupCode;
    text("#documents-native-status", native.active_groups?.includes(select.value) ? "所选群已启用" : groupCode ? "所选群已暂停" : "所选群未绑定");
    text("#documents-native-binding", groupCode ? `已绑定 QQ 群号：${groupCode}。${native.credential_saved ? "独立网页登录态已保存；有效性在使用时检查。" : "请重新扫码登录。"}` : "请填写上方所选群的实际 QQ 群号；首次需先绑定腾讯文档目录。");
    renderDocumentNativeFlow(native.flow);
  }
  function renderDocumentNativeFlow(flow, qrDataUrl = null) {
    documentNativeFlow = flow || null;
    $("#documents-native-login").hidden = !flow;
    const image = $("#documents-native-qr");
    image.hidden = !flow?.has_qr;
    if (!flow) { image.removeAttribute("src"); return; }
    const labels = {starting: "正在打开 QQ 官方登录页面…", waiting_scan: "请使用群成员的 QQ 账号扫码，并在手机上确认。", authorized: "扫码与群权限检查通过。点击“保存登录并启用”后生效。", saved: "登录已保存。", cancelled: "本次登录已取消。", expired: "二维码已过期，请重新登录。", failed: flow.message || "登录未完成，请重新登录。"};
    text("#documents-native-login-status", labels[flow.state] || "登录状态暂不可用。");
    if (flow.has_qr && /^[a-f0-9]{32}$/.test(flow.flow_id)) {
      if (qrDataUrl && /^data:image\/png;base64,[A-Za-z0-9+/=]+$/.test(qrDataUrl) && qrDataUrl.length <= 700000) {
        if (image.src !== qrDataUrl) image.src = qrDataUrl;
        image.dataset.flowId = flow.flow_id;
      } else if (image.dataset.flowId !== flow.flow_id) image.removeAttribute("src");
      image.hidden = !image.hasAttribute("src");
    } else { image.removeAttribute("src"); delete image.dataset.flowId; }
    $("#documents-native-finish").hidden = flow.state !== "authorized";
    $("#documents-native-cancel").hidden = !["starting", "waiting_scan", "authorized"].includes(flow.state);
  }
  async function pollDocumentNativeFlow() {
    if (!documentNativeFlow || documentNativePolling) return;
    documentNativePolling = true;
    const flowId = documentNativeFlow.flow_id;
    try {
      const result = await api("documents", {action: "native_auth_status", flow_id: flowId});
      if (documentNativeFlow?.flow_id === flowId) renderDocumentNativeFlow(result.flow, result.qr_data_url);
    } finally { documentNativePolling = false; }
  }
  let documentFolderCursor = 0;
  async function loadDocumentFolders(reset) {
    if (reset) { documentFolderCursor = 0; $("#documents-folder-list").replaceChildren(); }
    const result = await api("documents", {action: "folders", start: documentFolderCursor});
    for (const folder of result.data.folders || []) {
      const row = element("div", "connection-row");
      row.append(element("span", "", folder.title || folder.id));
      const button = element("button", "button quiet", "选择此目录"); button.type = "button";
      button.addEventListener("click", () => { $("#documents-folder").value = folder.id; $("#documents-bind-form").dataset.dirty = "true"; });
      row.append(button); $("#documents-folder-list").append(row);
    }
    documentFolderCursor = result.data.next_start;
    $("#documents-folders-more").hidden = result.data.finish !== false;
    if (!$("#documents-folder-list").children.length) $("#documents-folder-list").append(element("p", "field-hint", "当前页没有目录；可继续读取，或在下面创建专用目录。"));
  }
  async function documentAction(body) {
    const result = await api("documents", body);
    if (result.message) toast(result.message);
    await refreshState(); return result;
  }

  function renderIntegrationCatalogue() {
    const root = $("#integration-catalogue"); root.replaceChildren();
    const browser = state?.config?.integrations?.browser;
    const row = element("div", "connection-row"), info = element("div");
    info.append(element("strong", "", "公共网页浏览"), element("small", "", "匿名渲染、文字读取和截图"));
    row.append(element("span", "connection-logo", "↗"), info, element("span", `badge ${browser?.configured ? "success" : "neutral"}`, browser?.configured ? "已配置 · 按会话授权" : "尚未配置")); root.append(row);
    for (const plugin of state?.config?.integrations?.plugins || []) {
      const row = element("div", "connection-row"), info = element("div");
      info.append(element("strong", "", plugin.name), element("small", "", `${(plugin.tools || []).length} 个已登记查询工具`));
      row.append(element("span", "connection-logo", "⌘"), info, element("span", "badge neutral", "只读 · 按会话授权")); root.append(row);
    }
  }

  async function loadCapabilities() {
    if (capabilityLoading || capabilityRefreshing || !csrfToken) return;
    capabilityLoading = true;
    const refresh = $("#capabilities-refresh"); refresh.disabled = true;
    text("#capabilities-status", "正在读取本机能力与已缓存的账号应用目录…");
    try {
      capabilityData = await api("config", {section: "capabilities", action: "catalog"});
      renderCapabilities();
    } catch (error) {
      text("#capabilities-status", error.message);
    } finally { capabilityLoading = false; refresh.disabled = false; }
  }

  async function refreshHostedCapabilities() {
    if (capabilityRefreshing || !csrfToken) return;
    capabilityRefreshing = true;
    const button = $("#capabilities-refresh"); button.disabled = true;
    updateWriteAccess();
    text("#capabilities-status", "正在检查账号应用；空闲机器人会短暂停止，检查后自动恢复。此操作可能需要约两分钟。");
    try {
      let job = await api("config", {section: "capabilities", action: "refresh"});
      while (job.state === "running") {
        await new Promise(resolve => setTimeout(resolve, 2000));
        job = await api("config", {section: "capabilities", action: "status"});
      }
      if (job.catalog) { capabilityData = job.catalog; renderCapabilities(); }
      if (job.state === "failed") throw new Error(job.message || "账号应用目录刷新未完成。");
      toast(job.message || "账号应用目录已刷新。");
    } catch (error) { text("#capabilities-status", error.message); toast(error.message, true); }
    finally {
      capabilityRefreshing = false; button.disabled = false; updateWriteAccess();
      await refreshState();
      await loadCapabilities();
    }
  }

  function renderCapabilities() {
    const root = $("#capability-list"); root.replaceChildren();
    const data = capabilityData;
    if (!data) return;
    text("#capabilities-status", (data.notices || []).join(" ") || "目录已刷新。个人账号应用默认停用；每个会话独立选择授权。");
    const native = element("article", "capability-row");
    native.append(element("h3", "", data.native?.name || "Codex 通用工作环境"),
      element("p", "field-hint", data.native?.description || "启动时自动准备，沿用原生工具和工作流程。"));
    const settings = element("button", "text-button", "调整通用工具与搜索 ↗");
    settings.addEventListener("click", () => showView("persona")); native.append(settings); root.append(native);
    const kinds = {app: "账号应用", mcp: "MCP", skill: "Skill", hooks: "命令 Hooks"};
    for (const item of data.capabilities || []) {
      const card = element("article", "capability-row"), heading = element("div", "capability-heading");
      heading.append(element("h3", "", item.name), element("span", `badge ${item.enabled ? "success" : "neutral"}`,
        item.registered ? (item.enabled ? "已启用" : "已导入 · 停用") : kinds[item.kind] || "能力"));
      card.append(heading, element("p", "field-hint", item.description));
      if (item.personal_account) card.append(element("p", "field-hint", "涉及账号能力或本机指令，需主动选择；不会自动授予任何会话。"));
      if (item.kind === "hooks") card.append(element("p", "field-hint", "启用后，命令会在机器人原生任务中按事件运行；只导入配置，不复制个人登录或命令依赖的脚本。"));
      if (!item.available) {
        card.append(element("p", "field-hint", item.reason || "当前不可迁移。"));
        if (!item.registered) { root.append(card); continue; }
      }
      const toolInputs = [], buttons = element("div", "button-row");
      let manualTools = null;
      if (!item.registered && ["app", "mcp"].includes(item.kind)) {
        if (item.tools?.length) {
          const details = element("details", "capability-tools"), list = element("fieldset", "tool-options");
          details.open = true; details.append(element("summary", "", "选择要导入的工具"));
          for (const tool of item.tools) {
            const label = element("label"), input = element("input"); input.type = "checkbox"; input.value = tool.name;
            input.checked = false; label.append(input, element("span", "", tool.name)); list.append(label); toolInputs.push(input);
          }
          details.append(list); card.append(details);
        } else {
          const label = element("label", "field"); label.append(element("span", "", "允许的工具名称"));
          manualTools = element("textarea"); manualTools.rows = 2; manualTools.maxLength = 4200;
          manualTools.placeholder = "例如 search, fetch_content";
          label.append(manualTools, element("small", "", "填写已核对的精确名称，最多 32 个；导入后可检查连接。")); card.append(label);
        }
      } else if (item.tools?.length) {
        const details = element("details", "capability-tools");
        details.append(element("summary", "", `${item.tools.length} 个已登记工具`), element("p", "field-hint", item.tools.map(tool => tool.name).join(" · ")));
        card.append(details);
      }
      const actionButton = (label, action, style = "secondary") => {
        const button = element("button", `button ${style} compact`, label); button.dataset.write = "";
        button.addEventListener("click", () => perform(button, async () => {
          const body = {id: item.id, action};
          if (action === "import" && ["app", "mcp"].includes(item.kind)) {
            body.allowed_tools = manualTools ? listInput(manualTools.value) : toolInputs.filter(input => input.checked).map(input => input.value);
            if (!body.allowed_tools.length) throw new Error("请先选择或填写要导入的工具。");
          }
          const result = await api("config", {section: "capabilities", ...body}); toast(result.message || "能力设置已保存。");
          await refreshState(); await loadCapabilities();
        })); buttons.append(button);
      };
      if (!item.registered) actionButton("导入所选能力", "import", "primary");
      else if (item.kind === "skill") {
        const button = element("button", "text-button", "在 Skills 中管理 ↗");
        button.addEventListener("click", () => showView("persona")); buttons.append(button);
      } else if (item.kind === "hooks") {
        if (item.enabled || item.available) actionButton(item.enabled ? "停用 Hooks" : "启用 Hooks", item.enabled ? "disable" : "enable");
      } else {
        if (item.enabled || item.available) actionButton(item.enabled ? "停用连接" : "启用连接", item.enabled ? "disable" : "enable");
        if (item.enabled) actionButton("检查连接", "check");
        const grant = element("button", "text-button", "设置会话授权 ↗");
        grant.addEventListener("click", () => showView("groups")); buttons.append(grant);
      }
      card.append(buttons); root.append(card);
    }
    if (!data.capabilities?.length) root.append(element("p", "field-hint", "暂未发现可选的本机连接或账号应用。可继续使用自动准备的通用能力。"));
    updateWriteAccess();
  }

  const stickerApi = body => api("config", {section: "sticker_library", ...body});
  const stickerLabels = {cute:"可爱",goofy:"搞怪",meme:"梗图",ordinary:"普通",animal:"动物",person:"人物",cartoon:"卡通",text:"文字",teasing:"调侃",happy:"开心",sad:"悲伤",speechless:"无语",amused:"搞笑",calm:"平静",angry:"生气",surprised:"惊讶",confused:"困惑",affectionate:"亲昵",encouraging:"鼓励",tired:"疲惫",embarrassed:"尴尬",celebratory:"庆祝",neutral:"中性"};
  const stickerSeries = () => stickerData?.series || [{id:"default",name:"默认系列"}];
  const seriesName = id => stickerSeries().find(series => series.id === id)?.name || id || "默认系列";
  function seriesOptions(select, all = false) {
    const selected = select.value; select.replaceChildren();
    if (all) addOption(select, "", "全部系列");
    for (const series of stickerSeries()) addOption(select, series.id, series.name);
    select.value = [...select.options].some(option => option.value === selected) ? selected : all ? "" : "default";
  }
  function taxonomyOptions(key, values) {
    const root = $(key === "styles" ? "#sticker-style-options" : "#sticker-emotion-options");
    const selected = new Set($$("input:checked", root).map(input => input.value)); root.replaceChildren();
    for (const value of values) {
      const label = element("label"), input = element("input"); input.type = "checkbox"; input.name = key; input.value = value; input.checked = selected.has(value);
      label.append(input, element("span", "", stickerLabels[value] || value)); root.append(label);
    }
  }
  async function loadStickers() {
    if (!csrfToken || stickerLoading || currentView !== "stickers") return;
    stickerLoading = true; $("#stickers-error").hidden = true;
    try {
      const response = await stickerApi({action: "list"}); stickerData = response.catalogue;
      stickerTaxonomy = {styles: response.styles || [], emotions: response.emotions || []};
      stickerDefaultInstructions = response.default_usage_instructions || "";
      $("#sticker-policy-default").disabled = !stickerDefaultInstructions;
      renderStickers(); renderStickerScopes();
    } catch (error) { text("#stickers-error", error.message); $("#stickers-error").hidden = false; }
    finally { stickerLoading = false; }
  }
  function renderStickers() {
    const root = $("#sticker-list"), assets = stickerData?.assets || []; root.replaceChildren(); text("#stickers-count", assets.length);
    seriesOptions($("#sticker-series-filter"), true); seriesOptions($("#sticker-form").elements.series_id);
    const collection = $("#sticker-series-list"); collection.replaceChildren();
    for (const series of stickerSeries()) collection.append(element("span", "badge neutral", `${series.name} · ${assets.filter(asset => (asset.series_id || "default") === series.id).length}`));
    taxonomyOptions("styles", stickerTaxonomy.styles); taxonomyOptions("emotions", stickerTaxonomy.emotions);
    const filtered = assets.filter(asset => !$("#sticker-series-filter").value || (asset.series_id || "default") === $("#sticker-series-filter").value);
    if (!filtered.length) root.append(emptyState(assets.length ? "这个系列还没有表情" : "共享素材库还没有表情", "可批量导入；聊天中的新表情识别后会进入默认系列。", true));
    for (const asset of filtered) {
      const pending = asset.metadata_status === "pending";
      const row = element("article", "sticker-row"), heading = element("div", "sticker-heading");
      heading.append(element("strong", "", asset.name), element("span", `badge ${pending ? "warning" : asset.enabled ? "success" : "neutral"}`, pending ? "等待语义整理" : asset.enabled ? "可选用" : "已停用"));
      row.append(heading, element("p", "muted", asset.summary || asset.description || "尚无画面说明"), element("small", "field-hint sticker-asset-meta", `${seriesName(asset.series_id)} · ${asset.animated ? `动图 · ${asset.frames} 帧` : "静态图片"} · ${Math.ceil(number(asset.bytes) / 1024)} KiB`));
      row.append(element("small", "field-hint sticker-asset-meta", `风格：${(asset.styles || []).map(value => stickerLabels[value] || value).join(" / ") || "待标注"} · 情绪：${(asset.emotions || []).map(value => stickerLabels[value] || value).join(" / ") || "待标注"}`));
      const actions = element("div", "button-row"), edit = element("button", "text-button", "预览与编辑"), remove = element("button", "text-button danger-text", "全局移除");
      edit.type = remove.type = "button"; remove.dataset.write = "";
      edit.addEventListener("click", () => perform(edit, async () => {
        if (stickerImporting) throw new Error("请等待当前批次导入完成。");
        if ($("#sticker-form").dataset.dirty === "true" && !await confirmAction("切换表情？", "当前尚未保存的说明将被替换。", "切换")) return;
        await selectSticker(asset);
      }));
      remove.addEventListener("click", () => perform(remove, async () => {
        if (!await confirmAction("从共享库移除这个表情？", `所有群聊与私聊都将无法再选择“${asset.name}”。若只是不希望某个会话使用，请调整下方会话偏好。已发出聊天和待投递快照不会删除。`, "全局移除")) return;
        await stickerApi({action: "remove", id: asset.id, revision: String(asset.revision)});
        if (editingSticker?.id === asset.id) clearStickerEditor();
        await loadStickers(); toast("已从共享库移除。");
      }));
      actions.append(edit, remove); row.append(actions);
      const move = element("div", "sticker-move"), select = element("select"), button = element("button", "button secondary compact", "移动系列");
      select.setAttribute("aria-label", `${asset.name} 的目标系列`); seriesOptions(select); select.value = asset.series_id || "default";
      button.type = "button"; button.dataset.write = "";
      button.addEventListener("click", () => perform(button, async () => {
        const response = await stickerApi({action:"move",id:asset.id,revision:String(asset.revision),series_id:select.value});
        if (editingSticker?.id === asset.id && $("#sticker-form").dataset.dirty !== "true" && response.asset) await selectSticker(response.asset);
        await loadStickers(); toast("已移动到指定系列。");
      }));
      move.append(select,button); row.append(move); root.append(row);
    }
    updateWriteAccess();
  }
  function clearStickerEditor() {
    editingSticker = null; stickerPreviewRequest++;
    const form = $("#sticker-form"); form.reset(); cleanForm(form); form.elements.series_id.disabled = false;
    $("#sticker-file-field").hidden = false; $("#sticker-preview").hidden = true; $("#sticker-preview").removeAttribute("src");
    text("#sticker-editor-title", "添加表情"); text("#sticker-version", "新素材"); text("#sticker-preview-note", "");
  }
  async function selectSticker(asset) {
    editingSticker = {...asset}; const request = ++stickerPreviewRequest;
    const form = $("#sticker-form");
    for (const key of ["name", "description", "summary", "when_to_use", "avoid_when", "enabled"]) setField(form, key, asset[key]);
    setField(form, "series_id", asset.series_id || "default"); form.elements.series_id.disabled = true;
    for (const key of ["styles", "emotions"]) $$(`input[name="${key}"]`, form).forEach(input => {input.checked = (asset[key] || []).includes(input.value);});
    setField(form, "tags", (asset.tags || []).join("，")); cleanForm(form);
    $("#sticker-file-field").hidden = true; $("#sticker-preview").hidden = true;
    text("#sticker-editor-title", "编辑表情"); text("#sticker-version", `版本 ${asset.revision}`);
    text("#sticker-preview-note", "正在读取预览；移动系列请使用左侧列表中的操作。");
    const response = await stickerApi({action: "preview", id: asset.id, revision: String(asset.revision)});
    if (request !== stickerPreviewRequest) return;
    if (!/^data:image\/png;base64,[A-Za-z0-9+/=]+$/u.test(response.preview_data_url || "")) throw new Error("预览格式不正确。");
    $("#sticker-preview").src = response.preview_data_url; $("#sticker-preview").hidden = false;
    text("#sticker-preview-note", asset.animated ? "动图以采样帧理解，原动画保留发送。移动系列请使用列表中的操作。" : "静态预览；发送使用原图。移动系列请使用列表中的操作。");
  }

  function renderStickerScopes() {
    const select = $("#sticker-preference-scope"), selected = select.value; select.replaceChildren(); addOption(select,"","选择一个会话");
    for (const [privateChat, items] of [[false,state?.groups || []],[true,state?.private_chats || []]]) {
      for (const item of items) addOption(select,JSON.stringify({target:item.id,private:privateChat}),`${privateChat ? "私聊" : "群聊"} · ${item.name || item.id}`);
    }
    if ([...select.options].some(option => option.value === selected)) select.value = selected;
    else {stickerPreferenceRequest++;$("#sticker-preference-list").replaceChildren();}
  }
  async function loadStickerPreferences() {
    const selected = $("#sticker-preference-scope").value, request = ++stickerPreferenceRequest;
    const root = $("#sticker-preference-list"); root.replaceChildren();
    if (!selected) return;
    const scope = JSON.parse(selected), result = await stickerApi({action:"preferences",...scope});
    if (request !== stickerPreferenceRequest) return;
    const targets = new Set();
    for (const item of result.scores || []) {
      for (const signal of [item.asset,item.series,...(item.styles || [])]) {
        if (!signal?.target) continue;
        const key = JSON.stringify(signal.target); if (targets.has(key)) continue; targets.add(key);
        const dimension = signal.target.dimension, id = signal.target.value;
        const name = dimension === "asset" ? (stickerData?.assets || []).find(asset => asset.id === id)?.name || id : dimension === "series" ? seriesName(id) : stickerLabels[id] || id;
        const row = element("article","sticker-row"), title = element("div","sticker-heading");
        const rule = signal.explicit_rule?.action;
        title.append(element("strong","",`${{asset:"表情",series:"系列",style:"风格"}[dimension] || dimension} · ${name}`),element("span",`badge ${signal.excluded ? "warning" : "neutral"}`,signal.excluded ? "本会话不使用" : `偏好 ${number(signal.level).toFixed(2)} / 5`));
        row.append(title,element("p","muted",`近期窗口 ${number(signal.window_count)} 次 · 最近使用样本 ${number(signal.rolling_count)} 次 · 最后使用 ${signal.last_used_at ? shortTime(number(signal.last_used_at) * 1000) : "尚无记录"}`));
        row.append(element("small","field-hint",rule ? `明确设置：${rule === "prefer" ? "更喜欢" : "不使用"}；明确规则优先于自动计算。` : "由人的使用记录计算；机器人自己发图不会强化偏好。"));
        const actions = element("div","button-row");
        for (const [operation,label] of [["prefer","更喜欢"],["avoid","不再使用"],["reset","恢复自动"]]) {
          const button = element("button","text-button",label); button.type="button"; button.dataset.write="";
          button.addEventListener("click",()=>perform(button,async()=>{await stickerApi({action:"preference",...scope,dimension,value:id,operation});await loadStickerPreferences();toast("仅本会话的表情偏好已更新。");})); actions.append(button);
        }
        row.append(actions);root.append(row);
      }
    }
    if (!targets.size) root.append(emptyState("暂无可计算的表情偏好","导入素材或在聊天中使用表情后，这里会显示表情、系列和风格的使用记录。",true));
    updateWriteAccess();
  }

  function showView(name, updateHash = true) {
    if (!Object.hasOwn(views, name)) name = "overview";
    currentView = name;
    $$(".view").forEach(view => { view.hidden = view.id !== `view-${name}`; });
    $$("[data-view]").forEach(button => {
      const selected = button.dataset.view === name;
      button.classList.toggle("selected", selected);
      if (selected) button.setAttribute("aria-current", "page");
      else button.removeAttribute("aria-current");
    });
    text("#page-title", views[name][0]);
    text("#breadcrumb-current", views[name][0]);
    text("#page-subtitle", views[name][1]);
    document.title = `${views[name][0]} · ClawMingle`;
    if (updateHash) history.replaceState(null, "", `#${name}`);
    window.scrollTo({ top: 0, behavior: "instant" });
    if (name === "memory") loadMemory();
    if (name === "stickers") loadStickers();
    if (name === "workflows" && !capabilityData) loadCapabilities();
  }

  function confirmAction(title, message, accept = "继续") {
    if (modalResolve) return Promise.resolve(false);
    text("#confirm-title", title);
    text("#confirm-message", message);
    text("#confirm-accept", accept);
    $("#confirm-dialog").showModal();
    return new Promise(resolve => { modalResolve = resolve; });
  }
  function finishConfirm(value) {
    $("#confirm-dialog").close();
    const resolve = modalResolve;
    modalResolve = null;
    resolve?.(value);
  }

  function providerTypeChanged() {
    const custom = $("#provider-form").elements.id.value === "custom";
    $("#provider-url-field").hidden = !custom;
    $("#provider-form").elements.base_url.required = custom;
  }
  function integrationSourceChanged(service) {
    const form = $(`#${service}-form`), source = form.elements.source.value;
    $(`#${service}-key-field`).hidden = source !== "api_key";
    text(`#${service}-source-hint`, source === "codex"
      ? state?.config?.connections?.[service]?.codex_available ? "已有连接可复用，无需填写新密钥。" : "现有授权尚未就绪，请先完成对应登录或 MCP 配置。"
      : "独立密钥仅保存在本机，留空保留；已保存值不会回填。");
  }
  function saveForm(id, build, success) {
    const form = $(id);
    form.addEventListener("submit", event => {
      event.preventDefault();
      perform($('button[type="submit"]', form), async () => {
        const body = build(fields(form), form);
        await api("config", body);
        if (body.section === "skill") {
          editingSkill = body.name;
          setField(form, "name", body.name);
          form.elements.name.readOnly = true;
          text("#skill-editor-title", "编辑 Skill");
        }
        $$('input[type="password"]', form).forEach(input => { input.value = ""; });
        cleanForm(form);
        toast(success || "设置已保存，服务将在空闲时应用配置。");
        await refreshState();
      });
    });
  }

  for (const [name, label] of Object.entries({task_update: "长任务开始说明与进展", sticker_lookup: "表情库检索、多帧预览与偏好抽选", sticker_manage: "表情收藏与会话偏好；共享素材编辑仅管理员"})) {
    const row = element("label"), input = element("input"); input.type = "checkbox"; input.name = `tool_${name}`;
    row.append(input, element("span", "", label)); $("#persona-form .tool-options").append(row);
  }
  $$("[data-view]").forEach(button => button.addEventListener("click", () => showView(button.dataset.view)));
  $("#capabilities-refresh").addEventListener("click", refreshHostedCapabilities);
  $$("[data-go]").forEach(button => button.addEventListener("click", () => showView(button.dataset.go)));
  window.addEventListener("hashchange", () => showView(location.hash.slice(1), false));
  $("#confirm-cancel").addEventListener("click", () => finishConfirm(false));
  $("#confirm-accept").addEventListener("click", () => finishConfirm(true));
  $("#confirm-dialog").addEventListener("cancel", event => { event.preventDefault(); finishConfirm(false); });
  $("#confirm-dialog").addEventListener("close", () => { if (modalResolve) finishConfirm(false); });
  $$('form:not(.conversation-form)').forEach(form => {
    form.addEventListener("input", () => { form.dataset.dirty = "true"; });
    form.addEventListener("change", () => { form.dataset.dirty = "true"; });
  });
  $("#theme-toggle").addEventListener("click", () => {
    const theme = document.documentElement.dataset.theme === "dark" ? "light" : "dark";
    document.documentElement.dataset.theme = theme;
    try { localStorage.setItem("clawmingle-theme", theme); } catch { /* Theme choice need not persist. */ }
    const label = theme === "dark" ? "切换浅色模式" : "切换深色模式";
    $("#theme-toggle").setAttribute("aria-label", label);
    $("#theme-toggle").title = label;
  });
  try { document.documentElement.dataset.theme = localStorage.getItem("clawmingle-theme") || (matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light"); } catch { /* Keep the document default. */ }
  $("#refresh-state").addEventListener("click", () => perform($("#refresh-state"), async () => { if (!csrfToken) await bootstrap(); await refreshState(true); }));
  for (const action of ["start", "stop", "restart"]) {
    const button = $(`#service-${action}`);
    button.addEventListener("click", () => perform(button, async () => {
      if (action !== "start" && !await confirmAction(action === "stop" ? "停止 ClawMingle？" : "重启 ClawMingle？", "机器人连接会暂时中断。正在处理的请求可能需要重新核对，建议先等待当前对话完成。", action === "stop" ? "停止服务" : "重启服务")) return;
      await api("service", { action });
      toast(({ start: "已发送启动请求。", stop: "已发送停止请求。", restart: "已发送重启请求。" })[action]);
      await refreshState();
    }));
  }
  $$('[data-check]').forEach(button => button.addEventListener("click", () => perform(button, async () => {
    const result = await api("check", { target: button.dataset.check });
    toast(result.message || (result.ok ? "检查通过。" : "检查未通过，请核对配置。"), result.ok === false);
    await refreshState();
  })));
  $("#provider-form").elements.id.addEventListener("change", providerTypeChanged);
  $("#add-preset").addEventListener("click", () => {
    if ($$(".preset-row").length >= 16) { toast("最多配置 16 个模型预设。", true); return; }
    $(".preset-empty")?.remove();
    $("#preset-editor").append(presetRow("", { model: models()[0] ? modelId(models()[0]) : state?.config?.provider?.model || "", reasoning_effort: "high" }));
    $("#general-form").dataset.dirty = "true";
  });
  saveForm("#general-form", (data, form) => {
    if (number(data.quiet_ms) > number(data.max_wait_ms)) throw new Error("短句收集间隔不能超过最长等待时间。");
    return { section: "general", reply_format: data.reply_format || "text", mode: data.mode, private_enabled: form.elements.private_enabled.checked, max_private_sessions: number(data.max_private_sessions), quiet_ms: number(data.quiet_ms), max_wait_ms: number(data.max_wait_ms), private_allowed_users: listInput(data.private_allowed_users), command_admins: listInput(data.command_admins), model_presets: collectPresets() };
  });
  saveForm("#qq-form", data => ({ section: "qq", app_id: data.app_id.trim(), app_secret: data.app_secret }));
  saveForm("#jev-form", data => ({ section: "jev", model: data.model.trim(), api_key: data.api_key }));
  for (const service of ["tinyfish", "tikhub"]) {
    const form = $(`#${service}-form`);
    form.elements.source.addEventListener("change", () => integrationSourceChanged(service));
    saveForm(`#${service}-form`, data => ({section: service, source: data.source, enabled: form.elements.enabled.checked,
      api_key: data.source === "api_key" ? data.api_key : ""}), "连接配置已保存，各空间授权独立保留。");
    const clear = $(`#${service}-key-clear`);
    clear.addEventListener("click", () => perform(clear, async () => {
      if (!await confirmAction("清除独立 API Key？", "仅清除机器人的独立密钥。使用该密钥的连接将关闭，已有 Codex 授权和各空间的选择保留。", "清除密钥")) return;
      await api("config", {section: service, source: state.config.connections[service].source,
        enabled: state.config.connections[service].enabled, remove_api_key: true});
      form.elements.api_key.value = "";
      cleanForm(form);
      toast("独立密钥已清除。");
      await refreshState();
    }));
  }
  saveForm("#provider-form", data => ({ section: "provider", id: data.id, base_url: data.id === "custom" ? data.base_url.trim() : "", api_key: data.api_key, model: selectedModel($("#provider-model-choice"), $("#provider-model-custom")), reasoning_effort: data.reasoning_effort }));
  saveForm("#memory-settings-form", (data, form) => ({ section: "memory", enabled: form.elements.enabled.checked, automatic_capture: form.elements.automatic_capture.checked, ...Object.fromEntries(["archive_hour", "archive_minute", "archive_every_days", "short_term_days"].map(key => [key, number(data[key])])) }), "记忆设置已保存并应用。");
  saveForm("#behavior-form", collectBehavior, "对话规则与上下文窗口已保存并应用。");
  $("#behavior-reset").addEventListener("click", async () => {
    if (!state?.behavior || !await confirmAction("填入默认表达与模式规则？", "将替换当前表单中的回答策略、参与提示词和名称；统一 Soul、旧版文案存档、上下文窗口与容量保留。点击保存后才会生效。", "填入默认")) return;
    renderBehavior(state.behavior.defaults);
    $("#behavior-form").dataset.dirty = "true";
  });
  saveForm("#persona-form", data => ({ section: "persona", identity: data.identity, soul: data.soul, web_search: data.web_search, enabled_tools: toolNames.filter(tool => $("#persona-form").elements.namedItem(`tool_${tool}`).checked) }));
  $("#interactions-form").elements.feedback_mode.addEventListener("change", event => {
    const form = $("#interactions-form");
    const presets = {quiet: {heartbeat_interval_seconds: 0}, standard: {progress_interval_seconds: 45, heartbeat_interval_seconds: 90}, detailed: {progress_interval_seconds: 20, heartbeat_interval_seconds: 60}};
    for (const [key, value] of Object.entries(presets[event.target.value] || {})) setField(form, key, value);
  });
  saveForm("#interactions-form", (data, form) => {
    const heartbeat = number(data.heartbeat_interval_seconds);
    if (heartbeat !== 0 && heartbeat < 30) throw new Error("状态提醒间隔至少为 30 秒，或填 0 关闭。");
    const total = number(data.long_timeout_seconds);
    if (total !== 0 && total < 60) throw new Error("长任务总时限至少为 60 秒，或填 0 不设总时限。");
    return {section: "interactions", enabled: form.elements.enabled.checked, feedback_mode: data.feedback_mode,
      ...Object.fromEntries(["ack_delay_seconds", "heartbeat_interval_seconds", "long_timeout_seconds", "tool_calls", "long_max_bubbles", "long_bubble_chars", "max_progress_messages", "progress_interval_seconds", "workspace_timeout_seconds"].map(key => [key, number(data[key])]))};
  }, "任务进展设置已保存，与参与模式分别生效。");
  $("#sticker-policy-default").addEventListener("click", () => {
    const form = $("#sticker-settings-form");
    form.elements.usage_instructions.value = stickerDefaultInstructions;
    form.dataset.dirty = "true";
  });
  saveForm("#sticker-settings-form", (data, form) => ({section: "stickers", enabled: form.elements.enabled.checked, automatic: form.elements.automatic.checked, auto_collect: form.elements.auto_collect.checked, cooldown_seconds: number(data.cooldown_seconds), usage_instructions: data.usage_instructions}), "表情包策略已保存。");
  $("#stickers-refresh").addEventListener("click", () => perform($("#stickers-refresh"), loadStickers));
  $("#sticker-new").addEventListener("click", async () => {
    if ($("#sticker-form").dataset.dirty === "true" && !await confirmAction("添加另一张表情？", "当前尚未保存的编辑将被清空。", "继续")) return;
    if (stickerImporting) return;
    clearStickerEditor();
  });
  $("#sticker-series-filter").addEventListener("change", renderStickers);
  $("#sticker-series-form").addEventListener("submit", event => {
    event.preventDefault(); const form = event.currentTarget;
    perform($('button[type="submit"]',form),async()=>{await stickerApi({action:"series_create",series_id:form.elements.id.value.trim(),name:form.elements.name.value.trim()});form.reset();cleanForm(form);await loadStickers();toast("共享系列已建立。");});
  });
  $("#sticker-preference-scope").addEventListener("change",()=>perform(null,loadStickerPreferences));
  $("#sticker-preference-refresh").addEventListener("click",()=>perform($("#sticker-preference-refresh"),loadStickerPreferences));
  $("#sticker-form").addEventListener("submit", event => {
    event.preventDefault(); const form = event.currentTarget;
    perform($('button[type="submit"]', form), async () => {
      const data = fields(form), base = {name:data.name.trim(),tags:listInput(data.tags),description:data.description,summary:data.summary,styles:$$('input[name="styles"]:checked',form).map(input=>input.value),emotions:$$('input[name="emotions"]:checked',form).map(input=>input.value),when_to_use:data.when_to_use,avoid_when:data.avoid_when,enabled:form.elements.enabled.checked};
      if (base.emotions.length > 6) throw new Error("情绪标签最多选择 6 项。");
      if (editingSticker) {
        if (!base.name) throw new Error("请填写表情名称。");
        const response = await stickerApi({...base,action:"edit",id:editingSticker.id,revision:String(editingSticker.revision)});
        cleanForm(form); await loadStickers(); if (response.asset) await selectSticker(response.asset); toast("表情说明已保存。"); return;
      }
      const files = [...form.elements.file.files];
      if (!files.length || files.length > 100) throw new Error("请选择 1 至 100 张图片，每张最多 2 MiB。");
      const report = [], selectedSeries = form.elements.series_id.value; let added=0, duplicates=0, failures=0;
      stickerImporting=true;
      try {
        for (const file of files) {
          try {
            if (!file.size || file.size > 2 * 1024 * 1024) throw new Error("文件为空或超过 2 MiB");
            const encoded = await new Promise((resolve,reject)=>{const reader=new FileReader();reader.onload=()=>resolve(String(reader.result).split(",")[1]);reader.onerror=()=>reject(new Error("读取文件失败"));reader.readAsDataURL(file);});
            const name = files.length === 1 && base.name ? base.name : [...file.name.replace(/\.[^.]+$/u,"")].slice(0,60).join("") || "新表情";
            const result=await stickerApi({...base,name,action:"import",series_id:selectedSeries,data_base64:encoded});
            if (result.duplicate === true || result.created === false) {duplicates++;report.push(`重复：${file.name}（保留已有系列与说明）`);}
            else {added++;report.push(`已导入：${file.name}`);}
          } catch (error) {failures++;report.push(`未完成或待核实：${file.name} · ${error.message}`);}
          text("#sticker-import-result",`已处理 ${report.length} / ${files.length}\n${report.join("\n")}`);$("#sticker-import-result").hidden=false;
        }
      } finally {stickerImporting=false;}
      text("#sticker-import-result",`本批次：入库 ${added}，重复 ${duplicates}，未完成或待核实 ${failures}。\n${report.join("\n")}\n重复素材不会自动移组；需要时在列表中移动。网络中断项请刷新核对后重试，内容去重会保留已入库素材。`);
      if (!failures) {cleanForm(form);form.elements.file.value="";}
      await loadStickers();toast(`批次处理完成：入库 ${added}，重复 ${duplicates}，待核实 ${failures}。`,failures>0);
    });
  });

  saveForm("#skill-form", data => ({ section: "skill", action: "save", name: data.name.trim(), description: data.description.trim(), instructions: data.instructions, enabled: $("#skill-form").elements.enabled.checked }), "Skill 已保存并应用。");
  $("#skill-new").addEventListener("click", async () => {
    if ($("#skill-form").dataset.dirty === "true" && !await confirmAction("放弃尚未保存的编辑？", "新建将清空当前编辑框，已有保存的 Skill 不受影响。", "新建 Skill")) return;
    loadSkill(null);
  });
  $("#bind-form").addEventListener("submit", event => {
    event.preventDefault();
    const form = event.currentTarget;
    perform($('button[type="submit"]', form), async () => {
      await api("conversation", { scope: "group", id: form.elements.id.value.trim(), action: "bind" });
      form.reset();
      cleanForm(form);
      toast("群聊已绑定。");
      await refreshState();
    });
  });
  for (const [id, action] of [["login-reuse", "reuse"], ["login-logout", "logout"], ["login-device-start", "device_start"], ["login-device-cancel", "device_cancel"], ["login-device-poll", "device_poll"]]) {
    const button = $(`#${id}`);
    button.addEventListener("click", () => perform(button, async () => {
      if (action === "reuse" && !await confirmAction("将已有登录授权给机器人？", "仅同步所有者的登录凭据到独立机器人，聊天记录和个人设置不会复制。机器人将使用这个账号连接模型服务。", "确认授权")) return;
      if (action === "logout" && !await confirmAction("退出机器人的登录？", "这只移除机器人独立保存的登录，不影响你的个人 Codex 登录。未配置 API Key 时，机器人将暂时无法回复。", "退出机器人登录")) return;
      const result = await api("login", { action });
      if (result.device !== undefined) device = result.device;
      else if (result.url || result.code) device = result;
      if (action === "device_cancel" || action === "logout") device = null;
      renderDevice();
      toast(result.message || ({ reuse: "登录授权已更新。", logout: "机器人已退出登录。", device_start: "设备登录已开始，请完成页面上的授权。", device_cancel: "已取消本次设备授权。", device_poll: "登录状态已检查。" })[action]);
      await refreshState();
    }));
  }
  $("#copy-device-code").addEventListener("click", () => perform($("#copy-device-code"), async () => {
    if (!device?.code) return;
    if (!navigator.clipboard) throw new Error("当前浏览器不支持自动复制，请手动选择设备代码。");
    await navigator.clipboard.writeText(String(device.code));
    toast("设备代码已复制。");
  }));
  $("#memory-kind-select").addEventListener("change", event => {
    memoryPrivate = event.target.value === "private";
    memoryGroupId = ""; memoryData = null; memoryRequest++; memoryLoading = false;
    renderMemoryGroups();
  });
  $("#memory-group-select").addEventListener("change", event => {
    memoryGroupId = event.target.value; memoryData = null;
    text("#memory-loaded-at", "正在读取所选会话的记忆；其他会话的未保存编辑仍保留。");
    loadMemory();
  });
  $("#memory-refresh").addEventListener("click", () => loadMemory());
  $("#memory-maintain").addEventListener("click", () => memoryMutate($("#memory-maintain"), "archive"));
  $("#documents-auth-start").addEventListener("click", () => perform($("#documents-auth-start"), async () => {
    const result = await api("documents", {action: "auth_start"});
    const url = new URL(result.url); if (url.origin !== "https://docs.qq.com") throw new Error("授权地址无效。");
    $("#documents-auth-url").href = url.href; $("#documents-auth-url").hidden = false;
    toast("授权链接已生成，请在五分钟内打开并登录腾讯文档。");
  }));
  $("#documents-auth-finish").addEventListener("click", () => perform($("#documents-auth-finish"), async () => {
    await documentAction({action: "auth_finish"}); $("#documents-auth-url").hidden = true; $("#documents-auth-url").removeAttribute("href");
  }));
  $("#documents-disable").addEventListener("click", () => perform($("#documents-disable"), () => documentAction({action: "disable"})));
  $("#documents-token-form").addEventListener("submit", event => { event.preventDefault(); const form = event.currentTarget;
    perform($("button", form), async () => { await documentAction({action: "token", token: form.elements.token.value}); form.reset(); form.dataset.dirty = "false"; });
  });
  $("#documents-group").addEventListener("change", () => { $("#documents-folder").value = state.config.documents?.bindings?.[$("#documents-group").value] || ""; $("#documents-native-form").dataset.dirty = "false"; renderDocuments(); });
  $("#documents-bind-form").addEventListener("submit", event => { event.preventDefault(); const form = event.currentTarget;
    perform($("button[type=submit]", form), async () => { await documentAction({action: "bind", group: form.elements.group.value, folder: form.elements.folder.value.trim()}); form.dataset.dirty = "false"; renderDocuments(); });
  });
  $("#documents-unbind").addEventListener("click", () => perform($("#documents-unbind"), async () => {
    await documentAction({action: "unbind", group: $("#documents-group").value}); $("#documents-bind-form").dataset.dirty = "false"; renderDocuments();
  }));
  $("#documents-folders").addEventListener("click", () => perform($("#documents-folders"), () => loadDocumentFolders(true)));
  $("#documents-folders-more").addEventListener("click", () => perform($("#documents-folders-more"), () => loadDocumentFolders(false)));
  $("#documents-create-folder-form").addEventListener("submit", event => { event.preventDefault(); const form = event.currentTarget;
    perform($("button", form), async () => { const result = await documentAction({action: "create_folder", title: form.elements.title.value.trim()});
      $("#documents-folder").value = result.data.file_id; $("#documents-bind-form").dataset.dirty = "true"; form.dataset.dirty = "false"; toast("目录已创建，请保存与当前群的绑定。"); });
  });
  $("#documents-native-form").addEventListener("submit", event => { event.preventDefault(); const form = event.currentTarget;
    perform($("button[type=submit]", form), async () => {
      if (state.config.documents?.qq_native?.credential_saved && !await confirmAction("重新登录 QQ 网页账号？", "保存成功后会替换全部群共用的独立网页登录态。新账号须能访问全部已绑定群；扫码失败或取消时原登录态保持可用。", "继续扫码")) return;
      const result = await api("documents", {action: "native_auth_start", group: $("#documents-group").value, group_code: form.elements.group_code.value.trim()});
      renderDocumentNativeFlow(result.flow);
    });
  });
  $("#documents-native-check").addEventListener("click", () => perform($("#documents-native-check"), pollDocumentNativeFlow));
  $("#documents-native-finish").addEventListener("click", () => perform($("#documents-native-finish"), async () => {
    if (!documentNativeFlow) return;
    await documentAction({action: "native_auth_finish", flow_id: documentNativeFlow.flow_id});
    $("#documents-native-form").dataset.dirty = "false"; renderDocuments();
  }));
  $("#documents-native-cancel").addEventListener("click", () => perform($("#documents-native-cancel"), async () => {
    if (!documentNativeFlow) return;
    const result = await api("documents", {action: "native_auth_cancel", flow_id: documentNativeFlow.flow_id});
    renderDocumentNativeFlow(result.flow);
  }));
  for (const action of ["enable", "disable"]) $("#documents-native-" + action).addEventListener("click", () =>
    perform($("#documents-native-" + action), () => documentAction({action: "native_" + action})));
  $("#documents-native-unbind").addEventListener("click", () => perform($("#documents-native-unbind"), async () => {
    if (!await confirmAction("解除所选群的列表登记？", "后续创建的文档不会再自动登记到这个群。已有群在线文档和腾讯文档目录保持不变。", "解除登记")) return;
    await documentAction({action: "native_unbind", group: $("#documents-group").value});
    $("#documents-native-form").dataset.dirty = "false"; renderDocuments();
  }));
  setInterval(() => { if (!document.hidden && currentView === "documents" && ["starting", "waiting_scan"].includes(documentNativeFlow?.state)) pollDocumentNativeFlow().catch(error => toast(error.message, true)); }, 3000);
  document.addEventListener("visibilitychange", () => { if (!document.hidden) refreshState(); });
  window.addEventListener("beforeunload", event => {
    if ($$('form[data-dirty="true"]').length || [...memoryEdits.values()].some(edit => edit.dirty)) { event.preventDefault(); event.returnValue = ""; }
  });
  showView(location.hash.slice(1), false);
  updateWriteAccess();
  bootstrap().then(async () => { await refreshState(); if (currentView === "workflows") loadCapabilities(); });
  setInterval(() => { if (!document.hidden) refreshState(); }, 8000);
})();
