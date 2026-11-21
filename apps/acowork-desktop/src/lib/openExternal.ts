import { open } from "@tauri-apps/plugin-shell";

/**
 * Hand a URL to the operating system's default browser.
 *
 * The one blessed path for "leave the app": `shell:allow-open` is granted to the
 * main webview only (capabilities/default.json), and the plugin validates the
 * scheme against its scope before spawning anything. Preview tabs, `<a>` clicks
 * and error fallbacks all funnel through here so there is a single place where
 * an outbound navigation can happen.
 */
export function openExternal(url: string): Promise<void> {
    return open(url);
}