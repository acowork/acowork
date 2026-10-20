import { useState, useEffect, useCallback, useRef } from "react";
import { createPortal } from "react-dom";
import { invoke } from "@tauri-apps/api/core";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { KeyRound, LogOut, Smartphone } from "lucide-react";
import { useAuthStore } from "../../stores/authStore";
import { ChangePasswordModal } from "../account/ChangePasswordModal";
import { useUserProfileStore } from "../../stores/userProfileStore";
import { UserAvatar, BUILTIN_ICONS, BUILTIN_ICON_IDS } from "../common/UserAvatar";
import { AuthedImage } from "../common/AuthedImage";
import { fetchSelfProfile, updateSelfProfile } from "../../lib/selfProfile";
import { createPairingQr, NoLanAddressError, type PairingQr } from "../../lib/pairing";
import type { BackendUserProfile, AvatarAssetEntry } from "../../lib/types";
import {
  fetchUserAvatarConfig,
  updateUserAvatarConfig,
  fetchUserAvatarAssets,
  deleteUserAvatarFile,
  resolveUserAvatarFileUrl,
} from "../../lib/avatar";
import type { UserAvatarConfig } from "../../lib/avatar";
import { useTranslation } from "../../i18n/useTranslation";
import { StyledInput } from "../common/StyledInput";
import { Dropdown } from "../common/Dropdown";
import { OverlayScrollbar } from "../common/OverlayScrollbar";
import { ExpandableRow, ListBox } from "../common/list";
import i18n from "../../i18n";
import { log } from "../../lib/logger";

const TIMEZONES = [
  "Asia/Shanghai",
  "Asia/Tokyo",
  "America/New_York",
  "America/Los_Angeles",
  "Europe/London",
  "UTC",
];

const IMAGE_EXTENSIONS = ["png", "jpg", "jpeg", "gif", "webp", "svg"];

// ── Component ───────────────────────────────────────────────────────────

