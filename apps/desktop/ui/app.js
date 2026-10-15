const tauri = window.__TAURI__;
const invoke = tauri.core.invoke;
const connection = document.querySelector("#connection");
const connectButton = document.querySelector("#connect");
const accountMessage = document.querySelector("#message");
const environment = document.querySelector("#environment");
const agentStatus = document.querySelector("#agent-status");
const secretSetup = document.querySelector("#secret-setup");
const clientSecretInput = document.querySelector("#client-secret");
const saveSecretButton = document.querySelector("#save-secret");
const taskList = document.querySelector("#task-list");
const taskMessage = document.querySelector("#task-message");
const historyList = document.querySelector("#history-list");
const historyMessage = document.querySelector("#history-message");
const taskEditor = document.querySelector("#task-editor");
const taskForm = document.querySelector("#task-form");
const editorMessage = document.querySelector("#editor-message");
const logList = document.querySelector("#log-list");
const settingsMessage = document.querySelector("#settings-message");
let clientSecretConfigured = false;
let accountConnected = false;
let tasks = [];
let editingTask = null;
let applicationSettings = null;

function setMessage(element, value, success = false) {
  element.textContent = value || "";
  element.classList.toggle("success", success);
}

function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function formatDate(value) {
  if (!value) return "—";
  const date = typeof value === "number" ? new Date(value) : new Date(`${value.replace(" ", "T")}Z`);
  return Number.isNaN(date.getTime()) ? String(value) : date.toLocaleString();
}

function showConnected(email) {
  accountConnected = true;
  connection.classList.add("connected");
  connection.textContent = email ? `已连接 Google 账户：${email}` : "已连接 Google 账户";
  connectButton.disabled = !clientSecretConfigured;
  connectButton.textContent = clientSecretConfigured ? "重新授权 Google Drive" : "请先安全保存客户端密钥";
}

async function refreshAccountStatus() {
  try {
    const status = await invoke("google_drive_connection_status");
    environment.textContent = status.test_environment ? "测试环境" : "正式环境";
    clientSecretConfigured = status.client_secret_configured;
    secretSetup.hidden = clientSecretConfigured;
    if (status.connected) {
      showConnected(status.email);
    } else {
      accountConnected = false;
      connection.classList.remove("connected");
      connection.textContent = "尚未连接 Google 账户";
      connectButton.disabled = !clientSecretConfigured;
      connectButton.textContent = clientSecretConfigured ? "连接 Google Drive" : "请先安全保存客户端密钥";
    }
  } catch (error) {
    connection.textContent = "无法读取 RustBackup 授权状态";
    setMessage(accountMessage, String(error));
  }
}

async function refreshAgentStatus() {
  try {
    const status = await invoke("agent_status");
    agentStatus.textContent = status.active_tasks > 0
      ? `后台 Agent 运行中 · ${status.active_tasks} 项任务`
      : "后台 Agent 运行中 · 空闲";
    agentStatus.classList.add("running");
    agentStatus.title = `运行 ${status.uptime_seconds} 秒`;
  } catch {
    agentStatus.textContent = "后台 Agent 未连接";
    agentStatus.classList.remove("running");
    agentStatus.title = "桌面端会尝试启动独立 Agent；请检查运行日志";
  }
}

function taskState(view) {
  const activeManualRun = view.manual_runs.find((run) => run.status === "queued" || run.status === "running");
  const latest = view.recent_runs[0];
  if (!view.task.enabled) return { label: "已暂停", className: "" };
  if (activeManualRun?.status === "running" || latest?.status === "running") {
    return { label: "正在备份", className: "running" };
  }
  if (activeManualRun?.status === "queued") return { label: "队列等待", className: "running" };
  if (latest && ["error", "paused", "partial_success", "reconcile_required"].includes(latest.status)) {
    return { label: latest.status === "partial_success" ? "部分完成" : latest.status, className: "error" };
  }
  return { label: "运行中", className: "enabled" };
}

function modeLabel(task) {
  const names = { local: "本地", cloud: "Google Drive", dual: "本地 + Google Drive", bidirectional: "双向同步" };
  return names[task.mode] || task.mode;
}

function appendButton(parent, label, className, onClick, disabled = false) {
  const button = element("button", className, label);
  button.type = "button";
  button.disabled = disabled;
  button.addEventListener("click", onClick);
  parent.append(button);
  return button;
}

