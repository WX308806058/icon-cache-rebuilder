<script setup lang="ts">
import { nextTick, onMounted, onUnmounted, reactive, ref } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { getVersion } from "@tauri-apps/api/app";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { CacheInfo, RebuildResult, StepRecord, StepStatus } from "./types";

interface StepItem {
  step: number;
  name: string;
  status: StepStatus;
  message: string;
}

interface LogLine {
  time: string;
  text: string;
  level: "info" | "success" | "warn" | "error";
}

const INIT_STEPS: StepItem[] = [
  { step: 1, name: "结束 explorer.exe 进程", status: "idle", message: "taskkill /f /im explorer.exe" },
  { step: 2, name: "清理 Explorer 图标缓存目录", status: "idle", message: "del /f /s /q %LOCALAPPDATA%\\Microsoft\\Windows\\Explorer\\*" },
  { step: 3, name: "删除 IconCache.db", status: "idle", message: "del /f /a %LOCALAPPDATA%\\IconCache.db" },
  { step: 4, name: "重启 explorer.exe", status: "idle", message: "start explorer.exe" },
];

const steps = reactive<StepItem[]>(INIT_STEPS.map((s) => ({ ...s })));
const running = ref(false);
const finished = ref<null | { success: boolean; total_deleted: number; failed: number }>(null);
const logs = reactive<LogLine[]>([]);
const cacheInfo = ref<CacheInfo | null>(null);
const cacheLoading = ref(false);
const showConfirm = ref(false);
const logPanel = ref<HTMLElement | null>(null);

const appVersion = ref("");
const checkingUpdate = ref(false);
const downloadingUpdate = ref(false);
const pendingUpdate = ref<Update | null>(null);
const showUpdateModal = ref(false);
let lastLoggedProgress = -1;

let unlisten: UnlistenFn | null = null;

function now(): string {
  return new Date().toLocaleTimeString("zh-CN", { hour12: false });
}

function pushLog(text: string, level: LogLine["level"] = "info") {
  logs.push({ time: now(), text, level });
  nextTick(() => {
    if (logPanel.value) logPanel.value.scrollTop = logPanel.value.scrollHeight;
  });
}

async function loadCacheInfo() {
  cacheLoading.value = true;
  try {
    cacheInfo.value = await invoke<CacheInfo>("get_cache_info");
  } catch (e) {
    pushLog(`读取缓存信息失败: ${e}`, "error");
  } finally {
    cacheLoading.value = false;
  }
}

function applyStepEvent(rec: StepRecord) {
  const target = steps.find((s) => s.step === rec.step);
  if (target) {
    target.status = rec.status;
    target.message = rec.message;
  }
  const levelMap: Record<StepStatus, LogLine["level"]> = {
    idle: "info",
    running: "info",
    success: "success",
    warning: "warn",
    failed: "error",
  };
  pushLog(`[${rec.name}] ${rec.message}`, levelMap[rec.status] ?? "info");
}

