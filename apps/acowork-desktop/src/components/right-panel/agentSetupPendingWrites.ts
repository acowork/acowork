export function flushPendingWrites<T extends { agentId: string }>(
  agentId: string,
  timers: Map<string, ReturnType<typeof setTimeout>>,
  pendingValues: Map<string, T>,
  flush: (pending: T) => void,
): string[] {
  const prefix = `${agentId}:`;
  const flushedKeys: string[] = [];

  for (const [key, timer] of timers) {
    if (key.startsWith(prefix)) {
      clearTimeout(timer);
      timers.delete(key);
    }
  }
  for (const [key, pending] of pendingValues) {
    if (!key.startsWith(prefix)) continue;
    pendingValues.delete(key);
    flushedKeys.push(key);
    flush(pending);
  }
  return flushedKeys;
}
