<script setup lang="ts">
import { computed, nextTick, onMounted, onUnmounted, reactive, ref, shallowRef } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { getVersion } from "@tauri-apps/api/app";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { open } from "@tauri-apps/plugin-dialog";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  CacheInfo,
  RebuildResult,
  ReleaseInfo,
  ReleaseResult,
  StepRecord,
  StepStatus,
  WorkBuddyRemoveResult,
  WorkBuddyStatus,
} from "./types";

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

const RELEASE_STEPS: StepItem[] = [
  { step: 1, name: "环境校验", status: "idle", message: "检查 git、仓库与标签冲突" },
  { step: 2, name: "提交改动", status: "idle", message: "git add . && git commit" },
  { step: 3, name: "推送代码", status: "idle", message: "git push origin <分支>" },
  { step: 4, name: "创建附注标签", status: "idle", message: "git tag -a <版本> -m <发布说明>" },
  { step: 5, name: "推送标签", status: "idle", message: "git push origin <版本>" },
];

const steps = reactive<StepItem[]>(INIT_STEPS.map((s) => ({ ...s })));
const running = ref(false);
const finished = ref<null | { success: boolean; total_deleted: number; failed: number }>(null);
const logs = reactive<LogLine[]>([]);
const cacheInfo = ref<CacheInfo | null>(null);
const cacheLoading = ref(false);
const showConfirm = ref(false);
const logPanel = ref<HTMLElement | null>(null);

// ---------- 页签 ----------
const activeTab = ref<"rebuild" | "release">("rebuild");
const subtitle = computed(() =>
  activeTab.value === "rebuild"
    ? "修复桌面 / 任务栏图标显示异常与缓存损坏"
    : "提交 → 推送 → 打附注标签，GitHub Actions 自动构建并发布",
);

const appVersion = ref("");
const checkingUpdate = ref(false);
// 必须用 shallowRef：深响应式 ref 会把 Update 类实例包成 Proxy，
// downloadAndInstall() 在代理上执行时 this 不再持有 #私有字段，直接抛
// "Cannot read private member from an object whose class did not declare it"
const pendingUpdate = shallowRef<Update | null>(null);
const showUpdateModal = ref(false);
// 更新弹出框内部阶段：confirm 确认 → downloading 下载安装 → done 完成待重启
const updateStage = ref<"confirm" | "downloading" | "done">("confirm");
const updateProgress = ref(0); // 0~100
const updateFailed = ref(false);
// 更新过程日志：只在弹出框内显示，不进主窗口执行日志
const updateLogs = reactive<{ time: string; text: string; level: "info" | "success" | "error" }[]>([]);
const updateLogPanel = ref<HTMLElement | null>(null);
// 「检查更新」按钮旁的短暂反馈（2.5s 后自动消失）
const updateHint = ref<"" | "latest" | "error">("");
const ctxMenuEnabled = ref(false);
const ctxMenuBusy = ref(false);
const wbStatus = ref<WorkBuddyStatus | null>(null);
const wbBusy = ref(false);

// ---------- 版本发布 ----------
const releaseSteps = reactive<StepItem[]>(RELEASE_STEPS.map((s) => ({ ...s })));
const releaseDir = ref(localStorage.getItem("release-dir") || "D:\\ViteProjects\\icon-cache-rebuilder");
const releaseInfo = ref<ReleaseInfo | null>(null);
const releaseInfoLoading = ref(false);
const releaseInfoError = ref("");
const releaseVersion = ref("");
const releaseMessage = ref("");
const releaseNotes = ref("");
const releasing = ref(false);
const releaseStarted = ref(false);
const showReleaseConfirm = ref(false);
const normalizedReleaseVersion = computed(() => {
  const v = releaseVersion.value.trim();
  return v ? (v.startsWith("v") ? v : `v${v}`) : "";
});
// Release 页面说明预览：发布说明 → 提交说明 → 默认文案（与 lib.rs 后端回退逻辑一致）
const releaseNotesPreview = computed(() => {
  const n = releaseNotes.value.trim();
  if (n) return n;
  const m = releaseMessage.value.trim();
  return m || `chore: release ${normalizedReleaseVersion.value}`;
});