function renderTasks() {
  taskList.replaceChildren();
  const historySelect = document.querySelector("#history-task");
  const selectedHistoryTask = historySelect.value;
  historySelect.replaceChildren();
  for (const view of tasks) {
    const option = element("option", "", view.task.name);
    option.value = view.task.id;
    historySelect.append(option);

    const card = element("article", "task-card");
    const header = element("div", "task-card-header");
    header.append(element("h3", "task-name", view.task.name));
    const state = taskState(view);
    header.append(element("span", `state-pill ${state.className}`, state.label));
    card.append(header);
    card.append(element("p", "task-path", `源：${view.task.source.path || "尚未选择"}`));
    if (view.task.destinations.local.enabled) {
      card.append(element("p", "task-path", `本地目标：${view.task.destinations.local.path}`));
    }
    const latest = view.recent_runs[0];
    const detail = latest
      ? `最近一次：${latest.status} · 扫描 ${latest.scanned_files} 个文件 · 变更 ${latest.changed_files} 个 · ${formatDate(latest.started_at)}`
      : "尚无备份记录";
    card.append(element("p", "task-summary", `${modeLabel(view.task)} · ${view.task.schedule.type} · ${detail}`));
    if (latest?.error_summary) card.append(element("p", "task-path", latest.error_summary));
    const actions = element("div", "task-actions");
    appendButton(actions, "编辑", "secondary", () => openEditor(view.task));
    appendButton(actions, view.task.enabled ? "暂停" : "启用 / 恢复", view.task.enabled ? "secondary" : "primary", () => toggleTask(view));
    appendButton(actions, "立即备份", "secondary", () => queueBackup(view), !view.task.enabled);
    appendButton(actions, "查看历史", "secondary", () => {
      document.querySelector("#history-task").value = view.task.id;
      activatePanel("history");
      refreshHistory();
    });
    card.append(actions);
    taskList.append(card);
  }
  if (!tasks.length) taskList.append(element("div", "empty-state", "还没有备份任务。添加任务后，选择源目录并检查目标位置。"));
  if (tasks.some((view) => view.task.id === selectedHistoryTask)) historySelect.value = selectedHistoryTask;
  else if (tasks.length) historySelect.value = tasks[0].task.id;
}

async function refreshTasks() {
  try {
    tasks = await invoke("list_backup_tasks");
    renderTasks();
    return true;
  } catch (error) {
    taskList.replaceChildren(element("div", "empty-state", `无法读取任务：${String(error)}`));
    return false;
  }
}

function openEditor(task) {
  editingTask = JSON.parse(JSON.stringify(task));
  document.querySelector("#editor-title").textContent = task.id === "work-documents" ? "编辑工作资料任务" : "编辑备份任务";
  document.querySelector("#task-name").value = task.name;
  document.querySelector("#task-source").value = task.source.path || "";
  document.querySelector("#task-destination").value = task.destinations.local.path || "";
  document.querySelector("#task-mode").value = task.mode;
  document.querySelector("#task-schedule").value = task.schedule.type;
  document.querySelector("#task-interval").value = task.schedule.reconcile_interval_minutes;
  document.querySelector("#task-excludes").value = task.filters.exclude.join("\n");
  setMessage(editorMessage, "");
  taskEditor.showModal();
}

async function toggleTask(view) {
  const enabling = !view.task.enabled;
  if (enabling) {
    const task = view.task;
    const cloudNotice = task.destinations.google_drive.enabled
      ? "\nGoogle Drive 目标也会在授权有效时接收备份文件。"
      : "";
    const localNotice = task.destinations.local.enabled
      ? `\n确认后可能创建本地目标目录：${task.destinations.local.path}`
      : "";
    const accepted = window.confirm(
      `启用“${task.name}”后，Agent 将扫描所选源目录：\n${task.source.path || "（尚未选择）"}${localNotice}${cloudNotice}\n\n请确认路径范围和目标目录。`,
    );
    if (!accepted) return;
  }
  setMessage(taskMessage, enabling ? "正在验证并启用任务…" : "正在暂停任务…");
  try {
    await invoke("set_backup_task_enabled", {
      taskId: view.task.id,
      enabled: enabling,
      confirmed: enabling,
    });
    setMessage(taskMessage, enabling ? "任务已启用，Agent 正在接管。" : "已请求暂停；正在进行的文件操作会安全结束。", true);
    await refreshTasks();
  } catch (error) {
    setMessage(taskMessage, `操作未完成：${String(error)}`);
  }
}

async function queueBackup(view) {
  if (!window.confirm(`将“${view.task.name}”加入 Agent 的持久备份队列？关闭桌面窗口后队列仍会继续。`)) return;
  setMessage(taskMessage, "正在加入后台队列…");
  try {
    const run = await invoke("queue_backup_task", { taskId: view.task.id });
    setMessage(taskMessage, `备份请求已加入队列：${run.run_id}`, true);
    await refreshTasks();
  } catch (error) {
    setMessage(taskMessage, `加入队列失败：${String(error)}`);
  }
}

async function selectDirectory() {
  const selected = await tauri.dialog.open({ directory: true, multiple: false });
  return typeof selected === "string" ? selected : null;
}

