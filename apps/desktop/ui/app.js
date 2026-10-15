const invoke = window.__TAURI__.core.invoke;
const connection = document.querySelector("#connection");
const connectButton = document.querySelector("#connect");
const message = document.querySelector("#message");
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
  connectButton.disabled = true;
  connectButton.textContent = "账户已连接";
}

async function refreshStatus() {
  try {
    const status = await invoke("google_drive_connection_status");
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
    connection.textContent = "无法读取 RustBackup 配置";
    message.textContent = String(error);
  }
}

saveSecretButton.addEventListener("click", async () => {
  const secret = clientSecretInput.value;
  saveSecretButton.disabled = true;
  message.textContent = "正在安全保存客户端密钥…";

  try {
    await invoke("store_google_oauth_client_secret", { secret });
    clientSecretConfigured = true;
    secretSetup.hidden = true;
    if (!accountConnected) {
      connectButton.disabled = false;
      connectButton.textContent = "连接 Google Drive";
    }
    message.textContent = "客户端密钥已保存到 Windows 凭据管理器。";
  } catch (error) {
    message.textContent = `保存未完成：${String(error)}`;
  } finally {
    clientSecretInput.value = "";
    saveSecretButton.disabled = false;
  }
});

connectButton.addEventListener("click", async () => {
  connectButton.disabled = true;
  connectButton.textContent = "等待 Google 授权…";
  message.textContent = "系统浏览器将打开 Google 授权页面。完成授权后请返回此窗口。";

  try {
    const status = await invoke("connect_google_drive");
    showConnected(status.email);
    message.textContent = "账户连接成功。备份任务仍保持关闭。";
  } catch (error) {
    message.textContent = `连接未完成：${String(error)}`;
    connectButton.disabled = !clientSecretConfigured;
    connectButton.textContent = clientSecretConfigured
      ? "重试连接 Google Drive"
      : "请先安全保存客户端密钥";
  }
});

refreshStatus();
