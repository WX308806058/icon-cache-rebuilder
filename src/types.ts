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
