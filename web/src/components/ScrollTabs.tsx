import { useLayoutEffect, useRef, useState } from 'react'
import { Button } from './ui'

const FADE = 24
function revealButton(element: HTMLDivElement, button: HTMLButtonElement) {
  const bounds = element.getBoundingClientRect()
  const item = button.getBoundingClientRect()
  const left = item.left - bounds.left - FADE
  const right = item.right - bounds.right + FADE
  if (left < 0) element.scrollLeft += left
  else if (right > 0) element.scrollLeft += right
}

/** Native buttons keep keyboard focus; only this strip scrolls when a section
 * is selected elsewhere. Edge fades disappear when there is nothing to reveal. */
export default function ScrollTabs<T extends string>({
  label,
  items,
  value,
  onChange,
}: {
  label: string
  items: readonly { value: T; label: string }[]
  value: T
  onChange: (value: T) => void
}) {
  const strip = useRef<HTMLDivElement>(null)
  const [edges, setEdges] = useState({ left: false, right: false })

  useLayoutEffect(() => {
    const element = strip.current
    if (!element) return
    const measure = () => {
      const left = element.scrollLeft > 1
      const right =
        element.scrollWidth - element.clientWidth - element.scrollLeft > 1
      setEdges((current) =>
        current.left === left && current.right === right
          ? current
          : { left, right }
      )
    }
    const revealSelected = () => {
      const selected = element.querySelector<HTMLButtonElement>(
        '[aria-pressed="true"]'
      )
      // Keep the selected button and its focus ring outside either fade.
      if (selected) revealButton(element, selected)
      measure()
    }
    revealSelected()
    const observer = new ResizeObserver(revealSelected)
    observer.observe(element)
    element.addEventListener('scroll', measure, { passive: true })
    return () => {
      observer.disconnect()
      element.removeEventListener('scroll', measure)
    }
  }, [value])

  const mask = `linear-gradient(to right, transparent, #000 ${edges.left ? FADE : 0}px, #000 calc(100% - ${edges.right ? FADE : 0}px), transparent)`
  return (
    <div
      ref={strip}
      role="group"
      aria-label={label}
      className="scroll-tabs -mx-1 flex min-w-0 gap-2 overflow-x-auto overscroll-x-contain p-1"
      style={{ maskImage: mask, WebkitMaskImage: mask }}
    >
      {items.map((item) => (
        <Button
          key={item.value}
          className="shrink-0 whitespace-nowrap"
          variant={value === item.value ? 'primary' : 'ghost'}
          aria-pressed={value === item.value}
          onFocus={(event) => {
            const element = strip.current
            // Pointer selection reveals after click, so a partially visible
            // button cannot move away from the pointer before mouseup.
            if (element && event.currentTarget.matches(':focus-visible'))
              revealButton(element, event.currentTarget)
          }}
          onClick={() => onChange(item.value)}
        >
          {item.label}
        </Button>
      ))}
    </div>
  )
}
