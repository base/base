type MarkdownNode = {
  children?: MarkdownNode[]
  lang?: string | null
  type?: string
  value?: string
}

type MermaidNode = MarkdownNode & {
  attributes: Array<{
    name: string
    type: 'mdxJsxAttribute'
    value: string
  }>
  children: []
  name: 'Mermaid'
  type: 'mdxJsxFlowElement'
}

export function remarkMermaid() {
  return (tree: MarkdownNode) => {
    transform(tree)
  }
}

function transform(node: MarkdownNode) {
  if (!Array.isArray(node.children)) return

  for (let index = 0; index < node.children.length; index += 1) {
    const child = node.children[index]
    if (child?.type === 'code' && child.lang === 'mermaid') {
      node.children[index] = {
        type: 'mdxJsxFlowElement',
        name: 'Mermaid',
        attributes: [
          {
            type: 'mdxJsxAttribute',
            name: 'chart',
            value: child.value ?? '',
          },
        ],
        children: [],
      } satisfies MermaidNode
      continue
    }

    transform(child)
  }
}
