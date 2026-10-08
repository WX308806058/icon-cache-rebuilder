export type StepStatus = "idle" | "running" | "success" | "warning" | "failed";

export interface StepRecord {
  step: number;
  name: string;
  status: StepStatus;
  message: string;
}

export interface RebuildResult {
  success: boolean;
  total_deleted: number;
  failed_files: string[];
  steps: StepRecord[];
}

export interface CacheInfo {
  explorer_dir: string;
  file_count: number;
  total_size: number;
  icon_cache_db: number | null;
}

export interface WorkBuddyStatus {
  /** 仍存在的 WorkBuddy 右键菜单注册键（展示名） */
  found: string[];
  /** 存在系统级（HKLM）键，删除需要管理员权限 */
  needs_admin: boolean;
}

export interface WorkBuddyRemoveResult {
  removed: string[];
  missing: string[];
  failed: string[];
  /** 有键因权限不足未删除，需要提权（UAC）重试 */
  needs_elevation: boolean;
}

export interface ReleaseInfo {
  /** 仓库最新的版本标签（如 v0.9.0） */
  current_tag: string;
  branch: string;
  /** 建议的下一版本号（补丁位 +1），可手动修改 */
  next_version: string;
  /** 工作区待提交的改动数量 */
  changed_files: number;
  /** 改动预览（最多 8 条） */
  changes_preview: string[];
}

export interface ReleaseResult {
  success: boolean;
  tag: string;
  branch: string;
  /** 本次是否产生了新提交（工作区无改动时跳过提交） */
  committed: boolean;
  steps: StepRecord[];
}