let unlisten: UnlistenFn | null = null;
let unlistenRelease: UnlistenFn | null = null;

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

async function loadContextMenuState() {
  try {
    ctxMenuEnabled.value = await invoke<boolean>("is_context_menu_enabled");
  } catch (e) {
    pushLog(`读取右键菜单状态失败: ${e}`, "error");
  }
}

async function toggleContextMenu() {
  if (ctxMenuBusy.value) return;
  ctxMenuBusy.value = true;
  try {
    ctxMenuEnabled.value = await invoke<boolean>("set_context_menu", {
      enabled: !ctxMenuEnabled.value,
    });
    pushLog(
      ctxMenuEnabled.value
        ? "右键菜单已注册：在桌面或文件夹空白处右键即可快速重建图标缓存。"
        : "右键菜单项已移除。",
      "success",
    );
  } catch (e) {
    pushLog(`设置右键菜单失败: ${e}`, "error");
  } finally {
    ctxMenuBusy.value = false;
  }
}

async function loadWorkBuddyStatus() {
  try {
    wbStatus.value = await invoke<WorkBuddyStatus>("check_workbuddy_menu");
  } catch (e) {
    pushLog(`检测 WorkBuddy 右键菜单失败: ${e}`, "error");
  }
}

async function removeWorkBuddy() {
  if (wbBusy.value) return;
  wbBusy.value = true;
  pushLog("正在清理 WorkBuddy 桌面右键菜单 ...");
  try {
    const result = await invoke<WorkBuddyRemoveResult>("remove_workbuddy_menu");
    result.removed.forEach((x) => pushLog(`已删除注册：${x}`, "success"));
    if (result.missing.length > 0) pushLog(`${result.missing.length} 处注册原本就不存在`);
    result.failed.forEach((x) => pushLog(`删除失败：${x}`, "error"));
    if (result.needs_elevation) {
      pushLog("存在系统级（HKLM）注册，正在请求管理员权限，请在弹出的 UAC 窗口中确认 ...", "warn");
      try {
        await invoke("remove_workbuddy_menu_elevated");
        pushLog("管理员授权完成，WorkBuddy 右键菜单已全部清除。", "success");
      } catch (e) {
        pushLog(`${e}`, "error");
      }
    } else if (result.removed.length > 0) {
      pushLog("WorkBuddy 右键菜单清理完成。", "success");
    } else {
      pushLog("未发现 WorkBuddy 右键菜单注册，无需清理。", "success");
    }
  } catch (e) {
    pushLog(`清理 WorkBuddy 右键菜单出错: ${e}`, "error");
  } finally {
    await loadWorkBuddyStatus();
    wbBusy.value = false;
  }
}

// ---------- 版本发布 ----------

function applyReleaseStepEvent(rec: StepRecord) {
  const target = releaseSteps.find((s) => s.step === rec.step);
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
  pushLog(`[发布·${rec.name}] ${rec.message}`, levelMap[rec.status] ?? "info");
}

async function loadReleaseInfo() {
  const dir = releaseDir.value.trim();
  if (!dir) return;
  releaseInfoLoading.value = true;
  releaseInfoError.value = "";
  try {
    const info = await invoke<ReleaseInfo>("get_release_info", { projectDir: dir });
    releaseInfo.value = info;
    // 版本号与提交说明只在未手动填写时预填，避免覆盖用户输入
    if (!releaseVersion.value.trim()) releaseVersion.value = info.next_version;
    if (!releaseMessage.value.trim()) releaseMessage.value = `chore: release ${info.next_version}`;
  } catch (e) {
    releaseInfo.value = null;
    releaseInfoError.value = String(e);
  } finally {
    releaseInfoLoading.value = false;
  }
}

