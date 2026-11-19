import { createElement as h } from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, it } from 'vitest'
import RemoveBotCard from './components/RemoveBotCard'
import RenameBot from './components/RenameBot'
import { ProfileContext, type PanelProfile } from './profile'
import type { Bot } from './types'

const bot = { name: 'bot-a', displayName: 'Bot A' } as Bot

function render(profile: PanelProfile, node: ReturnType<typeof h>): string {
  return renderToStaticMarkup(h(ProfileContext.Provider, { value: profile }, node))
}

const rename = h(RenameBot, { bot, onRenamed: () => {} })
const remove = h(RemoveBotCard, { funding: null, hasContainer: true, busy: false, onRemove: () => {} })

describe('controls a hosted panel refuses', () => {
  it('renders neither rename nor Remove on hosted', () => {
    expect(render('hosted', rename)).toBe('')
    expect(render('hosted', remove)).toBe('')
  })

  it('still renders both on the full panel', () => {
    expect(render('full', rename)).not.toBe('')
    expect(render('full', remove)).toContain('Remove this bot')
  })
})
