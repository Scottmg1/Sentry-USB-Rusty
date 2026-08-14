import { Fragment, type ReactNode } from "react"

// The assistant writes light Markdown: **bold** labels, `code` spans, and
// short numbered or bulleted lists. Rendering the raw string leaves the
// asterisks on screen, so this turns the small subset it actually uses into
// elements.
//
// Everything here builds React nodes from parsed text. No HTML string is ever
// constructed and dangerouslySetInnerHTML is never used, so assistant output
// cannot inject markup no matter what it contains.

const INLINE_PATTERN = /(\*\*[^*\n]+\*\*|__[^_\n]+__|`[^`\n]+`|\*[^*\n]+\*)/g

function renderInline(text: string, keyPrefix: string): ReactNode[] {
  const nodes: ReactNode[] = []
  let index = 0
  for (const piece of text.split(INLINE_PATTERN)) {
    if (!piece) continue
    const key = `${keyPrefix}-${index}`
    index += 1
    if ((piece.startsWith("**") && piece.endsWith("**") && piece.length > 4)
      || (piece.startsWith("__") && piece.endsWith("__") && piece.length > 4)) {
      nodes.push(<strong key={key} className="font-semibold text-slate-100">{piece.slice(2, -2)}</strong>)
    } else if (piece.startsWith("`") && piece.endsWith("`") && piece.length > 2) {
      nodes.push(
        <code key={key} className="rounded bg-white/[0.06] px-1 py-0.5 font-mono text-[0.85em] text-slate-200">
          {piece.slice(1, -1)}
        </code>,
      )
    } else if (piece.startsWith("*") && piece.endsWith("*") && piece.length > 2) {
      nodes.push(<em key={key}>{piece.slice(1, -1)}</em>)
    } else {
      nodes.push(<Fragment key={key}>{piece}</Fragment>)
    }
  }
  return nodes
}

type Block =
  | { kind: "paragraph"; lines: string[] }
  | { kind: "list"; ordered: boolean; items: string[] }

const ORDERED_ITEM = /^\s*\d+[.)]\s+(.*)$/
const BULLET_ITEM = /^\s*[-*•]\s+(.*)$/

function toBlocks(content: string): Block[] {
  const blocks: Block[] = []
  let paragraph: string[] = []
  let list: { ordered: boolean; items: string[] } | null = null

  const flushParagraph = () => {
    if (paragraph.length) blocks.push({ kind: "paragraph", lines: paragraph })
    paragraph = []
  }
  const flushList = () => {
    if (list) blocks.push({ kind: "list", ordered: list.ordered, items: list.items })
    list = null
  }

  for (const rawLine of content.replace(/\r\n?/g, "\n").split("\n")) {
    const line = rawLine.trimEnd()
    const ordered = ORDERED_ITEM.exec(line)
    const bullet = BULLET_ITEM.exec(line)

    if (ordered || bullet) {
      flushParagraph()
      const isOrdered = Boolean(ordered)
      const item = (ordered ? ordered[1] : bullet![1]).trim()
      if (!list || list.ordered !== isOrdered) {
        flushList()
        list = { ordered: isOrdered, items: [] }
      }
      list.items.push(item)
      continue
    }

    if (!line.trim()) {
      flushList()
      flushParagraph()
      continue
    }

    // A plain line directly under a list item is that item's continuation.
    if (list && /^\s{2,}/.test(rawLine) && list.items.length) {
      list.items[list.items.length - 1] += ` ${line.trim()}`
      continue
    }

    flushList()
    paragraph.push(line.trim())
  }
  flushList()
  flushParagraph()
  return blocks
}

export function AssistantMarkdown({ content }: { content: string }) {
  const blocks = toBlocks(content)
  if (!blocks.length) return null

  return (
    <div className="space-y-2 text-sm leading-relaxed">
      {blocks.map((block, blockIndex) => {
        if (block.kind === "list") {
          const ListTag = block.ordered ? "ol" : "ul"
          return (
            <ListTag
              key={`block-${blockIndex}`}
              className={cnList(block.ordered)}
            >
              {block.items.map((item, itemIndex) => (
                <li key={`block-${blockIndex}-item-${itemIndex}`} className="pl-1">
                  {renderInline(item, `block-${blockIndex}-item-${itemIndex}`)}
                </li>
              ))}
            </ListTag>
          )
        }
        return (
          <p key={`block-${blockIndex}`} className="break-words">
            {block.lines.map((line, lineIndex) => (
              <Fragment key={`block-${blockIndex}-line-${lineIndex}`}>
                {lineIndex > 0 && <br />}
                {renderInline(line, `block-${blockIndex}-line-${lineIndex}`)}
              </Fragment>
            ))}
          </p>
        )
      })}
    </div>
  )
}

function cnList(ordered: boolean) {
  return ordered
    ? "list-decimal space-y-1 pl-5 marker:text-slate-500"
    : "list-disc space-y-1 pl-5 marker:text-slate-500"
}