async function pickReleaseDir() {
  if (releasing.value) return;
  try {
    const picked = await open({ directory: true, title: "选择项目所在目录" });
    if (typeof picked === "string" && picked) {
      releaseDir.value = picked;
      localStorage.setItem("release-dir", picked);
      // 目录变化后重新预填版本号与提交说明
      releaseVersion.value = "";
      releaseMessage.value = "";
      releaseNotes.value = "";
      await loadReleaseInfo();
    }
  } catch (e) {
    pushLog(`打开目录选择对话框失败: ${e}`, "error");
  }
}

function openReleaseConfirm() {
  if (releasing.value) return;
  if (!releaseInfo.value) {
    pushLog("请先选择有效的项目目录（git 仓库）", "error");
    return;
  }
  if (!normalizedReleaseVersion.value) {
    pushLog("请填写发布版本号（如 v0.9.1）", "error");
    return;
  }
  showReleaseConfirm.value = true;
}

async function startRelease() {
  showReleaseConfirm.value = false;
  if (releasing.value) return;
  releasing.value = true;
  releaseStarted.value = true;
  localStorage.setItem("release-dir", releaseDir.value.trim());
  for (const s of releaseSteps) {
    s.status = "idle";
    s.message = RELEASE_STEPS[s.step - 1].message;
  }
  pushLog(`开始发布 ${normalizedReleaseVersion.value} ...`);
  try {
    const result = await invoke<ReleaseResult>("release_version", {
      projectDir: releaseDir.value.trim(),
      version: releaseVersion.value.trim(),
      message: releaseMessage.value.trim(),
      notes: releaseNotes.value.trim(),
    });
    pushLog(
      `版本 ${result.tag} 发布流程完成：GitHub Actions 构建中，约 7~10 分钟后自动发布。`,
      "success",
    );
    pushLog("镜像缓存约 5 分钟后过期，届时旧版本应用可检测到新版本。");
  } catch (e) {
    pushLog(`发布失败: ${e}`, "error");
  } finally {
    releasing.value = false;
    loadReleaseInfo();
  }
}

async function checkForUpdate(silent = false) {
  if (checkingUpdate.value) return;
  checkingUpdate.value = true;
  if (!silent) updateHint.value = "";
  try {
    // 官方源直连超时时会自动回退到镜像 endpoint；timeout 单位为毫秒，15 秒
    const update = await check({ timeout: 15000 });
    if (update) {
      // 更新过程不写主窗口执行日志，全部在弹出框内展示
      pendingUpdate.value = update;
      updateStage.value = "confirm";
      updateFailed.value = false;
      updateProgress.value = 0;
      showUpdateModal.value = true;
    } else if (!silent) {
      updateHint.value = "latest";
      window.setTimeout(() => {
        if (updateHint.value === "latest") updateHint.value = "";
      }, 2500);
    }
  } catch {
    // 静默检查（启动时）失败不打扰用户；手动检查在按钮旁短暂提示
    if (!silent) {
      updateHint.value = "error";
      window.setTimeout(() => {
        if (updateHint.value === "error") updateHint.value = "";
      }, 2500);
    }
  } finally {
    checkingUpdate.value = false;
  }
}

function pushUpdateLog(text: string, level: "info" | "success" | "error" = "info") {
  updateLogs.push({ time: now(), text, level });
  nextTick(() => {
    if (updateLogPanel.value) updateLogPanel.value.scrollTop = updateLogPanel.value.scrollHeight;
  });
}

function closeUpdateModal() {
  // 下载进行中不允许关闭（避免中断安装）；失败后可以关闭
  if (updateStage.value === "downloading" && !updateFailed.value) return;
  showUpdateModal.value = false;
  pendingUpdate.value = null;
}

