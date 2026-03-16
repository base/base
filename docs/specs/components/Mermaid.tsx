import { useEffect, useId, useRef, useState } from 'react'

type MermaidProps = {
  chart: string
}

export function Mermaid({ chart }: MermaidProps) {
  const containerRef = useRef<HTMLDivElement | null>(null)
  const chartId = `mermaid-${useId().replace(/[^a-zA-Z0-9_-]/g, '')}`
  const [isDark, setIsDark] = useState(false)
  const [status, setStatus] = useState<'loading' | 'ready' | 'error'>('loading')

  useEffect(() => {
    if (typeof document === 'undefined') return

    const root = document.documentElement
    const updateTheme = () => {
      setIsDark(root.classList.contains('dark'))
    }

    updateTheme()

    const observer = new MutationObserver(updateTheme)
    observer.observe(root, {
      attributeFilter: ['class'],
      attributes: true,
    })

    return () => observer.disconnect()
  }, [])

  useEffect(() => {
    let cancelled = false

    const renderDiagram = async () => {
      const container = containerRef.current
      if (!container) return

      setStatus('loading')
      container.innerHTML = ''

      try {
        const mermaid = (await import('mermaid')).default

        mermaid.initialize({
          securityLevel: 'strict',
          startOnLoad: false,
          theme: isDark ? 'dark' : 'default',
        })

        const { bindFunctions, svg } = await mermaid.render(chartId, chart)
        if (cancelled) return

        container.innerHTML = svg
        bindFunctions?.(container)
        setStatus('ready')
      } catch (error) {
        if (cancelled) return

        console.error('Failed to render Mermaid diagram.', error)
        container.innerHTML = ''
        setStatus('error')
      }
    }

    void renderDiagram()

    return () => {
      cancelled = true
    }
  }, [chart, chartId, isDark])

  return (
    <figure className="mermaid-diagram">
      {status === 'loading' ? (
        <div className="mermaid-diagram__loading">Rendering diagram...</div>
      ) : null}
      {status === 'error' ? (
        <pre className="mermaid-diagram__fallback">
          <code>{chart}</code>
        </pre>
      ) : null}
      <div
        ref={containerRef}
        className="mermaid-diagram__content"
        hidden={status !== 'ready'}
      />
    </figure>
  )
}
