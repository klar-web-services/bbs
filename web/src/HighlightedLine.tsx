import { useEffect, useMemo, useState } from 'react'
import { createHighlighterCore, type ThemedToken } from 'shiki/core'
import { createJavaScriptRegexEngine } from 'shiki/engine/javascript'
import githubDark from '@shikijs/themes/github-dark'
import typescript from '@shikijs/langs/typescript'
import tsx from '@shikijs/langs/tsx'
import javascript from '@shikijs/langs/javascript'
import jsx from '@shikijs/langs/jsx'
import rust from '@shikijs/langs/rust'
import python from '@shikijs/langs/python'
import go from '@shikijs/langs/go'
import java from '@shikijs/langs/java'
import kotlin from '@shikijs/langs/kotlin'
import ruby from '@shikijs/langs/ruby'
import php from '@shikijs/langs/php'
import csharp from '@shikijs/langs/csharp'
import cpp from '@shikijs/langs/cpp'
import c from '@shikijs/langs/c'
import css from '@shikijs/langs/css'
import scss from '@shikijs/langs/scss'
import html from '@shikijs/langs/html'
import json from '@shikijs/langs/json'
import yaml from '@shikijs/langs/yaml'
import bash from '@shikijs/langs/bash'
import sql from '@shikijs/langs/sql'
import markdown from '@shikijs/langs/markdown'
import type { ResultLine } from './types'

type Language = 'typescript' | 'tsx' | 'javascript' | 'jsx' | 'rust' | 'python' | 'go' | 'java' | 'kotlin' | 'ruby' | 'php' | 'csharp' | 'cpp' | 'c' | 'css' | 'scss' | 'html' | 'json' | 'yaml' | 'bash' | 'sql' | 'markdown' | 'text'

const highlighter = createHighlighterCore({
  themes: [githubDark],
  langs: [typescript, tsx, javascript, jsx, rust, python, go, java, kotlin, ruby, php, csharp, cpp, c, css, scss, html, json, yaml, bash, sql, markdown],
  engine: createJavaScriptRegexEngine(),
})

const languageFor = (path: string): Language => {
  const ext = path.split('.').pop()?.toLowerCase()
  return (({ ts: 'typescript', tsx: 'tsx', js: 'javascript', jsx: 'jsx', rs: 'rust', py: 'python', go: 'go', java: 'java', kt: 'kotlin', rb: 'ruby', php: 'php', cs: 'csharp', cpp: 'cpp', c: 'c', h: 'c', css: 'css', scss: 'scss', html: 'html', json: 'json', yaml: 'yaml', yml: 'yaml', sh: 'bash', sql: 'sql', md: 'markdown' } as Record<string, Language>)[ext || ''] || 'text')
}

function byteToCodeUnit(text: string, byteOffset: number) {
  let bytes = 0
  let units = 0
  for (const char of text) {
    const size = new TextEncoder().encode(char).length
    if (bytes + size > byteOffset) break
    bytes += size
    units += char.length
  }
  return units
}

export default function HighlightedLine({ line, path }: { line: ResultLine; path: string }) {
  const [tokens, setTokens] = useState<ThemedToken[]>([{ content: line.text, offset: 0 }])
  useEffect(() => {
    let active = true
    highlighter.then(instance => instance.codeToTokens(line.text || ' ', { lang: languageFor(path), theme: 'github-dark' }))
      .then(result => { if (active) setTokens(result.tokens[0] || []) })
      .catch(() => { if (active) setTokens([{ content: line.text, offset: 0 }]) })
    return () => { active = false }
  }, [line.text, path])
  // Different atoms routinely match overlapping spans of the same line, and a
  // single match can straddle several syntax tokens. Coalesce first so one
  // visually contiguous hit is one run, then mark only the run's outer edges.
  const ranges = useMemo(() => {
    const converted = line.ranges
      .map(range => ({ start: byteToCodeUnit(line.text, range.start), end: byteToCodeUnit(line.text, range.end) }))
      .filter(range => range.end > range.start)
      .sort((a, b) => a.start - b.start || a.end - b.end)
    const merged: { start: number; end: number }[] = []
    for (const range of converted) {
      const last = merged[merged.length - 1]
      if (last && range.start <= last.end) last.end = Math.max(last.end, range.end)
      else merged.push({ ...range })
    }
    return merged
  }, [line])

  return <code>{tokens.flatMap((token, tokenIndex) => {
    const tokenStart = token.offset
    const tokenEnd = tokenStart + token.content.length
    const points = [...new Set([tokenStart, tokenEnd, ...ranges.flatMap(r => [Math.max(tokenStart, r.start), Math.min(tokenEnd, r.end)])])].filter(p => p >= tokenStart && p <= tokenEnd).sort((a, b) => a - b)
    return points.slice(0, -1).map((start, index) => {
      const end = points[index + 1]
      const run = ranges.find(range => range.start < end && range.end > start)
      const content = line.text.slice(start, end)
      const style = { color: token.color }
      if (!run) return <span key={`${tokenIndex}-${index}`} style={style}>{content}</span>
      const edges = `${run.start === start ? ' is-start' : ''}${run.end === end ? ' is-end' : ''}`
      return <mark key={`${tokenIndex}-${index}`} className={`hit${edges}`} style={style}>{content}</mark>
    })
  })}</code>
}