async function refreshHistory() {
  const taskId = document.querySelector("#history-task").value;
  historyList.replaceChildren();
  if (!taskId) {
    historyList.append(element("div", "empty-state", "先添加一个备份任务。"));
    return;
  }
  try {
    const entries = await invoke("list_backup_history", { taskId });
    if (!entries.length) {
      historyList.append(element("div", "empty-state", "此任务还没有可恢复的历史版本。"));
      return;
    }
    for (const entry of entries) {
      const card = element("article", "history-card");
      const header = element("div", "history-card-header");
      header.append(element("strong", "", entry.display_relative_path));
      header.append(element("span", "state-pill", entry.storage_target === "local" ? "本地" : "Google Drive"));
      card.append(header);
      card.append(element("p", "", `${formatDate(entry.created_at_unix_ms)} · ${entry.size_bytes.toLocaleString()} 字节${entry.is_latest ? " · 当前版本" : ""}`));
      appendButton(card, "恢复到新目录", "secondary", () => restoreHistory(taskId, entry));
      historyList.append(card);
    }
  } catch (error) {
    setMessage(historyMessage, `读取历史失败：${String(error)}`);
  }
}

async function restoreHistory(taskId, entry) {
  const accepted = window.confirm(`将“${entry.display_relative_path}”恢复到你选择位置下的一个新目录，不覆盖已有文件。继续？`);
  if (!accepted) return;
  try {
    const destinationParent = await selectDirectory();
    if (!destinationParent) return;
    setMessage(historyMessage, "正在下载、校验并恢复历史版本…");
    const destination = await invoke("restore_backup_history_version", {
      taskId,
      versionId: entry.version_id,
      storageTarget: entry.storage_target,
      destinationParent,
    });
    setMessage(historyMessage, `恢复完成：${destination}`, true);
  } catch (error) {
    setMessage(historyMessage, `恢复失败：${String(error)}`);
  }
}

async function refreshLogs() {
  logList.replaceChildren();
  try {
    const entries = await invoke("get_recent_agent_logs");
    if (!entries.length) {
      logList.append(element("div", "empty-state", "暂无 Agent 日志。"));
      return;
    }
    for (const entry of entries) {
      const card = element("article", `log-card ${entry.level}`);
      const meta = element("div", "log-meta");
      meta.append(element("span", "", formatDate(entry.timestamp_unix_ms)));
      meta.append(element("span", "", entry.task_id || "Agent"));
      meta.append(element("span", "", entry.level));
      card.append(meta);
      card.append(element("p", "", entry.message));
      logList.append(card);
    }
  } catch (error) {
    logList.append(element("div", "empty-state", `无法读取日志：${String(error)}`));
  }
}

async function refreshSettings() {
  try {
    const settings = await invoke("get_application_settings");
    applicationSettings = settings;
    document.querySelector("#start-with-windows").checked = settings.start_with_windows;
    document.querySelector("#start-minimized").checked = settings.start_minimized;
    document.querySelector("#minimize-to-tray").checked = settings.minimize_to_tray;
  } catch (error) {
    setMessage(settingsMessage, `读取设置失败：${String(error)}`);
  }
}

function activatePanel(name) {
  document.querySelectorAll(".tab").forEach((tab) => tab.classList.toggle("active", tab.dataset.panel === name));
  document.querySelectorAll(".panel").forEach((panel) => {
    const active = panel.id === `panel-${name}`;
    panel.classList.toggle("active", active);
    panel.hidden = !active;
  });
  if (name === "history") refreshHistory();
  if (name === "logs") refreshLogs();
  if (name === "settings") refreshSettings();
}

document.querySelectorAll(".tab").forEach((tab) => tab.addEventListener("click", () => activatePanel(tab.dataset.panel)));

document.querySelector("#create-task").addEventListener("click", async () => {
  try {
    const task = await invoke("create_backup_task");
    await refreshTasks();
    openEditor(task);
  } catch (error) {
    setMessage(taskMessage, `创建任务失败：${String(error)}`);
  }
});

document.querySelector("#choose-source").addEventListener("click", async () => {
  try {
    const path = await selectDirectory();
    if (path) document.querySelector("#task-source").value = path;
  } catch (error) {
    setMessage(editorMessage, `无法打开目录选择器：${String(error)}`);
  }
});

document.querySelector("#choose-destination").addEventListener("click", async () => {
  try {
    const path = await selectDirectory();
    if (path) document.querySelector("#task-destination").value = path;
  } catch (error) {
    setMessage(editorMessage, `无法打开目录选择器：${String(error)}`);
  }
});

document.querySelector("#close-editor").addEventListener("click", () => taskEditor.close());
document.querySelector("#cancel-editor").addEventListener("click", () => taskEditor.close());