export function ProfileTab() {
  const { t } = useTranslation();
  const { profile, setProfile } = useUserProfileStore();
  const [nameValue, setNameValue] = useState(profile.displayName);

  // ── Avatar state ───────────────────────────────────────────────────
  const [avatarPopupOpen, setAvatarPopupOpen] = useState(false);
  const [avatarTab, setAvatarTab] = useState<"custom" | "builtin">("builtin");
  const [avatarConfig, setAvatarConfig] = useState<UserAvatarConfig | null>(null);
  const [avatarAssets, setAvatarAssets] = useState<AvatarAssetEntry[]>([]);
  const [avatarBusy, setAvatarBusy] = useState(false);

  const languages = [
    { value: "zh-CN", label: t("language.zhCN") },
    { value: "zh-TW", label: t("language.zhTW") },
    { value: "en", label: t("language.en") },
    { value: "ja", label: t("language.ja") },
    { value: "ko", label: t("language.ko") },
  ];

  // ── Load avatar config from backend ────────────────────────────────
  useEffect(() => {
    fetchUserAvatarConfig()
      .then((cfg) => {
        setAvatarConfig(cfg);
        setProfile({
          backendAvatarUrl: cfg.avatar ?? null,
          backendBuiltinAvatarId: cfg.builtin_avatar ?? null,
        });
      })
      .catch(() => {});
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  // Refresh avatar assets when popup opens
  useEffect(() => {
    if (!avatarPopupOpen) return;
    fetchUserAvatarAssets()
      .then((r) => setAvatarAssets(r.assets))
      .catch(() => {});
  }, [avatarPopupOpen]);

  // ── Avatar picker popup positioning ─────────────────────────────
  // The picker popup used to be `absolute` inside the avatar button's
  // `.relative` wrapper. The ProfileTab wraps everything in a
  // collapsible `ListBox` card (`overflow-hidden` so the rounded card
  // clips its children — see ListBox.tsx), which clipped the popup.
  // Fix mirrors the right-panel AgentSetupTab: render the popup via
  // `createPortal` to `document.body` with `position: fixed`, anchored
  // to the avatar button's measured viewport rect.
  //   - `avatarAnchorRef` → the `.relative` wrapper around the button
  //   - `avatarPopupPos` → { top, left } viewport coords for the popup
  //   - Position is computed synchronously on open (no first-frame
  //     flash) and re-computed on resize / capture-phase scroll so the
  //     popup tracks the button when the panel or page scrolls.
  const avatarAnchorRef = useRef<HTMLDivElement>(null);
  const [avatarPopupPos, setAvatarPopupPos] = useState<{ top: number; left: number } | null>(null);

  const computeAvatarPopupPos = useCallback(() => {
    const el = avatarAnchorRef.current;
    if (!el) return;
    const rect = el.getBoundingClientRect();
    // 8px gap below the avatar button — same visual as the old
    // `absolute left-0 top-full mt-2` placement.
    setAvatarPopupPos({ top: rect.bottom + 8, left: rect.left });
  }, []);

  useEffect(() => {
    if (!avatarPopupOpen) return;
    computeAvatarPopupPos();
    window.addEventListener("resize", computeAvatarPopupPos);
    // Capture-phase scroll: catches scrolling of the settings panel /
    // card containers, not just the window, so the fixed popup stays
    // glued to the avatar button.
    window.addEventListener("scroll", computeAvatarPopupPos, true);
    return () => {
      window.removeEventListener("resize", computeAvatarPopupPos);
      window.removeEventListener("scroll", computeAvatarPopupPos, true);
    };
  }, [avatarPopupOpen, computeAvatarPopupPos]);

  // ── Load backend user profile ──────────────────────────────────────
  const [backendUser, setBackendUser] = useState<BackendUserProfile | null>(null);
  const [backendLoading, setBackendLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [savedMsg, setSavedMsg] = useState<string | null>(null);

  // Form draft state for backend fields
  const [language, setLanguage] = useState("zh-CN");
  const [timezone, setTimezone] = useState("Asia/Shanghai");
  const [city, setCity] = useState("");
  const [occupation, setOccupation] = useState("");

  // Load the caller's own profile from backend on mount. Mode-aware
  // (`lib/selfProfile.ts`): `GET /api/auth/me` under multi_user, the
  // active-user record under local.
  useEffect(() => {
    let cancelled = false;
    fetchSelfProfile()
      .then((user) => {
        if (cancelled) return;
        setBackendUser(user);
        if (user) {
          setLanguage(user.language);
          // NOTE: do NOT call i18n.changeLanguage here. The global UI language
          // is a client-side choice (initialized from localStorage/navigator and
          // updated only by the dropdown's onChange). Silently overriding it
          // with the backend profile language caused the UI to flip back to
          // English after switching views when the backend value was stale.
          setTimezone(user.timezone);
          setCity(user.city ?? "");
          setOccupation(user.occupation ?? "");
          // Sync display name from backend to local store
          if (user.display_name) {
            setProfile({ displayName: user.display_name });
            setNameValue(user.display_name);
          }
        }
      })
      .catch(() => {
        // Gateway not reachable or no users yet — use local state
      })
      .finally(() => {
        if (!cancelled) setBackendLoading(false);
      });
    return () => { cancelled = true; };
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  // ── Save helpers ──────────────────────────────────────────────────

  /** Save a single field to backend (debounced via onBlur) */
  const saveField = useCallback(async (field: string, value: string) => {
    setSaving(true);
    setSavedMsg(null);
    try {
      const updated = await updateSelfProfile({ [field]: value });
      setBackendUser(updated);
      setSavedMsg("saved");
      setTimeout(() => setSavedMsg(null), 2000);
    } catch (err) {
      log.warn(`Failed to save ${field}:`, err);
      setSavedMsg("failed");
    } finally {
      setSaving(false);
    }
  }, []);

  /** Save display name to both backend and local store */
  const saveDisplayName = useCallback((value: string) => {
    const trimmed = value.trim();
    if (!trimmed) return;
    // Always update local store
    setProfile({ displayName: trimmed });
    // Also update backend if we have a user
    if (backendUser) {
      saveField("display_name", trimmed);
    }
  }, [backendUser, setProfile, saveField]);

  // ── Avatar handlers ───────────────────────────────────────────────

  const handleSelectCustom = async (relativePath: string) => {
    setAvatarBusy(true);
    try {
      const cfg = await updateUserAvatarConfig({ avatar: relativePath, builtin_avatar: "" });
      setAvatarConfig(cfg);
      setProfile({
        backendAvatarUrl: cfg.avatar ?? null,
        backendBuiltinAvatarId: cfg.builtin_avatar ?? null,
      });
    } catch (err) {
      log.warn("[ProfileTab] Select custom avatar failed:", err);
    } finally {
      setAvatarBusy(false);
      setAvatarPopupOpen(false);
    }
  };

  const handleSelectBuiltin = async (iconId: string) => {
    setAvatarBusy(true);
    try {
      const cfg = await updateUserAvatarConfig({ avatar: "", builtin_avatar: iconId });
      setAvatarConfig(cfg);
      setProfile({
        backendAvatarUrl: cfg.avatar ?? null,
        backendBuiltinAvatarId: cfg.builtin_avatar ?? null,
      });
    } catch (err) {
      log.warn("[ProfileTab] Select builtin avatar failed:", err);
    } finally {
      setAvatarBusy(false);
      setAvatarPopupOpen(false);
    }
  };

  const handleUploadClick = async () => {
    const selected = await openDialog({
      multiple: false,
      filters: [{ name: "Images", extensions: IMAGE_EXTENSIONS }],
    });
    if (!selected || typeof selected !== "string") return;

    setAvatarBusy(true);
    try {
      await invoke("upload_user_avatar_file", { filePath: selected });
      const resp = await fetchUserAvatarAssets();
      setAvatarAssets(resp.assets);
    } catch (err) {
      log.warn("[ProfileTab] Avatar upload failed:", err);
    } finally {
      setAvatarBusy(false);
    }
  };

  const handleDeleteAvatar = async (relativePath: string) => {
    setAvatarBusy(true);
    try {
      await deleteUserAvatarFile(relativePath);
      const [assetsResp, cfg] = await Promise.all([
        fetchUserAvatarAssets(),
        fetchUserAvatarConfig(),
      ]);
      setAvatarAssets(assetsResp.assets);
      setAvatarConfig(cfg);
    } catch (err) {
      log.warn("[ProfileTab] Delete avatar failed:", err);
    } finally {
      setAvatarBusy(false);
      setAvatarPopupOpen(false);
    }
  };

  // ── Render ────────────────────────────────────────────────────────
  // Tools-tab style level-1 collapsible cards (default open)
  const [profileOpen, setProfileOpen] = useState(true);
  const [identityOpen, setIdentityOpen] = useState(true);

  // Account actions (see the buttons next to the avatar below).
  const loggedIn = useAuthStore((s) => s.status === "logged_in");
  const [changePasswordOpen, setChangePasswordOpen] = useState(false);

  return (
    <div className="max-w-lg space-y-4">
      <ChangePasswordModal open={changePasswordOpen} onClose={() => setChangePasswordOpen(false)} />

      {/* ── Avatar & Display Name ────────────────────────────────── */}
      <ListBox dividers={false}>
        <ExpandableRow
          open={profileOpen}
          onToggle={() => setProfileOpen((v) => !v)}
          title={t("settings.profileTitle")}
          ariaLabel={t("settings.profileTitle")}
          bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset p-3"
        >

        {/* Avatar preview — click to open picker popup */}
        <div className="flex items-center gap-4">
          <div className="relative" ref={avatarAnchorRef}>
            <button
              onClick={() => {
                if (!avatarPopupOpen) computeAvatarPopupPos();
                setAvatarPopupOpen((v) => !v);
              }}
              className="relative block rounded-lg ring-1 ring-zinc-300/60 transition hover:ring-zinc-400 dark:ring-zinc-600/60 dark:hover:ring-zinc-400"
            >
              <UserAvatar
                size={64}
                avatarUrl={avatarConfig?.avatar ?? null}
                builtinAvatarId={avatarConfig?.builtin_avatar ?? null}
              />
              {/* Pencil badge */}
              <span className="absolute -bottom-0.5 -right-0.5 flex h-5 w-5 items-center justify-center rounded-full bg-zinc-800 text-white shadow-sm dark:bg-zinc-600">
                <svg viewBox="0 0 16 16" className="h-3 w-3 fill-current" xmlns="http://www.w3.org/2000/svg">
                  <path d="M11.013 1.427a1.75 1.75 0 0 1 2.474 0l1.086 1.086a1.75 1.75 0 0 1 0 2.474l-8.61 8.61c-.21.21-.47.364-.756.445l-3.251.93a.75.75 0 0 1-.927-.928l.929-3.25c.081-.286.235-.547.445-.758l8.61-8.61Zm.176 4.823L11.5 7l-3-3-.31.31a.75.75 0 0 0-.177.764l.93 3.251a.75.75 0 0 1-.927.928l-3.251-.93Z" />
                </svg>
              </span>
            </button>

            {/* Avatar picker popup — rendered via createPortal to
                  document.body so the ListBox card's overflow-hidden
                  does not clip it (same pattern as the right-panel
                  AgentSetupTab and the Tooltip/ContextMenu). `position:
                  fixed` anchors to the avatar button's measured viewport
                  coords (avatarPopupPos). */}
            {avatarPopupOpen &&
              createPortal(
                <>
                {/* Click-outside overlay */}
                <div
                  className="fixed inset-0 z-40"
                  onClick={() => setAvatarPopupOpen(false)}
                />
                <div
                  className="fixed z-50"
                  style={{ top: avatarPopupPos?.top ?? 0, left: avatarPopupPos?.left ?? 0 }}
                >
                  <OverlayScrollbar
                    maxHeight="min(calc(100vh-120px),460px)"
                    className="p-3 overscroll-contain"
                    containerClassName="w-72 overflow-hidden rounded-lg border border-border-outer bg-modal-surface shadow-lg"
                  >
                  {/* Tabs */}
                  <div className="mb-3 flex gap-1 border-b border-border-divider">
                    <button
                      onClick={() => setAvatarTab("custom")}
                      className={`px-3 py-1 text-xs font-medium transition-colors ${avatarTab === "custom"
                        ? "border-b-2 border-zinc-800 text-text dark:border-zinc-200 "
                        : "text-text-tertiary hover:text-zinc-600 "
                        }`}
                    >
                      Custom
                    </button>
                    <button
                      onClick={() => setAvatarTab("builtin")}
                      className={`px-3 py-1 text-xs font-medium transition-colors ${avatarTab === "builtin"
                        ? "border-b-2 border-zinc-800 text-text dark:border-zinc-200 "
                        : "text-text-tertiary hover:text-zinc-600 "
                        }`}
                    >
                      Builtin
                    </button>
                  </div>

                  {/* Custom tab */}
                  {avatarTab === "custom" && (
                    <div className="grid grid-cols-4 gap-2">
                      <button
                        onClick={handleUploadClick}
                        disabled={avatarBusy}
                        className="flex aspect-square items-center justify-center rounded-md border border-dashed border-zinc-300 text-text-tertiary transition-colors hover:border-zinc-400 hover:text-zinc-600 disabled:opacity-50 dark:border-zinc-600  dark:hover:border-zinc-400"
                      >
                        <span className="text-lg">+</span>
                      </button>
                      {avatarAssets.map((asset) => {
                        const isSelected = avatarConfig?.avatar === asset.relative_path;
                        return (
                          <div
                            key={asset.relative_path}
                            className={`group relative aspect-square overflow-hidden rounded-md border-2 transition-colors ${isSelected
                              ? "border-zinc-800 dark:border-zinc-200"
                              : "border-transparent hover:border-zinc-300 dark:hover:border-zinc-600"
                              }`}
                          >
                            <AuthedImage
                              src={resolveUserAvatarFileUrl(asset.relative_path)}
                              alt={asset.relative_path}
                              className="h-full w-full cursor-pointer object-cover"
                              onClick={() => handleSelectCustom(asset.relative_path)}
                            />
                            <button
                              onClick={(e) => {
                                e.stopPropagation();
                                handleDeleteAvatar(asset.relative_path);
                              }}
                              disabled={avatarBusy}
                              className="absolute right-0.5 top-0.5 flex h-4 w-4 items-center justify-center rounded bg-red-500/80 text-[8px] text-white opacity-0 transition-opacity group-hover:opacity-100"
                            >
                              ×
                            </button>
                          </div>
                        );
                      })}
                    </div>
                  )}

                  {/* Builtin tab */}
                  {avatarTab === "builtin" && (
                    <div className="grid grid-cols-4 gap-2">
                      {BUILTIN_ICON_IDS.map((iconId) => {
                        const isSelected = avatarConfig?.builtin_avatar === iconId;
                        return (
                          <button
                            key={iconId}
                            onClick={() => handleSelectBuiltin(iconId)}
                            disabled={avatarBusy}
                            className={`flex items-center justify-center rounded-md p-1 transition-colors ${isSelected
                              ? "bg-zinc-200 dark:bg-zinc-600"
                              : "hover:bg-zinc-100 dark:hover:bg-zinc-700"
                              }`}
                          >
                            <img
                              src={BUILTIN_ICONS[iconId] ?? ""}
                              alt={iconId}
                              draggable={false}
                              className="h-12 w-12 rounded-lg object-cover"
                            />
                          </button>
                        );
                      })}
                    </div>
                  )}
                  </OverlayScrollbar>
                </div>
                </>,
                document.body,
              )}
          </div>
          <div>
            <p className="text-sm font-medium text-text ">
              {profile.displayName}
            </p>
          </div>
          {/* Account actions — the same two the avatar's right-click menu
              offers (AccountMenu.tsx), duplicated here because the menu is
              now right-click only and the profile page is the left-click
              target. `ml-auto` parks them on the row's trailing edge, well
              clear of the avatar + name. Hidden when there is no account to
              act on (AUTH_MODE=local / not logged in).

              The two buttons share the app's row-trailing-action grammar
              (`inline-flex … gap-1 rounded btn-solid px-2 py-1 text-11
              font-medium` — the same string as every Refresh / Check /
              Add-Server button in settings + harness). They used to be
              the app's only `btn-outline` users, which rendered them as
              bordered ghost buttons against every other secondary action's
              solid fill. `btn-solid` is also the neutral tone on purpose:
              logout is a routing action here (AccountMenu.tsx renders it
              neutral too), not a destructive one — 注销 / delete is what
              carries danger, and that stays red. */}
          {loggedIn && (
            <div className="ml-auto flex flex-col gap-1.5">
              <button
                type="button"
                onClick={() => setChangePasswordOpen(true)}
                className="inline-flex shrink-0 items-center gap-1 rounded btn-solid px-2 py-1 text-11 font-medium"
              >
                <KeyRound size={12} />
                {t("account.changePassword")}
              </button>
              <button
                type="button"
                onClick={() => void useAuthStore.getState().logout()}
                className="inline-flex shrink-0 items-center gap-1 rounded btn-solid px-2 py-1 text-11 font-medium"
              >
                <LogOut size={12} />
                {t("account.logout")}
              </button>
            </div>
          )}
        </div>

        {/* Display name */}
        <div className="mt-3 space-y-1.5">
          <label className="block text-xs font-medium text-text-secondary ">
            {t("settings.displayName")}
          </label>
          <StyledInput
            type="text"
            value={nameValue}
            onChange={(e) => setNameValue(e.target.value)}
            onBlur={() => saveDisplayName(nameValue)}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                saveDisplayName(nameValue);
                (e.target as HTMLInputElement).blur();
              }
            }}
            placeholder={t("settings.displayNamePlaceholder")}
            className="rounded py-2 text-text placeholder:text-text-tertiary"
          />
        </div>
        </ExpandableRow>
      </ListBox>

      {/* ── Backend Identity Fields ───────────────────────────────── */}
      <ListBox dividers={false}>
        <ExpandableRow
          open={identityOpen}
          onToggle={() => setIdentityOpen((v) => !v)}
          title={t("settings.identityTitle")}
          ariaLabel={t("settings.identityTitle")}
          trailing={
            <span onClick={(e) => e.stopPropagation()} className="flex shrink-0 items-center gap-2">
              {savedMsg && (
                <span className={`text-10 ${savedMsg === "saved" ? "text-[var(--color-accent)]" : "text-red-500"}`}>
                  {savedMsg === "saved" ? t("settings.saved") : t("settings.saveFailed")}
                </span>
              )}
              {saving && <span className="text-10 text-text-tertiary">{t("settings.saving")}</span>}
            </span>
          }
          bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset p-3"
        >

        {backendLoading ? (
          <p className="text-xs text-text-tertiary">{t("settings.loading")}</p>
        ) : backendUser ? (
          <div className="space-y-3">
            {/* Language */}
            <div>
              <label className="mb-1 block text-xs text-text-tertiary">{t("settings.language")}</label>
              <Dropdown
                value={language}
                onChange={(lng) => {
                  log.debug("[ProfileTab] Switching language to:", lng);
                  setLanguage(lng);
                  i18n.changeLanguage(lng).then(() => {
                    log.debug("[ProfileTab] changeLanguage resolved, i18n.language =", i18n.language);
                  });
                  if (backendUser?.user_id) {
                    saveField("language", lng);
                  }
                }}
                options={languages.map((l) => ({ value: l.value, label: l.label }))}
              />
            </div>

            {/* Timezone */}
            <div>
              <label className="mb-1 block text-xs text-text-tertiary">{t("settings.timezone")}</label>
              <Dropdown
                value={timezone}
                onChange={(tz) => {
                  setTimezone(tz);
                  saveField("timezone", tz);
                }}
                options={TIMEZONES.map((tz) => ({ value: tz, label: tz }))}
              />
            </div>

            {/* City */}
            <div>
              <label className="mb-1 block text-xs text-text-tertiary">{t("settings.city")}</label>
              <StyledInput
                type="text"
                value={city}
                onChange={(e) => setCity(e.target.value)}
                onBlur={() => saveField("city", city.trim())}
                onKeyDown={(e) => {
                  if (e.key === "Enter") {
                    saveField("city", city.trim());
                    (e.target as HTMLInputElement).blur();
                  }
                }}
                placeholder={t("settings.cityPlaceholder")}
                className="rounded py-2 text-text placeholder:text-text-tertiary"
              />
            </div>

            {/* Occupation */}
            <div>
              <label className="mb-1 block text-xs text-text-tertiary">{t("settings.occupation")}</label>
              <StyledInput
                type="text"
                value={occupation}
                onChange={(e) => setOccupation(e.target.value)}
                onBlur={() => saveField("occupation", occupation.trim())}
                onKeyDown={(e) => {
                  if (e.key === "Enter") {
                    saveField("occupation", occupation.trim());
                    (e.target as HTMLInputElement).blur();
                  }
                }}
                placeholder={t("settings.occupationPlaceholder")}
                className="rounded py-2 text-text placeholder:text-text-tertiary"
              />
            </div>
          </div>
        ) : (
          <p className="text-xs text-text-tertiary">
            {t("settings.noProfile")}
          </p>
        )}
        </ExpandableRow>
      </ListBox>

      {/* ── Connect phone (QR pairing, 方案③) ── only when signed in ── */}
      {loggedIn && <PairingCard />}
    </div>
  );
}

// ── Pairing QR card ───────────────────────────────────────────────────
// Self-contained: owns the generate / countdown / refresh cycle so the main
// ProfileTab render stays focused on the profile form. Rendered only for a
// signed-in account (pairing is bound to the bearer identity server-side).
function PairingCard() {
  const { t } = useTranslation();
  const [open, setOpen] = useState(true);
  const [qr, setQr] = useState<PairingQr | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<"no-lan" | "failed" | null>(null);
  const [now, setNow] = useState(Date.now());

  // Tick once a second while a code is on screen, so the countdown and the
  // expiry state advance without a re-render driven from outside.
  useEffect(() => {
    if (!qr) return;
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, [qr]);

  const secsLeft = qr ? Math.max(0, Math.ceil((qr.expiresAt - now) / 1000)) : 0;
  const expired = qr != null && secsLeft <= 0;

  const generate = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      setQr(await createPairingQr());
      setNow(Date.now());
    } catch (err) {
      setQr(null);
      if (err instanceof NoLanAddressError) {
        setError("no-lan");
      } else {
        setError("failed");
        log.warn("[PairingCard] generate failed:", err);
      }
    } finally {
      setBusy(false);
    }
  }, []);

  return (
    <ListBox dividers={false}>
      <ExpandableRow
        open={open}
        onToggle={() => setOpen((v) => !v)}
        title={t("settings.pairingTitle")}
        ariaLabel={t("settings.pairingTitle")}
        bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset p-3"
      >
        <p className="text-xs text-text-tertiary">{t("settings.pairingHelp")}</p>
        <div className="mt-3 flex flex-col items-center gap-3">
          {qr && !expired ? (
            <>
              <img
                src={qr.dataUrl}
                alt={t("settings.pairingTitle")}
                className="h-52 w-52 rounded bg-white p-2"
              />
              <p className="text-xs text-text-secondary">
                {t("settings.pairingExpires", { secs: secsLeft })}
              </p>
              <p className="break-all text-center text-10 text-text-tertiary">
                {t("settings.pairingAddress", { url: qr.baseUrl })}
              </p>
            </>
          ) : qr && expired ? (
            <p className="text-xs text-red-500">{t("settings.pairingExpired")}</p>
          ) : error === "no-lan" ? (
            <p className="text-center text-xs text-red-500">{t("settings.pairingNoLan")}</p>
          ) : error === "failed" ? (
            <p className="text-xs text-red-500">{t("settings.pairingFailed")}</p>
          ) : null}

          <button
            type="button"
            onClick={() => void generate()}
            disabled={busy}
            className="flex items-center justify-center gap-1.5 rounded btn-solid px-3 py-1.5 text-xs font-medium disabled:opacity-50"
          >
            <Smartphone size={14} />
            {qr ? t("settings.pairingRefresh") : t("settings.pairingGenerate")}
          </button>
        </div>
      </ExpandableRow>
    </ListBox>
  );
}
