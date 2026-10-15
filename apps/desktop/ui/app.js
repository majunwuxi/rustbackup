const invoke = window.__TAURI__.core.invoke;
const connection = document.querySelector("#connection");
const connectButton = document.querySelector("#connect");
const message = document.querySelector("#message");
const environment = document.querySelector("#environment");
const secretSetup = document.querySelector("#secret-setup");
const clientSecretInput = document.querySelector("#client-secret");
const saveSecretButton = document.querySelector("#save-secret");
let clientSecretConfigured = false;
let accountConnected = false;

function showConnected(email) {
  accountConnected = true;
  connection.classList.add("connected");
  connection.textContent = email
    ? `已连接 Google 账户：${email}`
    : "已连接 Google 账户";
  connectButton.disabled = !clientSecretConfigured;
  connectButton.textContent = clientSecretConfigured
    ? "重新授权 Google Drive"
    : "请先安全保存客户端密钥";
}

async function refreshStatus() {
  try {
    const status = await invoke("google_drive_connection_status");
    environment.textContent = status.test_environment ? "测试环境" : "正式环境";
    clientSecretConfigured = status.client_secret_configured;
    secretSetup.hidden = clientSecretConfigured;
    if (status.connected) {
      showConnected(status.email);
    } else {
      accountConnected = false;
      connection.textContent = "尚未连接 Google 账户";
      connectButton.disabled = !clientSecretConfigured;
      connectButton.textContent = clientSecretConfigured
        ? "连接 Google Drive"
        : "请先安全保存客户端密钥";
    }
  } catch (error) {
    connection.textContent = "无法读取 RustBackup 授权状态";
    message.textContent = String(error);
    message.classList.remove("success");
  }
}

saveSecretButton.addEventListener("click", async () => {
  const secret = clientSecretInput.value;
  saveSecretButton.disabled = true;
  message.textContent = "正在安全保存客户端密钥…";
  message.classList.remove("success");

  try {
    await invoke("store_google_oauth_client_secret", { secret });
    clientSecretConfigured = true;
    secretSetup.hidden = true;
    if (!accountConnected) {
      connectButton.disabled = false;
      connectButton.textContent = "连接 Google Drive";
    }
    message.textContent = "客户端密钥已保存到 Windows 凭据管理器。";
    message.classList.add("success");
  } catch (error) {
    message.textContent = `保存未完成：${String(error)}`;
    message.classList.remove("success");
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
  message.textContent = reauthorizing
    ? "将清除此设备保存的登录令牌和账号绑定，然后打开 Google 授权页面。完成授权后请返回此窗口。"
    : "系统浏览器将打开 Google 授权页面。完成授权后请返回此窗口。";
  message.classList.remove("success");

  try {
    const status = await invoke(
      reauthorizing ? "reauthorize_google_drive" : "connect_google_drive",
    );
    showConnected(status.email);
    message.textContent = "账户连接成功。备份任务仍保持关闭。";
    message.classList.add("success");
  } catch (error) {
    await refreshStatus();
    message.textContent = `连接未完成：${String(error)}`;
    message.classList.remove("success");
    if (!accountConnected) {
      connectButton.disabled = !clientSecretConfigured;
      connectButton.textContent = clientSecretConfigured
        ? "重试连接 Google Drive"
        : "请先安全保存客户端密钥";
    }
  }
});

refreshStatus();