async function installUpdate() {
  const update = pendingUpdate.value;
  if (!update || (updateStage.value === "downloading" && !updateFailed.value)) return;
  updateStage.value = "downloading";
  updateFailed.value = false;
  updateProgress.value = 0;
  updateLogs.length = 0;
  let contentLength = 0;
  let downloaded = 0;
  let lastLoggedPercent = -20;
  pushUpdateLog(`发现新版本 v${update.version}（当前 v${appVersion.value}）`, "success");
  try {
    await update.downloadAndInstall((event) => {
      switch (event.event) {
        case "Started":
          contentLength = event.data.contentLength ?? 0;
          if (contentLength > 0) pushUpdateLog(`更新包大小 ${formatBytes(contentLength)}`);
          break;
        case "Progress": {
          downloaded += event.data.chunkLength;
          if (contentLength > 0) {
            updateProgress.value = Math.min(99, Math.floor((downloaded / contentLength) * 100));
            if (updateProgress.value >= lastLoggedPercent + 20) {
              lastLoggedPercent = updateProgress.value;
              pushUpdateLog(
                `下载进度 ${updateProgress.value}%（${formatBytes(downloaded)} / ${formatBytes(contentLength)}）`,
              );
            }
          }
          break;
        }
        case "Finished":
          updateProgress.value = 100;
          pushUpdateLog("下载完成，正在安装新版本 ...", "success");
          break;
      }
    });
    updateStage.value = "done";
    pushUpdateLog("安装完成，应用即将重启。", "success");
    // 短暂停留让用户看到完成状态，随后重启应用（所有窗口随之关闭）
    await new Promise((resolve) => setTimeout(resolve, 1000));
    await relaunch();
  } catch (e) {
    updateFailed.value = true;
    pushUpdateLog(`更新失败: ${e}`, "error");
  }
}

onMounted(async () => {
  unlisten = await listen<StepRecord>("rebuild-progress", (event) => applyStepEvent(event.payload));
  unlistenRelease = await listen<StepRecord>("release-progress", (event) =>
    applyReleaseStepEvent(event.payload),
  );
  await loadCacheInfo();
  loadContextMenuState();
  loadWorkBuddyStatus();
  loadReleaseInfo();
  pushLog("就绪。点击“开始重建图标缓存”执行操作。");
  try {
    appVersion.value = await getVersion();
  } catch {
    appVersion.value = "";
  }
  // 启动后静默检查一次更新：发现新版本弹确认框，失败不打扰用户
  checkForUpdate(true);
});

