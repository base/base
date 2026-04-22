import { readdirSync, readFileSync, statSync } from 'node:fs'
import { join, relative, sep } from 'node:path'
import { fileURLToPath } from 'node:url'

import { remarkMermaid } from './lib/remarkMermaid'
import rehypeKatex from 'rehype-katex'
import remarkMath from 'remark-math'
import { defineConfig, type SidebarItem } from 'vocs'

const docsDir = fileURLToPath(new URL('./', import.meta.url))
const pagesDir = fileURLToPath(new URL('./pages', import.meta.url))

type NodeInfo = {
  hasIndex: boolean
  indexTitle?: string
  items: SidebarItem[]
}

function toPosix(path: string) {
  return path.split(sep).join('/')
}

function getPathLink(filePath: string) {
  const rel = toPosix(relative(pagesDir, filePath)).replace(/\.(md|mdx)$/i, '')
  if (rel === 'index') return '/'
  if (rel.endsWith('/index')) return `/${rel.slice(0, -6)}`
  return `/${rel}`
}

function formatSlug(slug: string) {
  return slug
    .split('-')
    .filter(Boolean)
    .map((part) => {
      const upper = part.toUpperCase()
      if (upper === 'L1' || upper === 'L2' || upper === 'EVM' || upper === 'P2P') return upper
      return part.charAt(0).toUpperCase() + part.slice(1)
    })
    .join(' ')
}

function getTitle(filePath: string, fallback: string) {
  const content = readFileSync(filePath, 'utf8')
  const match = content.match(/^#\s+(.+)$/m)
  return match ? match[1].trim() : fallback
}

function sortName(a: string, b: string) {
  const rank = (name: string) => {
    if (name === 'index') return 0
    if (name === 'overview') return 1
    return 2
  }
  const [an, bn] = [a.replace(/\.(md|mdx)$/i, ''), b.replace(/\.(md|mdx)$/i, '')]
  const ar = rank(an)
  const br = rank(bn)
  if (ar !== br) return ar - br
  return an.localeCompare(bn)
}

function buildTree(dirPath: string): NodeInfo {
  const entries = readdirSync(dirPath).sort(sortName)
  const files = entries.filter((entry) => /\.(md|mdx)$/i.test(entry))
  const dirs = entries.filter((entry) => statSync(join(dirPath, entry)).isDirectory())

  let hasIndex = false
  let indexTitle: string | undefined
  const items: SidebarItem[] = []

  for (const file of files) {
    const filePath = join(dirPath, file)
    const basename = file.replace(/\.(md|mdx)$/i, '')
    const title = getTitle(filePath, formatSlug(basename))
    if (basename === 'index') {
      hasIndex = true
      indexTitle = title
      continue
    }
    items.push({ text: title, link: getPathLink(filePath) })
  }

  for (const dir of dirs) {
    const dirPathChild = join(dirPath, dir)
    const child = buildTree(dirPathChild)
    if (!child.hasIndex && child.items.length === 0) continue

    const link = child.hasIndex ? getPathLink(join(dirPathChild, 'index.md')) : undefined
    const text = child.indexTitle ?? formatSlug(dir)

    items.push({
      text,
      ...(link ? { link } : {}),
      ...(child.items.length ? { items: child.items } : {}),
    })
  }

  return { hasIndex, indexTitle, items }
}

function sectionItem(section: string, text: string): SidebarItem {
  const sectionPath = join(pagesDir, section)
  const tree = buildTree(sectionPath)
  return {
    text,
    ...(tree.hasIndex ? { link: `/${section}` } : {}),
    ...(tree.items.length ? { items: tree.items } : {}),
  }
}

const bcpsSection: SidebarItem = {
  text: 'Base Change Proposals',
  items: [
    { text: 'BCP List', link: '/bcps' },
    { text: 'BCP Process', link: '/bcps/bcp-0000' },
  ],
}

const bridgingSection: SidebarItem = {
  text: 'Bridging',
  items: [
    { text: 'Deposits', link: '/protocol/bridging/deposits' },
    { text: 'Withdrawals', link: '/protocol/bridging/withdrawals' },
    { text: 'Standard Bridges', link: '/protocol/bridging/bridges' },
    { text: 'Cross Domain Messengers', link: '/protocol/bridging/messengers' },
  ],
  collapsed: true,
}

const consensusSection: SidebarItem = {
  text: 'Consensus',
  link: '/protocol/consensus',
  items: [
    { text: 'Derivation', link: '/protocol/consensus/derivation' },
    { text: 'P2P', link: '/protocol/consensus/p2p' },
    { text: 'RPC', link: '/protocol/consensus/rpc' },
  ],
  collapsed: true,
}

const executionSection: SidebarItem = {
  text: 'Execution',
  link: '/protocol/execution',
  items: [
    { text: 'Precompiles', link: '/protocol/execution/evm/precompiles' },
    { text: 'Predeploys', link: '/protocol/execution/evm/predeploys' },
    { text: 'Preinstalls', link: '/protocol/execution/evm/preinstalls' },
    { text: 'RPC', link: '/protocol/execution/evm/rpc' },
  ],
  collapsed: true,
}

const sidebar: SidebarItem[] = [
  { text: 'Home', link: '/' },
  {
    text: 'Protocol',
    items: [
      { text: 'Overview', link: '/protocol/overview' },
      consensusSection,
      executionSection,
      bridgingSection,
      { text: 'Batcher', link: '/protocol/batcher' },
      { ...sectionItem('protocol/fault-proof', 'Proofs'), collapsed: true },
    ],
  },
  bcpsSection,
  {
    text: 'Upgrades',
    items: [
      { text: 'Azul', link: '/upgrades/azul/overview' },
      {
        text: 'Optimism',
        collapsed: true,
        items: [
          { text: 'Jovian', link: '/upgrades/jovian/overview' },
          { text: 'Isthmus', link: '/upgrades/isthmus/overview' },
          { text: 'Pectra Blob Schedule (Sepolia)', link: '/upgrades/pectra-blob-schedule/overview' },
          { text: 'Holocene', link: '/upgrades/holocene/overview' },
          { text: 'Granite', link: '/upgrades/granite/overview' },
          { text: 'Fjord', link: '/upgrades/fjord/overview' },
          { text: 'Ecotone', link: '/upgrades/ecotone/overview' },
          { text: 'Delta', link: '/upgrades/delta/overview' },
          { text: 'Canyon', link: '/upgrades/canyon/overview' },
        ],
      },
    ],
  },
  sectionItem('reference', 'Reference'),
]

export default defineConfig({
  banner: '⚠️ This specification is under active development and subject to change.',
  title: 'Base Chain Specification',
  description: 'Base Chain protocol specification, upgrades, and reference documentation.',
  logoUrl: {
    light: '/assets/base/logo.svg',
    dark: '/assets/base/logo-white.svg',
  },
  iconUrl: '/assets/base/favicon.png',
  topNav: [
    { text: 'Docs', link: 'https://docs.base.org/base-chain/' },
    { text: 'Blog', link: 'https://blog.base.dev/' },
  ],
  markdown: {
    remarkPlugins: [remarkMath, remarkMermaid],
    rehypePlugins: [rehypeKatex],
  },
  rootDir: '.',
  vite: {
    server: {
      fs: {
        allow: [docsDir, pagesDir],
      },
    },
  },
  sidebar,
  checkDeadlinks: 'error',
})
