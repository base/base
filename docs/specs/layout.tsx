import type { ReactNode } from 'react'
import { MDXProvider } from 'vocs/mdx-react'

import { Mermaid } from './components/Mermaid'

const components = {
  Mermaid,
}

export default function Layout({ children }: { children: ReactNode }) {
  return <MDXProvider components={components}>{children}</MDXProvider>
}