taskForm.addEventListener("submit", async (event) => {
  event.preventDefault();
  if (!editingTask) return;
  const task = editingTask;
  task.name = document.querySelector("#task-name").value.trim();
  task.source.path = document.querySelector("#task-source").value;
  task.destinations.local.path = document.querySelector("#task-destination").value;
  task.mode = document.querySelector("#task-mode").value;
  task.destinations.local.enabled = task.mode === "local" || task.mode === "dual";
  task.destinations.google_drive.enabled = task.mode === "cloud" || task.mode === "dual";
  task.schedule.type = document.querySelector("#task-schedule").value;
  task.schedule.reconcile_interval_minutes = Number(document.querySelector("#task-interval").value);
  task.schedule.scan_on_startup = true;
  task.filters.exclude = document.querySelector("#task-excludes").value
    .split(/\r?\n/).map((line) => line.trim()).filter(Boolean);
  if (task.destinations.google_drive.enabled && !task.destinations.google_drive.account_id) {
    setMessage(editorMessage, "此任务选择了 Google Drive，请先在“云端账户”连接 Google 账户。任务暂时保持关闭。");
    return;
  }
  setMessage(editorMessage, "正在验证任务配置…");
  try {
    await invoke("save_backup_task", { task });
    taskEditor.close();
    setMessage(taskMessage, "任务配置已保存；任务仍保持当前启停状态。", true);
    await refreshTasks();
  } catch (error) {
    setMessage(editorMessage, `保存失败：${String(error)}`);
  }
});

document.querySelector("#history-task").addEventListener("change", refreshHistory);
document.querySelector("#refresh-logs").addEventListener("click", refreshLogs);

document.querySelector("#save-settings").addEventListener("click", async () => {
  const settings = {
    ...(applicationSettings ?? { language: "zh-CN", max_parallel_transfers: 2 }),
    start_with_windows: document.querySelector("#start-with-windows").checked,
    start_minimized: document.querySelector("#start-minimized").checked,
    minimize_to_tray: document.querySelector("#minimize-to-tray").checked,
  };
  try {
    await invoke("save_application_settings", { settings });
    applicationSettings = settings;
    setMessage(settingsMessage, "设置已保存。登录启动项只影响当前 Windows 用户。", true);
  } catch (error) {
    setMessage(settingsMessage, `保存设置失败：${String(error)}`);
  }
});

saveSecretButton.addEventListener("click", async () => {
  const secret = clientSecretInput.value;
  saveSecretButton.disabled = true;
  setMessage(accountMessage, "正在安全保存客户端密钥…");
  try {
    await invoke("store_google_oauth_client_secret", { secret });
    clientSecretConfigured = true;
    secretSetup.hidden = true;
    if (!accountConnected) {
      connectButton.disabled = false;
      connectButton.textContent = "连接 Google Drive";
    }
    setMessage(accountMessage, "客户端密钥已保存到 Windows 凭据管理器。", true);
  } catch (error) {
    setMessage(accountMessage, `保存未完成：${String(error)}`);
  } finally {
    clientSecretInput.value = "";
    saveSecretButton.disabled = false;
  }
});

connectButton.addEventListener("click", async () => {
  const reauthorizing = accountConnected;
  if (reauthorizing) {
    accountConnected = false;
    connection.classList.remove("connected");
    connection.textContent = "正在清除此设备的登录状态…";
  }
  connectButton.disabled = true;
  connectButton.textContent = reauthorizing ? "正在重新授权…" : "等待 Google 授权…";
  setMessage(accountMessage, reauthorizing
    ? "将清除此设备保存的登录令牌和账号绑定，然后打开 Google 授权页面。完成授权后请返回此窗口。"
    : "系统浏览器将打开 Google 授权页面。完成授权后请返回此窗口。");
  try {
    const status = await invoke(reauthorizing ? "reauthorize_google_drive" : "connect_google_drive");
    showConnected(status.email);
    setMessage(accountMessage, "账户连接成功。备份任务仍需要你单独启用。", true);
    await refreshTasks();
  } catch (error) {
    await refreshAccountStatus();
    setMessage(accountMessage, `连接未完成：${String(error)}`);
    if (!accountConnected) {
      connectButton.disabled = !clientSecretConfigured;
      connectButton.textContent = clientSecretConfigured ? "重试连接 Google Drive" : "请先安全保存客户端密钥";
    }
  }
});

async function initialize() {
  await Promise.all([refreshAccountStatus(), refreshTasks(), refreshSettings(), refreshAgentStatus()]);
  window.setInterval(async () => {
    await refreshAgentStatus();
    if (!taskEditor.open) await refreshTasks();
  }, 5000);
}

initialize();
