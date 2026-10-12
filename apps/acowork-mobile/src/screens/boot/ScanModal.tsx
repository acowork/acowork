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
  const videoRef = useRef<HTMLVideoElement | null>(null)
  const canvasRef = useRef<HTMLCanvasElement | null>(null)
  const [msg, setMsg] = useState('正在启动相机…')
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
        setMsg('无法访问相机：请在系统设置中允许 ACowork 使用相机后重试，或改用手动输入网关地址。')
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
      setMsg('将桌面端的配对二维码对准取景框')

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
              setMsg('配对中…')
              void useAuthStore
                .getState()
                .pairLogin(parsed.gw, parsed.pt)
                .then((ok) => {
                  if (ok) onCloseRef.current()
                  else {
                    setMsg(useAuthStore.getState().loginError || '配对失败，请在桌面端重新生成二维码')
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
        }}
      />
      <canvas ref={canvasRef} style={{ display: 'none' }} />
      <div
        style={{
          marginTop: 'auto',
          padding: '24px 20px calc(28px + env(safe-area-inset-bottom))',
          background: 'linear-gradient(transparent, rgba(0,0,0,0.75))',
        }}
      >
        <div style={{ color: '#fff', fontSize: 14, textAlign: 'center', marginBottom: 16, lineHeight: 1.5 }}>
          {msg}
        </div>
        <button
          type="button"
          onClick={onClose}
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
          {failed ? '返回' : '取消'}
        </button>
      </div>
    </div>
  )
}