onUnmounted(() => {
  unlisten?.();
  unlistenRelease?.();
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
        <h1>系统优化工具</h1>
        <p>{{ subtitle }}</p>
      </div>
    </header>

    <nav class="tabs">
      <button class="tab" :class="{ active: activeTab === 'rebuild' }" @click="activeTab = 'rebuild'">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">
          <path d="M21 12a9 9 0 1 1-2.64-6.36" />
          <path d="M21 3v6h-6" />
        </svg>
        图标缓存重建
      </button>
      <button class="tab" :class="{ active: activeTab === 'release' }" @click="activeTab = 'release'">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">
          <path d="M4.5 16.5c-1.5 1.26-2 5-2 5s3.74-.5 5-2c.71-.84.7-2.13-.09-2.91a2.18 2.18 0 0 0-2.91-.09z" />
          <path d="m12 15-3-3a22 22 0 0 1 2-3.95A12.88 12.88 0 0 1 22 2c0 2.72-.78 7.5-6 11a22.35 22.35 0 0 1-4 2z" />
          <path d="M9 12H4s.55-3.03 2-4c1.62-1.08 5 0 5 0" />
          <path d="M12 15v5s3.03-.55 4-2c1.08-1.62 0-5 0-5" />
        </svg>
        版本发布
      </button>
    </nav>

    <div v-show="activeTab === 'rebuild'" class="tab-panel">
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
        <div class="info-row">
          <span class="label">右键菜单</span>
          <span class="value ctx-menu-value">
            <span
              class="ctx-state"
              :class="{ on: ctxMenuEnabled }"
              title="在桌面或文件夹空白处右键即可快速执行重建，无需打开应用"
            >
              {{ ctxMenuEnabled ? "已启用" : "未启用" }}
            </span>
            <button class="ctx-toggle" :disabled="ctxMenuBusy" @click="toggleContextMenu">
              {{ ctxMenuBusy ? "处理中 ..." : ctxMenuEnabled ? "移除" : "启用" }}
            </button>
          </span>
        </div>
        <div class="info-row">
          <span class="label">WorkBuddy 菜单</span>
          <span class="value ctx-menu-value">
            <span
              class="ctx-state"
              :class="{ on: wbStatus !== null && wbStatus.found.length === 0, warn: wbStatus !== null && wbStatus.found.length > 0 }"
              title="WorkBuddy 桌面右键菜单的注册残留（HKLM/HKCU 共 4 处注册表键），可一键清除"
            >
              <template v-if="wbStatus === null">检测中 ...</template>
              <template v-else-if="wbStatus.found.length === 0">未发现</template>
              <template v-else>
                发现 {{ wbStatus.found.length }} 处<template v-if="wbStatus.needs_admin">（需管理员）</template>
              </template>
            </span>
            <button
              class="ctx-toggle"
              :disabled="wbBusy"
              @click="wbStatus !== null && wbStatus.found.length > 0 ? removeWorkBuddy() : loadWorkBuddyStatus()"
            >
              {{ wbBusy ? "处理中 ..." : wbStatus !== null && wbStatus.found.length > 0 ? "一键去除" : "重新检测" }}
            </button>
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
    </div>

    <div v-show="activeTab === 'release'" class="tab-panel">
    <section class="card">
      <div class="card-head">
        <h2>版本发布</h2>
        <button
          class="icon-btn"
          :disabled="releaseInfoLoading || releasing"
          title="重新读取仓库信息"
          @click="loadReleaseInfo"
        >
          <svg :class="{ spinning: releaseInfoLoading }" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M21 12a9 9 0 1 1-2.64-6.36" />
            <path d="M21 3v6h-6" />
          </svg>
        </button>
      </div>
      <div class="release-form">
        <div class="rel-row">
          <span class="label">项目目录</span>
          <div class="rel-input-wrap">
            <input
              v-model="releaseDir"
              class="rel-input mono"
              placeholder="D:\ViteProjects\icon-cache-rebuilder"
              spellcheck="false"
              :disabled="releasing"
              @change="loadReleaseInfo"
            />
            <button class="ctx-toggle" :disabled="releasing" @click="pickReleaseDir">浏览</button>
          </div>
        </div>
        <div class="rel-row">
          <span class="label">发布版本</span>
          <div class="rel-input-wrap">
            <input
              v-model="releaseVersion"
              class="rel-input mono"
              placeholder="v0.9.1"
              spellcheck="false"
              :disabled="releasing"
              title="默认在最新标签基础上补丁位 +1，可手动修改"
            />
          </div>
        </div>
        <p class="rel-hint">
          <template v-if="releaseInfo">
            最新标签 <span class="mono-inline">{{ releaseInfo.current_tag }}</span>
            · 分支 {{ releaseInfo.branch }} ·
            <span :class="releaseInfo.changed_files > 0 ? 'wb-warn' : ''">
              {{ releaseInfo.changed_files > 0 ? `${releaseInfo.changed_files} 处待提交改动` : "工作区无改动" }}
            </span>
          </template>
          <template v-else-if="releaseInfoError">
            <span class="wb-err">{{ releaseInfoError }}</span>
          </template>
          <template v-else>正在读取仓库信息 ...</template>
        </p>
        <div class="rel-row">
          <span class="label">提交说明</span>
          <div class="rel-input-wrap">
            <input
              v-model="releaseMessage"
              class="rel-input"
              placeholder="chore: release vX.Y.Z"
              spellcheck="false"
              :disabled="releasing"
            />
          </div>
        </div>
        <div class="rel-row">
          <span class="label">发布说明</span>
          <div class="rel-input-wrap">
            <input
              v-model="releaseNotes"
              class="rel-input"
              placeholder="选填：本次更新内容，显示在 GitHub Release 页面；留空则用提交说明"
              spellcheck="false"
              :disabled="releasing"
            />
          </div>
        </div>
        <button
          class="primary-btn small release-btn"
          :disabled="releasing || !releaseInfo"
          title="自动执行：提交改动 → 推送 → 打标签 → 推送标签，GitHub Actions 随后自动构建并发布"
          @click="openReleaseConfirm"
        >
          <span v-if="releasing" class="btn-inner"><span class="btn-spinner mini"></span>正在发布 ...</span>
          <span v-else class="btn-inner">发布新版本</span>
        </button>
        <ol v-if="releaseStarted" class="steps release-steps">
          <li v-for="s in releaseSteps" :key="s.step" class="step" :data-status="s.status">
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
      </div>
    </section>
    </div>

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
          :disabled="checkingUpdate || (updateStage === 'downloading' && !updateFailed)"
          title="从 GitHub 检查是否有新版本"
          @click="checkForUpdate()"
        >
          <span v-if="updateStage === 'downloading' && !updateFailed" class="btn-inner"><span class="btn-spinner mini"></span>正在更新 ...</span>
          <span v-else-if="checkingUpdate" class="btn-inner"><span class="btn-spinner mini"></span>检查更新 ...</span>
          <span v-else-if="updateHint === 'latest'" class="hint-ok">已是最新</span>
          <span v-else-if="updateHint === 'error'" class="hint-error">检查失败</span>
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

    <div v-if="showUpdateModal && pendingUpdate" class="modal-overlay" @click.self="closeUpdateModal">
      <div class="modal">
        <div class="modal-icon update">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M12 3v12" />
            <path d="m7 10 5 5 5-5" />
            <path d="M5 21h14" />
          </svg>
        </div>

        <!-- 阶段一：确认更新 -->
        <template v-if="updateStage === 'confirm'">
          <h3>发现新版本 v{{ pendingUpdate.version }}</h3>
          <p class="modal-text">
            当前版本 <span class="mono-inline">v{{ appVersion }}</span>，新版本
            <span class="mono-inline">v{{ pendingUpdate.version }}</span> 已发布。
          </p>
          <p v-if="pendingUpdate.body" class="update-notes">{{ pendingUpdate.body }}</p>
          <p class="modal-tip">下载完成后将自动安装新版本并重启应用。</p>
          <div class="modal-actions">
            <button class="ghost-btn" @click="closeUpdateModal">稍后再说</button>
            <button class="primary-btn small" @click="installUpdate">立即更新</button>
          </div>
        </template>

        <!-- 阶段二：下载进度 / 完成 / 失败（更新日志只在这里展示） -->
        <template v-else>
          <h3>正在更新 v{{ pendingUpdate.version }}</h3>
          <div class="update-progress-head">
            <span class="update-status" :class="{ failed: updateFailed }">
              <template v-if="updateFailed">更新失败</template>
              <template v-else-if="updateStage === 'done'">更新完成</template>
              <template v-else-if="updateProgress > 0">正在下载 {{ updateProgress }}%</template>
              <template v-else>正在下载 ...</template>
            </span>
            <span class="update-percent">{{ updateProgress }}%</span>
          </div>
          <div class="update-progress-bar">
            <div
              class="update-progress-fill"
              :class="{ failed: updateFailed, done: updateStage === 'done' }"
              :style="{ width: updateProgress + '%' }"
            ></div>
          </div>
          <div ref="updateLogPanel" class="update-log">
            <p v-for="(line, i) in updateLogs" :key="i" :class="'lvl-' + line.level">
              <span class="t">{{ line.time }}</span>{{ line.text }}
            </p>
          </div>
          <p v-if="updateStage === 'done'" class="modal-tip center">应用将自动重启，无需其他操作。</p>
          <p v-if="updateFailed" class="modal-tip">可关闭窗口稍后重试，不影响当前版本使用。</p>
          <div v-if="updateFailed" class="modal-actions">
            <button class="ghost-btn" @click="closeUpdateModal">关闭</button>
            <button class="primary-btn small" @click="installUpdate">重试</button>
          </div>
        </template>
      </div>
    </div>

    <div v-if="showReleaseConfirm && releaseInfo" class="modal-overlay" @click.self="showReleaseConfirm = false">
      <div class="modal">
        <div class="modal-icon update">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M4.5 16.5c-1.5 1.26-2 5-2 5s3.74-.5 5-2c.71-.84.7-2.13-.09-2.91a2.18 2.18 0 0 0-2.91-.09z" />
            <path d="m12 15-3-3a22 22 0 0 1 2-3.95A12.88 12.88 0 0 1 22 2c0 2.72-.78 7.5-6 11a22.35 22.35 0 0 1-4 2z" />
            <path d="M9 12H4s.55-3.03 2-4c1.62-1.08 5 0 5 0" />
            <path d="M12 15v5s3.03-.55 4-2c1.08-1.62 0-5 0-5" />
          </svg>
        </div>
        <h3>确认发布 {{ normalizedReleaseVersion }}？</h3>
        <p class="modal-text">
          将对 <span class="mono-inline">{{ releaseDir }}</span> 执行：提交改动 → 推送
          {{ releaseInfo.branch }} → 打附注标签并推送。GitHub Actions 随后自动构建并发布新版本。
        </p>
        <p class="modal-text">
          Release 页面说明：<span class="mono-inline">{{ releaseNotesPreview }}</span>
        </p>
        <p v-if="releaseInfo.changed_files > 0" class="update-notes">{{ releaseInfo.changes_preview.join("\n") }}<template v-if="releaseInfo.changed_files > releaseInfo.changes_preview.length">… 等共 {{ releaseInfo.changed_files }} 处改动</template></p>
        <p v-else class="modal-text">工作区无改动，本次仅推送并打标签。</p>
        <p class="modal-tip">推送需访问 GitHub：请确认已开启代理。</p>
        <div class="modal-actions">
          <button class="ghost-btn" :disabled="releasing" @click="showReleaseConfirm = false">取消</button>
          <button class="primary-btn small" :disabled="releasing" @click="startRelease">确认发布</button>
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

/* ---------- tabs ---------- */
.tabs {
  display: flex;
  gap: 6px;
  padding: 4px;
  border-radius: 12px;
  background: rgba(148, 163, 184, 0.07);
  border: 1px solid rgba(148, 163, 184, 0.12);
}

.tab {
  flex: 1;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  gap: 8px;
  padding: 9px 14px;
  border: none;
  border-radius: 9px;
  font-size: 13.5px;
  font-weight: 500;
  font-family: inherit;
  letter-spacing: 1px;
  color: #94a3b8;
  background: transparent;
  cursor: pointer;
  transition: all 0.15s ease;
}

.tab:hover {
  color: #cbd5e1;
}

.tab.active {
  color: #fff;
  background: linear-gradient(135deg, rgba(14, 165, 233, 0.35), rgba(99, 102, 241, 0.35));
  box-shadow: 0 3px 12px rgba(56, 128, 248, 0.25);
}

.tab svg {
  width: 15px;
  height: 15px;
}

.tab-panel {
  display: flex;
  flex-direction: column;
  gap: 12px;
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
  width: 110px;
  white-space: nowrap;
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

/* ---------- context menu toggle ---------- */
.ctx-menu-value {
  display: inline-flex;
  align-items: center;
  gap: 10px;
}

.ctx-state {
  color: #64748b;
}

.ctx-state.on {
  color: #6ee7b7;
}

.ctx-state.warn {
  color: #fbbf24;
}

.wb-warn {
  color: #fbbf24;
}

.wb-err {
  color: #f87171;
}

/* ---------- release form ---------- */
.release-form {
  display: flex;
  flex-direction: column;
}

.rel-row {
  display: flex;
  align-items: center;
  gap: 12px;
  margin-bottom: 8px;
  font-size: 13px;
}

.rel-row .label {
  flex-shrink: 0;
  width: 96px;
  color: #94a3b8;
}

.rel-input-wrap {
  display: flex;
  align-items: center;
  gap: 8px;
  flex: 1;
  min-width: 0;
}

.rel-input {
  flex: 1;
  min-width: 0;
  border: 1px solid rgba(148, 163, 184, 0.22);
  border-radius: 8px;
  background: rgba(2, 6, 23, 0.55);
  color: #e2e8f0;
  font-size: 12.5px;
  font-family: inherit;
  padding: 6px 10px;
  outline: none;
  transition: border-color 0.15s ease;
}

.rel-input:focus {
  border-color: rgba(56, 189, 248, 0.5);
}

.rel-input:disabled {
  opacity: 0.55;
  cursor: not-allowed;
}

.rel-input.mono,
.mono {
  font-family: "Cascadia Code", Consolas, "JetBrains Mono", monospace;
}

.rel-hint {
  margin: -2px 0 10px 108px;
  font-size: 11.5px;
  color: #64748b;
  min-height: 16px;
}

.release-btn {
  width: 100%;
  margin-top: 2px;
}

.release-steps {
  margin-top: 10px;
}

.release-steps .step {
  padding: 4px 0;
}

.release-steps .step-icon {
  width: 23px;
  height: 23px;
  font-size: 11px;
}

.release-steps .step-icon svg {
  width: 12px;
  height: 12px;
}

.release-steps .step-name {
  font-size: 12.5px;
  line-height: 23px;
}

.release-steps .step-msg {
  font-size: 11.5px;
}

.release-steps .step:not(:last-child)::before {
  display: none;
}

.ctx-toggle {
  border: 1px solid rgba(148, 163, 184, 0.22);
  border-radius: 999px;
  padding: 2px 12px;
  font-size: 11.5px;
  font-family: inherit;
  letter-spacing: 1px;
  color: #94a3b8;
  background: transparent;
  cursor: pointer;
  transition: all 0.15s ease;
}

.ctx-toggle:hover:not(:disabled) {
  color: #7dd3fc;
  border-color: rgba(56, 189, 248, 0.5);
}

.ctx-toggle:disabled {
  opacity: 0.55;
  cursor: not-allowed;
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

/* ---------- 更新进度弹出框 ---------- */
.update-progress-head {
  display: flex;
  justify-content: space-between;
  align-items: baseline;
  font-size: 12px;
  color: #94a3b8;
  margin-bottom: 8px;
}

.update-status {
  color: #e2e8f0;
}

.update-status.failed {
  color: #f87171;
}

.update-percent {
  font-family: "Cascadia Code", Consolas, monospace;
  color: #34d399;
}

.update-progress-bar {
  height: 8px;
  border-radius: 999px;
  background: rgba(2, 6, 23, 0.55);
  border: 1px solid rgba(148, 163, 184, 0.14);
  overflow: hidden;
}

.update-progress-fill {
  height: 100%;
  border-radius: 999px;
  background: linear-gradient(90deg, #3b82f6, #34d399);
  transition: width 0.2s ease;
}

.update-progress-fill.done {
  background: #34d399;
}

.update-progress-fill.failed {
  background: #f87171;
}

.update-log {
  margin-top: 10px;
  max-height: 130px;
  overflow-y: auto;
  padding: 10px 12px;
  font-size: 12px;
  line-height: 1.7;
  color: #94a3b8;
  border-radius: 10px;
  background: rgba(2, 6, 23, 0.55);
  border: 1px solid rgba(148, 163, 184, 0.14);
}

.update-log p .t {
  color: #64748b;
  margin-right: 8px;
  font-variant-numeric: tabular-nums;
}

.update-log p.lvl-success {
  color: #34d399;
}

.update-log p.lvl-error {
  color: #f87171;
}

.modal-tip.center {
  text-align: center;
}

.hint-ok {
  color: #34d399;
}

.hint-error {
  color: #f87171;
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
