//! ACowork Mobile — Tauri v2 native shell.
//!
//! Deliberately near-empty, and that is the design, not an omission.
//! Desktop's backend spawns and supervises a local Gateway, owns the system
//! tray, and runs the LSP relay sidecar. A phone has none of those: the
//! Gateway it talks to is remote, there is no tray, and the LSP relay is
//! Desktop-only (see ADR-086 §v1 排除项).
//!
//! So the mobile crate owns **no** platform concerns beyond hosting the
//! webview — with one exception: `system_bar_insets` below, which is the
//! status-bar clearance the WebView cannot report for itself on Android.
//! Every capability it will eventually need — Gateway address,
//! auth token, last-opened tab — is a small piece of app state that
//! `tauri-plugin-store` persists. When the first genuinely native need
//! arrives (push notifications, deep links, camera), it is added here as
//! one audited command rather than as a speculative framework.

/// Where the mobile app last connected. Persisted so a cold start can skip
/// the Gateway-picker screen; `None` means "ask the user".
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GatewayEndpoint {
    pub url: String,
}

/// System-bar clearance in PHYSICAL pixels (the frontend divides by the
/// device scale factor). See `system_bar_insets` for why this exists.
#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct BarInsets {
    pub top: i32,
    pub bottom: i32,
    pub left: i32,
    pub right: i32,
}

/// Status-bar / navigation-bar clearance, read from the live window insets.
///
/// Android's WebView derives `env(safe-area-inset-*)` from the *display
/// cutout* only, so on a notch-less phone every direction reports `0px`
/// while the activity is edge-to-edge (`enableEdgeToEdge()` in the generated
/// `MainActivity`) and the webview really does paint under the status bar —
/// the navbar then lands under the clock and the battery icon. Measured on a
/// HUAWEI BLA-AL00: `mDisplayCutout insets=Rect(0,0-0,0)` against a 72px
/// `ty=STATUS_BAR` window, and all four `env()` values read 0.
///
/// iOS is unaffected (WKWebView maps the real safe area), which is why the
/// non-Android build answers zeros and the frontend keeps `env()` as an
/// equal input to the resolution.
#[tauri::command]
async fn system_bar_insets(app: tauri::AppHandle) -> BarInsets {
    use tauri::Manager;

    let (tx, rx) = std::sync::mpsc::channel();
    if let Some(webview) = app.get_webview_window("main") {
        // Hops to the UI thread, which is where the JNI call may run.
        let _ = webview.with_webview(move |platform| {
            #[cfg(target_os = "android")]
            {
                platform.jni_handle().exec(move |env, _activity, view| {
                    let _ = tx.send(read_insets(env, view));
                });
            }
            #[cfg(not(target_os = "android"))]
            {
                let _ = &platform;
                let _ = tx.send(BarInsets::default());
            }
        });
    }
    // A busy main thread must not stall the first layout: answer "no
    // clearance", which is what the CSS already resolves to anyway.
    rx.recv_timeout(std::time::Duration::from_millis(500))
        .unwrap_or_default()
}

/// The four system-window insets off the WebView's own view, with the
/// framework `status_bar_height` dimen as the floor for the top.
///
/// The floor matters on API < 30: `getRootWindowInsets()` is dispatched
/// asynchronously and can still be null — or all-zero for a fullscreen-layout
/// window — at the moment we ask, and a missing status-bar clearance is the
/// visible bug. The dimen is the real bar height on every vendor ROM.
#[cfg(target_os = "android")]
fn read_insets(
    env: &mut jni::JNIEnv<'_>,
    view: &jni::objects::JObject<'_>,
) -> BarInsets {
    use jni::objects::JValue;

    // Unwrap a `()I` result; anything unexpected reads as 0.
    fn int(v: jni::objects::JValueOwned<'_>) -> i32 {
        v.i().unwrap_or(0)
    }

    // `view.getRootWindowInsets().<name>()`, 0 when nothing is dispatched.
    // Plain functions rather than closures: both need `&mut env`, which two
    // captured borrows held at the same time would not allow.
    fn inset(
        env: &mut jni::JNIEnv<'_>,
        view: &jni::objects::JObject<'_>,
        name: &str,
    ) -> i32 {
        env.call_method(view, "getRootWindowInsets", "()Landroid/view/WindowInsets;", &[])
            .ok()
            .and_then(|v| v.l().ok())
            .filter(|o| !o.is_null())
            .and_then(|o| env.call_method(&o, name, "()I", &[]).ok())
            .map(int)
            .unwrap_or(0)
    }

    // `Resources.getDimensionPixelSize(getIdentifier(name, "dimen", "android"))`.
    fn dimen(
        env: &mut jni::JNIEnv<'_>,
        view: &jni::objects::JObject<'_>,
        name: &str,
    ) -> i32 {
        let Some(res) = env
            .call_method(view, "getResources", "()Landroid/content/res/Resources;", &[])
            .ok()
            .and_then(|v| v.l().ok())
        else {
            return 0;
        };
        let strs = [name, "dimen", "android"].map(|s| env.new_string(s).ok());
        let [Some(n), Some(t), Some(p)] = strs else {
            return 0;
        };
        let id = env
            .call_method(
                &res,
                "getIdentifier",
                "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)I",
                &[JValue::Object(&n), JValue::Object(&t), JValue::Object(&p)],
            )
            .ok()
            .map(int)
            .unwrap_or(0);
        if id == 0 {
            return 0;
        }
        env.call_method(&res, "getDimensionPixelSize", "(I)I", &[JValue::Int(id)])
            .ok()
            .map(int)
            .unwrap_or(0)
    }

    BarInsets {
        top: inset(env, view, "getSystemWindowInsetTop").max(dimen(env, view, "status_bar_height")),
        bottom: inset(env, view, "getSystemWindowInsetBottom"),
        left: inset(env, view, "getSystemWindowInsetLeft"),
        right: inset(env, view, "getSystemWindowInsetRight"),
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_store::Builder::new().build())
        .invoke_handler(tauri::generate_handler![system_bar_insets])
        .run(tauri::generate_context!())
        .expect("error while running ACowork Mobile");
}

/// Standalone crate: the `[workspace]` key in Cargo.toml keeps it out of the
/// core resolver, exactly like `apps/acowork-desktop/src-tauri`.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_round_trips() {
        let e = GatewayEndpoint {
            url: "http://192.168.1.10:19876".into(),
        };
        let json = serde_json::to_string(&e).unwrap();
        let back: GatewayEndpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(back.url, e.url);
    }
}
