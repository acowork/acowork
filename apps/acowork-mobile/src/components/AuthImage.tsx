/**
 * AuthImage — fetches an image URL with the gateway bearer token and renders
 * the result as a blob: URL. Falls back to `fallback` on failure or while loading.
 *
 * The avatar-file endpoints require auth; a bare <img src> cannot send the
 * Authorization header, so we fetch explicitly and produce a blob.
 */
import { useEffect, useState, type CSSProperties, type ReactNode } from 'react'
import { useAuthStore } from '../stores/authStore'

export function AuthImage({
  src,
  alt,
  className,
  style,
  fallback,
}: {
  src: string | null
  alt?: string
  className?: string
  style?: CSSProperties
  fallback: ReactNode
}) {
  const [blobUrl, setBlobUrl] = useState<string | null>(null)

  useEffect(() => {
    setBlobUrl(null)
    if (!src) return

    const token = useAuthStore.getState().accessToken
    let cancelled = false
    let objectUrl: string | null = null

    fetch(src, {
      headers: token ? { Authorization: `Bearer ${token}` } : {},
    })
      .then((resp) => {
        if (!resp.ok) return null
        return resp.blob()
      })
      .then((blob) => {
        if (!blob || cancelled) return
        objectUrl = URL.createObjectURL(blob)
        setBlobUrl(objectUrl)
      })
      .catch(() => {
        /* network error — show fallback */
      })

    return () => {
      cancelled = true
      if (objectUrl) URL.revokeObjectURL(objectUrl)
    }
  }, [src])

  if (!blobUrl) return <>{fallback}</>
  return <img src={blobUrl} alt={alt ?? ''} className={className} style={style} />
}