async function startRebuild() {
  showConfirm.value = false;
  if (running.value) return;
  running.value = true;
  finished.value = null;
  for (const s of steps) {
    s.status = "idle";
    s.message = INIT_STEPS[s.step - 1].message;
  }
  pushLog("开始重建图标缓存 ...");
  try {
    const result = await invoke<RebuildResult>("rebuild_icon_cache");
    finished.value = {
      success: result.success,
      total_deleted: result.total_deleted,
      failed: result.failed_files.length,
    };
    if (result.success) {
      pushLog(`重建完成：共清理 ${result.total_deleted} 个缓存文件`, "success");
    } else {
      pushLog(
        `重建结束：共清理 ${result.total_deleted} 个文件，${result.failed_files.length} 个失败项`,
        "error",
      );
      result.failed_files.forEach((f) => pushLog(`  失败: ${f}`, "error"));
    }
    window.setTimeout(() => loadCacheInfo(), 1500);
  } catch (e) {
    finished.value = { success: false, total_deleted: 0, failed: 0 };
    pushLog(`执行出错: ${e}`, "error");
  } finally {
    running.value = false;
  }
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(2)} MB`;
}

async function checkForUpdate(silent = false) {
  if (checkingUpdate.value || downloadingUpdate.value) return;
  checkingUpdate.value = true;
  if (!silent) pushLog("正在检查更新 ...");
  try {
    // 官方源直连超时时会自动回退到镜像 endpoint；timeout 单位为毫秒，15 秒
    const update = await check({ timeout: 15000 });
    if (update) {
      pendingUpdate.value = update;
      showUpdateModal.value = true;
      pushLog(`发现新版本 v${update.version}（当前 v${appVersion.value}）`, "success");
    } else if (!silent) {
      pushLog("当前已是最新版本。", "success");
    }
  } catch (e) {
    pushLog(`检查更新失败: ${e}`, "error");
  } finally {
    checkingUpdate.value = false;
  }
}

async function installUpdate() {
  const update = pendingUpdate.value;
  if (!update || downloadingUpdate.value) return;
  downloadingUpdate.value = true;
  showUpdateModal.value = false;
  lastLoggedProgress = -1;
  try {
    pushLog(`开始下载 v${update.version} ...`);
    let contentLength = 0;
    let downloaded = 0;
    await update.downloadAndInstall((event) => {
      switch (event.event) {
        case "Started":
          contentLength = event.data.contentLength ?? 0;
          if (contentLength > 0) pushLog(`更新包大小 ${formatBytes(contentLength)}`);
          break;
        case "Progress":
          downloaded += event.data.chunkLength;
          if (contentLength > 0) {
            const percent = Math.floor((downloaded / contentLength) * 100);
            if (percent >= lastLoggedProgress + 20) {
              lastLoggedProgress = percent;
              pushLog(`下载进度 ${percent}%（${formatBytes(downloaded)} / ${formatBytes(contentLength)}）`);
            }
          }
          break;
        case "Finished":
          pushLog("下载完成，正在安装新版本 ...", "success");
          break;
      }
    });
    pushLog("安装完成，应用即将重启。", "success");
    await relaunch();
  } catch (e) {
    pushLog(`更新失败: ${e}`, "error");
  } finally {
    downloadingUpdate.value = false;
    pendingUpdate.value = null;
  }
}

onMounted(async () => {
  unlisten = await listen<StepRecord>("rebuild-progress", (event) => applyStepEvent(event.payload));
  await loadCacheInfo();
  pushLog("就绪。点击“开始重建图标缓存”执行操作。");
  try {
    appVersion.value = await getVersion();
  } catch {
    appVersion.value = "";
  }
  // 启动后静默检查一次更新，失败不打扰用户（日志区留痕）
  checkForUpdate(true);
});

onUnmounted(() => {
  unlisten?.();
});
</script>

<template>
  <div class="app">
    <header class="header">
      <div class="logo">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">
          <path d="M21 12a9 9 0 1 1-2.64-6.36" />
          <path d="M21 3v6h-6" />
        </svg>
      </div>
      <div class="header-text">
        <h1>图标缓存重建工具 OAO</h1>
        <p>修复桌面 / 任务栏图标显示异常与缓存损坏</p>
      </div>
    </header>

    <section class="card">
      <div class="card-head">
        <h2>缓存概况</h2>
        <button
          class="icon-btn"
          :disabled="cacheLoading || running"
          title="刷新缓存信息"
          @click="loadCacheInfo"
        >
          <svg :class="{ spinning: cacheLoading }" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M21 12a9 9 0 1 1-2.64-6.36" />
            <path d="M21 3v6h-6" />
          </svg>
        </button>
      </div>
      <div v-if="cacheInfo" class="info-grid">
        <div class="info-row">
          <span class="label">缓存目录</span>
          <span class="value mono ellipsis" :title="cacheInfo.explorer_dir">{{ cacheInfo.explorer_dir }}</span>
        </div>
        <div class="info-row">
          <span class="label">缓存文件</span>
          <span class="value">{{ cacheInfo.file_count }} 个 · {{ formatBytes(cacheInfo.total_size) }}</span>
        </div>
        <div class="info-row">
          <span class="label">IconCache.db</span>
          <span class="value">
            <template v-if="cacheInfo.icon_cache_db !== null">{{ formatBytes(cacheInfo.icon_cache_db) }}</template>
            <span v-else class="muted">不存在</span>
          </span>
        </div>
      </div>
      <div v-else class="muted placeholder">正在读取缓存信息 ...</div>
    </section>

    <div class="banner">
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
        <path d="M12 9v4" />
        <path d="M12 17h.01" />
        <path d="M10.29 3.86 1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z" />
      </svg>
      <span>执行时将强制结束并重启 explorer.exe，任务栏与桌面会短暂消失，请先保存正在编辑的工作。</span>
    </div>

    <section class="card">
      <div class="card-head">
        <h2>执行步骤</h2>
      </div>
      <ol class="steps">
        <li v-for="s in steps" :key="s.step" class="step" :data-status="s.status">
          <span class="step-icon">
            <svg v-if="s.status === 'success'" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round">
              <path d="M20 6 9 17l-5-5" />
            </svg>
            <svg v-else-if="s.status === 'failed'" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round">
              <path d="M18 6 6 18" />
              <path d="m6 6 12 12" />
            </svg>
            <svg v-else-if="s.status === 'warning'" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
              <path d="M12 9v4" />
              <path d="M12 17h.01" />
              <path d="M10.29 3.86 1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z" />
            </svg>
            <span v-else-if="s.status === 'running'" class="spinner"></span>
            <span v-else class="step-num">{{ s.step }}</span>
          </span>
          <div class="step-body">
            <div class="step-name">{{ s.step }}. {{ s.name }}</div>
            <div class="step-msg" :title="s.message">{{ s.message }}</div>
          </div>
        </li>
      </ol>
    </section>

    <div v-if="finished" class="result-banner" :class="finished.success ? 'ok' : 'bad'">
      <template v-if="finished.success">重建完成，共清理 {{ finished.total_deleted }} 个文件</template>
      <template v-else>重建结束，{{ finished.failed }} 个失败项，详见日志</template>
    </div>

    <button class="primary-btn" :disabled="running" @click="showConfirm = true">
      <span v-if="running" class="btn-inner"><span class="btn-spinner"></span>正在重建 ...</span>
      <span v-else class="btn-inner">开始重建图标缓存</span>
    </button>

    <section class="card log-card">
      <div class="card-head">
        <h2>执行日志</h2>
        <span class="muted">{{ logs.length }} 条</span>
      </div>
      <div ref="logPanel" class="log">
        <div v-for="(l, i) in logs" :key="i" class="log-line" :data-level="l.level">
          <span class="log-time">{{ l.time }}</span>
          <span class="log-text">{{ l.text }}</span>
        </div>
      </div>
    </section>

    <footer class="footer">
      <div class="footer-left">
        <span v-if="appVersion" class="ver">v{{ appVersion }}</span>
        <button
          class="update-btn"
          :disabled="checkingUpdate || downloadingUpdate"
          title="从 GitHub 检查是否有新版本"
          @click="checkForUpdate()"
        >
          <span v-if="downloadingUpdate" class="btn-inner"><span class="btn-spinner mini"></span>正在更新 ...</span>
          <span v-else-if="checkingUpdate" class="btn-inner"><span class="btn-spinner mini"></span>检查更新 ...</span>
          <span v-else>检查更新</span>
        </button>
      </div>
      <span class="stack">Tauri 2 · Vue 3 · Vite 5 · TypeScript</span>
    </footer>

    <div v-if="showConfirm" class="modal-overlay" @click.self="showConfirm = false">
      <div class="modal">
        <div class="modal-icon">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M12 9v4" />
            <path d="M12 17h.01" />
            <path d="M10.29 3.86 1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z" />
          </svg>
        </div>
        <h3>确认执行重建？</h3>
        <p class="modal-text">
          执行过程中 explorer.exe 将被强制结束并自动重启，任务栏与桌面会短暂消失（约 1~3 秒），已打开的文件资源管理器窗口将被关闭。
        </p>
        <p class="modal-tip">建议先保存所有正在编辑的工作。</p>
        <div class="modal-actions">
          <button class="ghost-btn" :disabled="running" @click="showConfirm = false">取消</button>
          <button class="primary-btn small" :disabled="running" @click="startRebuild">确认执行</button>
        </div>
      </div>
    </div>

    <div v-if="showUpdateModal && pendingUpdate" class="modal-overlay" @click.self="showUpdateModal = false">
      <div class="modal">
        <div class="modal-icon update">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M12 3v12" />
            <path d="m7 10 5 5 5-5" />
            <path d="M5 21h14" />
          </svg>
        </div>
        <h3>发现新版本 v{{ pendingUpdate.version }}</h3>
        <p class="modal-text">
          当前版本 <span class="mono-inline">v{{ appVersion }}</span>，新版本
          <span class="mono-inline">v{{ pendingUpdate.version }}</span> 已发布。
        </p>
        <p v-if="pendingUpdate.body" class="update-notes">{{ pendingUpdate.body }}</p>
        <p class="modal-tip">下载完成后将自动安装新版本并重启应用。</p>
        <div class="modal-actions">
          <button class="ghost-btn" @click="showUpdateModal = false">稍后再说</button>
          <button class="primary-btn small" @click="installUpdate">立即更新</button>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.app {
  position: relative;
  height: 100%;
  display: flex;
  flex-direction: column;
  gap: 12px;
  padding: 20px 22px 14px;
  background:
    radial-gradient(600px 300px at 85% -10%, rgba(56, 189, 248, 0.14), transparent 60%),
    radial-gradient(500px 260px at -10% 110%, rgba(99, 102, 241, 0.12), transparent 60%),
    #0a0f1e;
  overflow-y: auto;
}

/* ---------- header ---------- */
.header {
  display: flex;
  align-items: center;
  gap: 14px;
}

.logo {
  width: 46px;
  height: 46px;
  flex-shrink: 0;
  display: grid;
  place-items: center;
  border-radius: 13px;
  color: #e0f2fe;
  background: linear-gradient(135deg, #0ea5e9, #6366f1);
  box-shadow: 0 6px 18px rgba(56, 128, 248, 0.35);
}

.logo svg {
  width: 26px;
  height: 26px;
}

.header h1 {
  font-size: 19px;
  font-weight: 600;
  letter-spacing: 0.5px;
}

.header p {
  margin-top: 3px;
  font-size: 12px;
  color: #94a3b8;
}

/* ---------- card ---------- */
.card {
  border: 1px solid rgba(148, 163, 184, 0.14);
  border-radius: 14px;
  background: rgba(148, 163, 184, 0.06);
  padding: 14px 16px;
}

.card-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-bottom: 10px;
}

.card-head h2 {
  font-size: 13px;
  font-weight: 600;
  color: #cbd5e1;
  letter-spacing: 1px;
}

.icon-btn {
  width: 26px;
  height: 26px;
  display: grid;
  place-items: center;
  border: 1px solid rgba(148, 163, 184, 0.2);
  border-radius: 8px;
  background: transparent;
  color: #94a3b8;
  cursor: pointer;
  transition: all 0.15s ease;
}

.icon-btn:hover:not(:disabled) {
  color: #38bdf8;
  border-color: rgba(56, 189, 248, 0.5);
}

.icon-btn:disabled {
  opacity: 0.45;
  cursor: not-allowed;
}

.icon-btn svg {
  width: 14px;
  height: 14px;
}

.spinning {
  animation: rotate 1s linear infinite;
}

@keyframes rotate {
  to {
    transform: rotate(360deg);
  }
}

/* ---------- info ---------- */
.info-grid {
  display: flex;
  flex-direction: column;
  gap: 7px;
}

.info-row {
  display: flex;
  align-items: baseline;
  gap: 12px;
  font-size: 13px;
}

.info-row .label {
  flex-shrink: 0;
  width: 96px;
  color: #94a3b8;
}

.info-row .value {
  color: #e2e8f0;
  min-width: 0;
}

.ellipsis {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.muted {
  color: #64748b;
}

.placeholder {
  font-size: 13px;
  padding: 4px 0;
}

/* ---------- banner ---------- */
.banner {
  display: flex;
  align-items: flex-start;
  gap: 10px;
  padding: 10px 14px;
  border-radius: 12px;
  font-size: 12.5px;
  line-height: 1.55;
  color: #fcd34d;
  background: rgba(251, 191, 36, 0.08);
  border: 1px solid rgba(251, 191, 36, 0.25);
}

.banner svg {
  width: 17px;
  height: 17px;
  flex-shrink: 0;
  margin-top: 1.5px;
}

/* ---------- steps ---------- */
.steps {
  list-style: none;
  display: flex;
  flex-direction: column;
}

.step {
  position: relative;
  display: flex;
  gap: 12px;
  padding: 7px 0;
}

.step:not(:last-child)::before {
  content: "";
  position: absolute;
  left: 13px;
  top: 34px;
  bottom: -6px;
  width: 1px;
  background: rgba(148, 163, 184, 0.18);
}

.step-icon {
  width: 27px;
  height: 27px;
  flex-shrink: 0;
  display: grid;
  place-items: center;
  border-radius: 50%;
  border: 1.5px solid rgba(148, 163, 184, 0.3);
  color: #94a3b8;
  font-size: 12px;
  font-weight: 600;
  transition: all 0.2s ease;
}

.step-icon svg {
  width: 14px;
  height: 14px;
}

.step[data-status="running"] .step-icon {
  border-color: rgba(56, 189, 248, 0.6);
  color: #38bdf8;
}

.step[data-status="success"] .step-icon {
  border-color: rgba(52, 211, 153, 0.6);
  background: rgba(52, 211, 153, 0.12);
  color: #34d399;
}

.step[data-status="warning"] .step-icon {
  border-color: rgba(251, 191, 36, 0.6);
  background: rgba(251, 191, 36, 0.1);
  color: #fbbf24;
}

.step[data-status="failed"] .step-icon {
  border-color: rgba(248, 113, 113, 0.6);
  background: rgba(248, 113, 113, 0.12);
  color: #f87171;
}

.step-name {
  font-size: 13.5px;
  font-weight: 500;
  color: #e2e8f0;
  line-height: 27px;
}

.step[data-status="success"] .step-name {
  color: #a7f3d0;
}

.step[data-status="failed"] .step-name {
  color: #fecaca;
}

.step-msg {
  margin-top: 2px;
  font-size: 12px;
  color: #7d8ba1;
  line-height: 1.4;
  word-break: break-all;
}

.step[data-status="running"] .step-msg {
  color: #7dd3fc;
}

.step[data-status="success"] .step-msg {
  color: #6ee7b7;
}

.step[data-status="warning"] .step-msg {
  color: #fcd34d;
}

.step[data-status="failed"] .step-msg {
  color: #fca5a5;
}

.spinner {
  width: 13px;
  height: 13px;
  border-radius: 50%;
  border: 2px solid rgba(56, 189, 248, 0.25);
  border-top-color: #38bdf8;
  animation: rotate 0.8s linear infinite;
}

/* ---------- result ---------- */
.result-banner {
  padding: 10px 14px;
  border-radius: 12px;
  font-size: 13px;
  font-weight: 500;
  text-align: center;
  animation: fadeUp 0.25s ease;
}

.result-banner.ok {
  color: #6ee7b7;
  background: rgba(52, 211, 153, 0.1);
  border: 1px solid rgba(52, 211, 153, 0.35);
}

.result-banner.bad {
  color: #fca5a5;
  background: rgba(248, 113, 113, 0.1);
  border: 1px solid rgba(248, 113, 113, 0.35);
}

/* ---------- button ---------- */
.primary-btn {
  border: none;
  border-radius: 12px;
  padding: 13px 20px;
  font-size: 15px;
  font-weight: 600;
  font-family: inherit;
  letter-spacing: 2px;
  color: #fff;
  cursor: pointer;
  background: linear-gradient(135deg, #0ea5e9, #6366f1);
  box-shadow: 0 6px 20px rgba(56, 128, 248, 0.35);
  transition: all 0.18s ease;
}

.primary-btn:hover:not(:disabled) {
  transform: translateY(-1px);
  box-shadow: 0 8px 26px rgba(56, 128, 248, 0.5);
  filter: brightness(1.08);
}

.primary-btn:active:not(:disabled) {
  transform: translateY(0);
}

.primary-btn:disabled {
  cursor: not-allowed;
  filter: saturate(0.4) brightness(0.75);
  box-shadow: none;
}

.primary-btn.small {
  padding: 9px 22px;
  font-size: 13.5px;
  letter-spacing: 1px;
}

.btn-inner {
  display: inline-flex;
  align-items: center;
  gap: 9px;
}

.btn-spinner {
  width: 15px;
  height: 15px;
  border-radius: 50%;
  border: 2px solid rgba(255, 255, 255, 0.35);
  border-top-color: #fff;
  animation: rotate 0.8s linear infinite;
}

.ghost-btn {
  border: 1px solid rgba(148, 163, 184, 0.3);
  border-radius: 12px;
  padding: 9px 22px;
  font-size: 13.5px;
  font-family: inherit;
  color: #cbd5e1;
  background: transparent;
  cursor: pointer;
  transition: all 0.15s ease;
}

.ghost-btn:hover:not(:disabled) {
  border-color: rgba(148, 163, 184, 0.55);
  color: #f1f5f9;
}

.ghost-btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}

/* ---------- log ---------- */
.log-card {
  flex: 1;
  display: flex;
  flex-direction: column;
  min-height: 130px;
}

.log-card .card-head {
  margin-bottom: 8px;
}

.log {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  border-radius: 10px;
  background: rgba(2, 6, 23, 0.65);
  border: 1px solid rgba(148, 163, 184, 0.1);
  padding: 10px 12px;
  font-family: "Cascadia Code", Consolas, "JetBrains Mono", monospace;
  font-size: 11.5px;
  line-height: 1.75;
  user-select: text;
}

.log-line {
  display: flex;
  gap: 10px;
}

.log-time {
  flex-shrink: 0;
  color: #475569;
}

.log-text {
  color: #94a3b8;
  word-break: break-all;
  white-space: pre-wrap;
}

.log-line[data-level="success"] .log-text {
  color: #6ee7b7;
}

.log-line[data-level="warn"] .log-text {
  color: #fcd34d;
}

.log-line[data-level="error"] .log-text {
  color: #fca5a5;
}

/* ---------- footer ---------- */
.footer {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 10px;
  font-size: 11px;
  color: #475569;
  letter-spacing: 1px;
}

.footer-left {
  display: flex;
  align-items: center;
  gap: 10px;
}

.ver {
  font-family: "Cascadia Code", Consolas, monospace;
  color: #64748b;
}

.update-btn {
  display: inline-flex;
  align-items: center;
  border: 1px solid rgba(148, 163, 184, 0.22);
  border-radius: 999px;
  padding: 4px 14px;
  font-size: 11.5px;
  font-family: inherit;
  letter-spacing: 1px;
  color: #94a3b8;
  background: transparent;
  cursor: pointer;
  transition: all 0.15s ease;
}

.update-btn:hover:not(:disabled) {
  color: #7dd3fc;
  border-color: rgba(56, 189, 248, 0.5);
}

.update-btn:disabled {
  opacity: 0.55;
  cursor: not-allowed;
}

.update-btn .btn-inner {
  display: inline-flex;
  align-items: center;
  gap: 6px;
}

.btn-spinner.mini {
  width: 11px;
  height: 11px;
  border-width: 1.5px;
}

/* ---------- modal ---------- */
.modal-overlay {
  position: fixed;
  inset: 0;
  z-index: 100;
  display: grid;
  place-items: center;
  background: rgba(2, 6, 23, 0.7);
  backdrop-filter: blur(4px);
  animation: fadeIn 0.15s ease;
}

.modal {
  width: 380px;
  border-radius: 16px;
  padding: 24px;
  background: #101830;
  border: 1px solid rgba(148, 163, 184, 0.18);
  box-shadow: 0 24px 60px rgba(0, 0, 0, 0.5);
  animation: fadeUp 0.2s ease;
}

.modal-icon {
  width: 44px;
  height: 44px;
  display: grid;
  place-items: center;
  border-radius: 50%;
  margin: 0 auto 14px;
  color: #fbbf24;
  background: rgba(251, 191, 36, 0.1);
  border: 1px solid rgba(251, 191, 36, 0.3);
}

.modal-icon svg {
  width: 22px;
  height: 22px;
}

.modal h3 {
  text-align: center;
  font-size: 16px;
  font-weight: 600;
  margin-bottom: 12px;
}

.modal-text {
  font-size: 13px;
  line-height: 1.7;
  color: #94a3b8;
}

.modal-tip {
  margin-top: 8px;
  font-size: 12px;
  color: #fcd34d;
}

.modal-icon.update {
  color: #34d399;
  background: rgba(52, 211, 153, 0.1);
  border-color: rgba(52, 211, 153, 0.35);
}

.mono-inline {
  font-family: "Cascadia Code", Consolas, monospace;
  color: #e2e8f0;
}

.update-notes {
  margin-top: 10px;
  max-height: 110px;
  overflow-y: auto;
  padding: 10px 12px;
  font-size: 12px;
  line-height: 1.6;
  color: #94a3b8;
  white-space: pre-wrap;
  word-break: break-word;
  border-radius: 10px;
  background: rgba(2, 6, 23, 0.55);
  border: 1px solid rgba(148, 163, 184, 0.14);
}

.modal-actions {
  display: flex;
  justify-content: flex-end;
  gap: 10px;
  margin-top: 20px;
}

@keyframes fadeIn {
  from {
    opacity: 0;
  }
}

@keyframes fadeUp {
  from {
    opacity: 0;
    transform: translateY(8px);
  }
}
</style>
