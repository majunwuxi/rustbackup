const invoke = window.__TAURI__.core.invoke;
const connection = document.querySelector("#connection");
const connectButton = document.querySelector("#connect");
const message = document.querySelector("#message");

function showConnected(email) {
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
    if (status.connected) {
      showConnected(status.email);
    } else {
      connection.textContent = "尚未连接 Google 账户";
      connectButton.disabled = false;
    }
  } catch (error) {
    connection.textContent = "无法读取 RustBackup 配置";
    message.textContent = String(error);
  }
}

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
    connectButton.disabled = false;
    connectButton.textContent = "重试连接 Google Drive";
  }
});

refreshStatus();
