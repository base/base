import { readdirSync, readFileSync, statSync } from 'node:fs'
import { join, relative, sep } from 'node:path'
import { fileURLToPath } from 'node:url'

import rehypeKatex from 'rehype-katex'
import remarkMath from 'remark-math'
import { defineConfig, type SidebarItem } from 'vocs'

const pagesDir = fileURLToPath(new URL('./pages', import.meta.url))

type NodeInfo = {
  hasIndex: boolean
  indexTitle?: string
  items: SidebarItem[]
}

type BuildTreeOptions = {
  excludeDirs?: Set<string>
  excludeFiles?: Set<string>
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

function buildTree(dirPath: string, options: BuildTreeOptions = {}): NodeInfo {
  const { excludeDirs, excludeFiles } = options
  const entries = readdirSync(dirPath).sort(sortName)
  const files = entries.filter((entry) => /\.(md|mdx)$/i.test(entry) && !excludeFiles?.has(entry))
  const dirs = entries.filter(
    (entry) => statSync(join(dirPath, entry)).isDirectory() && !excludeDirs?.has(entry),
  )

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

function sectionItemWithoutDirs(
  section: string,
  text: string,
  excludedDirs: string[],
  excludedFiles: string[] = [],
): SidebarItem {
  const sectionPath = join(pagesDir, section)
  const tree = buildTree(sectionPath, {
    excludeDirs: new Set(excludedDirs),
    excludeFiles: new Set(excludedFiles),
  })
  return {
    text,
    ...(tree.hasIndex ? { link: `/${section}` } : {}),
    ...(tree.items.length ? { items: tree.items } : {}),
  }
}

const evmLinks = new Set(['/protocol/precompiles', '/protocol/predeploys', '/protocol/preinstalls'])

function withEvmSection(items: SidebarItem[]): SidebarItem[] {
  const remainingItems: SidebarItem[] = []
  const groupedItems: SidebarItem[] = []
  let insertIndex = -1

  for (const item of items) {
    if ('link' in item && item.link && evmLinks.has(item.link)) {
      if (insertIndex === -1) insertIndex = remainingItems.length
      groupedItems.push(item)
      continue
    }
    remainingItems.push(item)
  }

  if (groupedItems.length === 0) return items
  if (insertIndex === -1) insertIndex = remainingItems.length

  const evmSection: SidebarItem = {
    text: 'EVM',
    items: groupedItems,
    collapsed: true,
  }

  return [
    ...remainingItems.slice(0, insertIndex),
    evmSection,
    ...remainingItems.slice(insertIndex),
  ]
}

const sidebar: SidebarItem[] = [
  { text: 'Home', link: '/' },
  {
    text: 'Upgrades',
    items: [
      { text: 'Jovian', link: '/protocol/jovian/overview' },
      { text: 'Isthmus', link: '/protocol/isthmus/overview' },
      { text: 'Pectra Blob Schedule (Sepolia)', link: '/protocol/pectra-blob-schedule/overview' },
      { text: 'Holocene', link: '/protocol/holocene/overview' },
      { text: 'Granite', link: '/protocol/granite/overview' },
      { text: 'Fjord', link: '/protocol/fjord/overview' },
      { text: 'Ecotone', link: '/protocol/ecotone/overview' },
      { text: 'Delta', link: '/protocol/delta/overview' },
      { text: 'Canyon', link: '/protocol/canyon/overview' },
    ],
  },
  (() => {
    const protocol = sectionItemWithoutDirs(
      'protocol',
      'Protocol',
      ['jovian', 'isthmus', 'holocene', 'granite', 'fjord', 'ecotone', 'delta', 'canyon', 'pectra-blob-schedule'],
      ['access-lists.md'],
    )
    const protocolItems = withEvmSection(protocol.items ?? [])
    return {
      ...protocol,
      items: [
        ...protocolItems,
        { ...sectionItem('fault-proof', 'Fault Proof'), collapsed: true },
      ],
    }
  })(),
  {
    text: 'Reference',
    items: [{ text: 'Glossary', link: '/glossary' }],
  },
]

export default defineConfig({
  title: 'Base Specification',
  description: 'Base Chain specs inspired by Ethereum and the OP Stack, with independent evolution after Jovian.',
  logoUrl: '/assets/base/logo.svg',
  iconUrl: '/assets/base/favicon.png',
  topNav: [
    { text: 'Docs', link: 'https://docs.base.org/base-chain/' },
    { text: 'Blog', link: 'https://blog.base.dev/' },
  ],
  markdown: {
    remarkPlugins: [remarkMath],
    rehypePlugins: [rehypeKatex],
  },
  rootDir: '.',
  sidebar,
})
