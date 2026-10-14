import React from "react";
import { useTranslation } from "../../i18n/useTranslation";
import { AlertTriangle, RefreshCw } from "lucide-react";
import { log } from "../../lib/logger";

interface ErrorBoundaryProps {
  children: React.ReactNode;
}

interface ErrorBoundaryState {
  hasError: boolean;
  error: Error | null;
  autoRetried: boolean;
}

// ── Error display component (function component to use hooks) ─────────────

interface ErrorDisplayProps {
  error: Error | null;
  autoRetried: boolean;
  onRetry: () => void;
  onRefresh: () => void;
}

function ErrorDisplay({ error, autoRetried, onRetry, onRefresh }: ErrorDisplayProps) {
  const { t } = useTranslation();

  return (
    <div className="flex h-screen w-screen items-center justify-center bg-page-bg">
      <div className="mx-auto max-w-md text-center">
        <AlertTriangle className="mx-auto h-12 w-12 text-amber-500" />
        <h2 className="mt-4 text-lg font-semibold text-text ">
          {t("errorBoundary.title")}
        </h2>
        <p className="mt-2 text-sm text-text-tertiary ">
          {t("errorBoundary.description")}
        </p>
        {error && (
          <p className="mt-2 max-h-24 overflow-auto rounded bg-zinc-100 p-2 text-xs text-text-tertiary dark:bg-zinc-800  select-text">
            {error.message}
          </p>
        )}
        {autoRetried && (
          <p className="mt-3 text-xs text-text-tertiary ">
            {t("errorBoundary.autoRecovering")}
          </p>
        )}
        {!autoRetried && (
          <div className="mt-6 flex items-center justify-center gap-3">
            <button
              onClick={onRetry}
              className="flex items-center gap-2 rounded-md bg-zinc-200 px-4 py-2 text-sm font-medium text-text-secondary hover:bg-zinc-300 dark:bg-zinc-700  dark:hover:bg-zinc-600"
            >
              <RefreshCw className="h-4 w-4" />
              {t("errorBoundary.retry")}
            </button>
            <button
              onClick={onRefresh}
              className="flex items-center gap-2 rounded-md px-4 py-2 text-sm font-medium text-white"
              style={{ backgroundColor: "var(--color-accent-strong)" }}
              onMouseEnter={(e) => { e.currentTarget.style.filter = "brightness(0.85)"; }}
              onMouseLeave={(e) => { e.currentTarget.style.filter = ""; }}
            >
              <RefreshCw className="h-4 w-4" />
              {t("errorBoundary.refreshPage")}
            </button>
          </div>
        )}
      </div>
    </div>
  );
}

// ── ErrorBoundary class component ────────────────────────────────────────

export class ErrorBoundary extends React.Component<ErrorBoundaryProps, ErrorBoundaryState> {
  // eslint-disable-next-line @typescript-eslint/no-useless-constructor
  constructor(props: ErrorBoundaryProps) {
    super(props);
    this.state = { hasError: false, error: null, autoRetried: false };
  }

  static getDerivedStateFromError(error: Error): ErrorBoundaryState {
    return { hasError: true, error, autoRetried: false };
  }

  componentDidCatch(error: Error, info: React.ErrorInfo) {
    log.error("[ErrorBoundary] Caught render error:", error, info);

    // 首次崩溃时自动尝试恢复（重新挂载子组件），避免无限重试循环
    if (!this.state.autoRetried) {
      this.setState({ autoRetried: true });
      // 延迟一小段时间让错误渲染完成，再自动恢复
      setTimeout(() => this.handleRetry(), 1500);
    }
  }

  handleRetry = () => {
    this.setState({ hasError: false, error: null, autoRetried: false });
  };

  handleRefresh = () => {
    window.location.reload();
  };

  render() {
    if (this.state.hasError) {
      return (
        <ErrorDisplay
          error={this.state.error}
          autoRetried={this.state.autoRetried}
          onRetry={this.handleRetry}
          onRefresh={this.handleRefresh}
        />
      );
    }

    return this.props.children;
  }
}

// ── ChunkLoadBoundary ─────────────────────────────────────────────────────

/**
 * Scoped boundary for lazily loaded subtrees.
 *
 * A failed dynamic import (dev server restarted, transient fetch error) makes
 * `React.lazy` throw while rendering, not just suspend. The only boundary
 * above the editor panel is the app-level `ErrorBoundary` in App.tsx, whose
 * fallback covers the whole window — so a leaf panel that fails to load used
 * to blank the entire UI. This keeps the failure (and its retry) inside the
 * panel area.
 *
 * `React.lazy` caches the rejected promise, so re-rendering the SAME lazy
 * component can never recover. Callers must hand over a fresh lazy component
 * and remount this boundary with a new `key` when retrying.
 */
export class ChunkLoadBoundary extends React.Component<
  { fallback: React.ReactNode; children: React.ReactNode },
  { failed: boolean }
> {
  state = { failed: false };

  static getDerivedStateFromError() {
    return { failed: true };
  }

  componentDidCatch(error: unknown, info: React.ErrorInfo) {
    log.error("[ChunkLoadBoundary] lazy chunk failed to load:", error, info);
  }

  render() {
    return this.state.failed ? this.props.fallback : this.props.children;
  }
}
