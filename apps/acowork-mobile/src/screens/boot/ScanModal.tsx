/**
 * QR pairing scanner (方案③) — Mobile side.
 *
 * Pure-JS WebView camera: `getUserMedia` feeds frames to a hidden canvas,
 * `jsQR` decodes them, and a valid payload is redeemed for a token pair via
 * `authStore.pairLogin` (which also lands the Gateway base URL, so scan
 * completes BOTH "connect" and "login" in one action).
 *
 * The payload is a JSON blob the Desktop renders (`lib/pairing.ts`):
 *   `{ "v": 1, "gw": "<base url>", "pt": "<one-time pairing token>" }`
 * Anything not matching schema v1 is ignored (the loop keeps scanning), so a
 * stray QR never triggers a bogus login.
 *
 * Deliberately NO native Kotlin/Gradle (the user's chosen light path): if the
 * WebView camera cannot start, we degrade to a clear message and the caller
 * keeps the manual address-entry route.
 */
import { useEffect, useRef, useState, type CSSProperties } from 'react'
import jsQR from 'jsqr'
import { useAuthStore } from '../../stores/authStore'
import { useT } from '../../i18n/useT'

/** Decoded pairing QR payload (schema v1). */
interface PairPayload {
  gw: string
  pt: string
}

/** Parse the QR text; `null` for anything that is not a v1 pairing payload. */
export function parsePairPayload(raw: string): PairPayload | null {
  try {
    const o = JSON.parse(raw) as { v?: number; gw?: unknown; pt?: unknown }
    if (o.v === 1 && typeof o.gw === 'string' && typeof o.pt === 'string') {
      return { gw: o.gw, pt: o.pt }
    }
    return null
  } catch {
    return null
  }
}

const overlay: CSSProperties = {
  position: 'fixed',
  inset: 0,
  background: '#000',
  display: 'flex',
  flexDirection: 'column',
  zIndex: 1000,
}

export function ScanModal({ onClose }: { onClose: () => void }) {
  const t = useT()
  const videoRef = useRef<HTMLVideoElement | null>(null)
  const canvasRef = useRef<HTMLCanvasElement | null>(null)
  const [msgKey, setMsgKey] = useState('scan.starting')
  const [failed, setFailed] = useState(false)

  // Keep the latest onClose without making it an effect dependency — an inline
  // arrow from the parent would otherwise restart the camera on every render.
  const onCloseRef = useRef(onClose)
  onCloseRef.current = onClose

  useEffect(() => {
    let raf = 0
    let stream: MediaStream | null = null
    let cancelled = false

    const stop = () => {
      cancelled = true
      cancelAnimationFrame(raf)
      stream?.getTracks().forEach((t) => t.stop())
    }

    const begin = async () => {
      try {
        stream = await navigator.mediaDevices.getUserMedia({
          video: { facingMode: { ideal: 'environment' } },
          audio: false,
        })
      } catch {
        setMsgKey('scan.cameraDenied')
        setFailed(true)
        return
      }
      const v = videoRef.current
      if (!v || cancelled) return
      v.srcObject = stream
      try {
        await v.play()
      } catch {
        /* autoplay can be blocked; the decode loop still reads frames */
      }
      setMsgKey('scan.aim')

      const loop = () => {
        if (cancelled) return
        const vid = videoRef.current
        const cvs = canvasRef.current
        if (vid && cvs && vid.readyState >= 2 && vid.videoWidth > 0) {
          const ctx = cvs.getContext('2d', { willReadFrequently: true })
          if (ctx) {
            cvs.width = vid.videoWidth
            cvs.height = vid.videoHeight
            ctx.drawImage(vid, 0, 0, cvs.width, cvs.height)
            const img = ctx.getImageData(0, 0, cvs.width, cvs.height)
            const code = jsQR(img.data, img.width, img.height, { inversionAttempts: 'dontInvert' })
            const parsed = code?.data ? parsePairPayload(code.data) : null
            if (parsed) {
              // Stop the camera before the network round-trip: the token is
              // single-use, so there is nothing left to scan either way.
              stop()
              setMsgKey('scan.pairing')
              void useAuthStore
                .getState()
                .pairLogin(parsed.gw, parsed.pt)
                .then((ok) => {
                  if (ok) onCloseRef.current()
                  else {
                    setMsgKey(useAuthStore.getState().loginError || 'scan.fallbackError')
                    setFailed(true)
                  }
                })
              return
            }
          }
        }
        raf = requestAnimationFrame(loop)
      }
      loop()
    }

    void begin()
    return stop
  }, [])

  return (
    <div style={overlay}>
      <video
        ref={videoRef}
        playsInline
        muted
        style={{
          position: 'absolute',
          inset: 0,
          width: '100%',
          height: '100%',
          objectFit: 'cover',
          opacity: failed ? 0 : 1,
          // A full-bleed *positioned* <video> paints AFTER the in-flow button
          // bar and still hit-tests at opacity:0; on some OEM WebViews it is
          // promoted to a hardware surface above the DOM entirely. Without
          // this, the 返回 / 取消 button looks tappable but never receives
          // the tap and the user is stranded on the error screen.
          pointerEvents: 'none',
        }}
      />
      <canvas ref={canvasRef} style={{ display: 'none' }} />
      <div
        style={{
          // Lift the bar above the positioned <video> in the paint order.
          // `pointerEvents:'none'` on the video is the primary fix; this is
          // the belt-and-braces for WebViews that composite video separately.
          position: 'relative',
          marginTop: 'auto',
          // `--safe-bottom` is the resolved clearance (lib/safeArea.ts):
          // raw `env()` is 0 on notch-less Android, which buries the cancel
          // button under the navigation bar.
          padding: '24px 20px calc(28px + var(--safe-bottom))',
          background: 'linear-gradient(transparent, rgba(0,0,0,0.75))',
        }}
      >
        <div style={{ color: '#fff', fontSize: 14, textAlign: 'center', marginBottom: 16, lineHeight: 1.5 }}>
          {t(msgKey)}
        </div>
        <button
          type="button"
          onClick={() => onCloseRef.current()}
          style={{
            display: 'block',
            width: '100%',
            padding: '12px',
            borderRadius: 8,
            border: 'none',
            background: failed ? '#333' : 'rgba(255,255,255,0.18)',
            color: '#fff',
            fontSize: 15,
          }}
        >
          {failed ? t('scan.back') : t('scan.cancel')}
        </button>
      </div>
    </div>
  )
}
