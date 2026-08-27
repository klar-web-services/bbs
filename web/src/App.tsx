import { Fragment, FormEvent, useEffect, useMemo, useState } from 'react'
import HighlightedLine from './HighlightedLine'
import type { Bootstrap, Catalog, SearchEvent, SearchResponse, SearchResult } from './types'

const SPINNER = ['\u280b', '\u2819', '\u2839', '\u2838', '\u283c', '\u2834', '\u2826', '\u2827', '\u2807', '\u280f']
const SKELETON = [[46, 18, 92], [120, 40], [30, 74, 26, 58], [88, 34], [52, 110, 22], [64, 28, 46], [100, 36, 18]]

const APPLE = typeof navigator !== 'undefined' && /Mac|iPhone|iPod|iPad/i.test(navigator.userAgent)
const SHORTCUT = APPLE ? '\u2318 K' : 'Ctrl K'

const emptyResponse: SearchResponse = { query: [], results: [], repositories_searched: 0, files_searched: 0, elapsed_ms: 0, cached: false, truncated: false }

export default function App() {
  const [bootstrap, setBootstrap] = useState<Bootstrap | null>(null)
  const [catalog, setCatalog] = useState<Catalog | null>(null)
  const [query, setQuery] = useState('')
  const [selectedRepos, setSelectedRepos] = useState<string[]>([])
  const [path, setPath] = useState('')
  const [branch, setBranch] = useState('')
  const [regex, setRegex] = useState(false)
  const [multiline, setMultiline] = useState(false)
  const [offline, setOffline] = useState(false)
  const [caseMode, setCaseMode] = useState<'Smart' | 'Ignore' | 'Sensitive'>('Smart')
  const [sort, setSort] = useState<'relevance' | 'repo' | 'path'>('relevance')
  const [status, setStatus] = useState('Ready')
  const [running, setRunning] = useState(false)
  const [error, setError] = useState('')
  const [response, setResponse] = useState<SearchResponse>(emptyResponse)
  const [repoFilter, setRepoFilter] = useState('')
  const [activeJob, setActiveJob] = useState<string | null>(null)

  useEffect(() => {
    fetch('/api/v1/bootstrap').then(r => r.json()).then(async data => {
      setBootstrap(data)
      const response = await fetch(`/api/v1/repositories${data.authenticated ? '' : '?offline=true'}`)
      if (!response.ok) throw new Error((await response.json()).error)
      setCatalog(await response.json())
    }).catch(err => setError(String(err)))
  }, [])

  useEffect(() => {
    const shortcut = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && event.key === 'k') { event.preventDefault(); document.querySelector<HTMLInputElement>('#query')?.focus() }
    }
    addEventListener('keydown', shortcut); return () => removeEventListener('keydown', shortcut)
  }, [])

  const visibleRepos = useMemo(() => catalog?.repositories.filter(repo => repo.full_name.toLowerCase().includes(repoFilter.toLowerCase())) || [], [catalog, repoFilter])

  async function submit(event: FormEvent) {
    event.preventDefault(); if (!bootstrap || !query.trim()) return
    setRunning(true); setError(''); setResponse(emptyResponse); setStatus('Starting search…')
    const request = {
      queries: [query], repositories: selectedRepos, paths: path ? [path] : [], branch: branch || null,
      regex, multiline, case_mode: caseMode, offline, context: 2, max_results: 500, sort, no_cache: false,
    }
    try {
      const created = await fetch('/api/v1/search', { method: 'POST', headers: { 'content-type': 'application/json', 'x-bbs-csrf': bootstrap.csrf_token }, body: JSON.stringify(request) })
      if (!created.ok) throw new Error((await created.json()).error)
      const { id } = await created.json()
      setActiveJob(id)
      const events = new EventSource(`/api/v1/search/${id}/events`)
      let finished = false
      events.onmessage = message => {
        const event: SearchEvent = JSON.parse(message.data)
        if (event.type === 'progress') setStatus(event.message)
        else if (event.type === 'result') setResponse(current => ({ ...current, results: mergeResult(current.results, event.result) }))
        else if (event.type === 'warning') setStatus(event.message)
        else if (event.type === 'error') { finished = true; setError(event.message); setRunning(false); setActiveJob(null); events.close() }
        else if (event.type === 'done') { finished = true; setResponse(event.response); setStatus(`${event.response.results.length} results`); setRunning(false); setActiveJob(null); events.close() }
      }
      events.onerror = () => { if (!finished) setError('The local result stream disconnected.'); setRunning(false); setActiveJob(null); events.close() }
    } catch (err) { setError(err instanceof Error ? err.message : String(err)); setRunning(false) }
  }

  async function cancel() {
    if (!activeJob || !bootstrap) return
    setStatus('Cancelling search…')
    await fetch(`/api/v1/search/${activeJob}/cancel`, { method: 'POST', headers: { 'x-bbs-csrf': bootstrap.csrf_token } }).catch(() => undefined)
  }

  return <div className="shell">
    <header className="app-header"><div className="brand"><span className="brand-mark" aria-hidden="true"><SearchMark /></span><h1>Better Bitbucket Search</h1></div><div className="version">local · v{bootstrap?.version || '…'}</div></header>
    <main>
      <form onSubmit={submit} className="search-panel">
        <div className="query-row"><span className="prompt">›</span><input id="query" autoFocus value={query} onChange={e => setQuery(e.target.value)} placeholder={'foo AND (bar OR /baz\\d+/)'} aria-label="Search query"/><kbd>{SHORTCUT}</kbd><button type={running ? 'button' : 'submit'} onClick={running ? cancel : undefined} disabled={!running && !query.trim()}>{running ? 'Cancel' : 'Search'}</button></div>
        <div className="filters">
          <label className="filter-field filter-path">Path<input type="text" value={path} onChange={e => setPath(e.target.value)} placeholder="src/**/*.ts" /></label>
          <label className="filter-field filter-branch">Branch<input type="text" value={branch} onChange={e => setBranch(e.target.value)} placeholder="default" /></label>
          <label className="filter-field filter-case">Case<select value={caseMode} onChange={e => setCaseMode(e.target.value as typeof caseMode)}><option value="Smart">smart</option><option value="Ignore">ignore</option><option value="Sensitive">sensitive</option></select></label>
          <label className="filter-field filter-sort">Sort<select value={sort} onChange={e => setSort(e.target.value as typeof sort)}><option value="relevance">relevance</option><option value="repo">repository</option><option value="path">path</option></select></label>
          <label className="check"><input type="checkbox" checked={regex} onChange={e => setRegex(e.target.checked)} /> Raw regex</label><label className="check" title="Let wildcards and . span line breaks"><input type="checkbox" checked={multiline} onChange={e => setMultiline(e.target.checked)} /> Multi-line</label>
          <label className="check"><input type="checkbox" checked={offline} onChange={e => setOffline(e.target.checked)} /> Offline</label>
        </div>
        <details className="repo-picker"><summary>{selectedRepos.length ? `${selectedRepos.length} repositories selected` : 'All accessible repositories'}</summary><div className="repo-menu"><input value={repoFilter} onChange={e => setRepoFilter(e.target.value)} placeholder="Filter repositories…"/><button type="button" onClick={() => setSelectedRepos([])}>All repositories</button>{visibleRepos.map(repo => <label key={repo.uuid}><input type="checkbox" checked={selectedRepos.includes(repo.full_name)} onChange={e => setSelectedRepos(current => e.target.checked ? [...current, repo.full_name] : current.filter(item => item !== repo.full_name))}/><span>{repo.full_name}</span><small>{repo.default_branch || 'empty'}</small></label>)}</div></details>
        <div className="grammar"><span>Boolean:</span> AND · OR · NOT · parentheses <i/> <span>Patterns:</span> wild*card · "exact phrase" · /regex/</div>
      </form>
      <section className="result-meta"><div>{running && <Spinner/>}<strong>{status}</strong>{running && <span className="caret" aria-hidden="true"/>}{response.cached && <span className="badge">cache hit</span>}{offline && <span className="badge warning">offline</span>}</div>{response.files_searched > 0 && <span>{response.repositories_searched} repos · {response.files_searched.toLocaleString()} files · {response.elapsed_ms} ms</span>}</section>
      {error && <div className="error"><strong>Search stopped</strong><span>{error}</span></div>}
      {running && response.results.length === 0 && <Scanning label={status}/>}
      {!running && !error && response.results.length === 0 && <div className="empty"><div className="empty-icon">⌕</div><h2>Find the line that matters</h2><p>Use Boolean expressions, wildcards, PCRE2 regexes, repository scopes, and path globs.</p></div>}
      <section className="results">{response.results.map(result => <ResultCard key={`${result.repository}:${result.path}`} result={result}/>)}</section>
    </main>
  </div>
}

function Spinner() {
  const [frame, setFrame] = useState(0)
  useEffect(() => {
    const timer = setInterval(() => setFrame(current => (current + 1) % SPINNER.length), 80)
    return () => clearInterval(timer)
  }, [])
  return <span className="spin" aria-hidden="true">{SPINNER[frame]}</span>
}

function Scanning({ label }: { label: string }) {
  return <section className="scanning" role="status" aria-busy="true" aria-label={label}>
    {SKELETON.map((widths, row) => <div className="scan-line" key={row} style={{ ['--delay' as string]: `${row * 90}ms` }}>
      <span className="scan-number">{row + 12}</span>
      <span className="scan-code">{widths.map((width, block) => <i key={block} style={{ width: `${width}px` }}/>)}</span>
    </div>)}
  </section>
}

function SearchMark() {
  return <svg className="brand-mark-icon" viewBox="0 0 32 32" fill="none" focusable="false">
    <circle className="brand-mark-lens" cx="13.5" cy="13.5" r="8.25" />
    <path className="brand-mark-handle" d="m19.4 19.4 6.1 6.1" />
    <path className="brand-mark-code" d="m12 10-3 3.5 3 3.5m3-7 3 3.5-3 3.5" />
  </svg>
}

function mergeResult(results: SearchResult[], result: SearchResult) { return [...results.filter(item => !(item.repository === result.repository && item.path === result.path)), result] }

function ResultCard({ result }: { result: SearchResult }) {
  const [collapsed, setCollapsed] = useState(false)
  return <article className={`result-card ${collapsed ? 'collapsed' : ''}`}>
    <div className="result-head">
      <button type="button" className="collapse" onClick={() => setCollapsed(value => !value)} aria-expanded={!collapsed} title={collapsed ? 'Expand result' : 'Collapse result'} aria-label={collapsed ? 'Expand result' : 'Collapse result'}>{collapsed ? '▶' : '▼'}</button>
      <div><span className="repo">{result.repository}</span><span className="slash">/</span><a href={result.web_url} target="_blank" rel="noreferrer">{result.path}</a></div>
      <div><span>{result.match_count} {result.match_count === 1 ? 'match' : 'matches'}</span><code>{result.branch}@{result.commit.slice(0, 9)}</code>{result.stale && <span className="badge warning">stale</span>}</div>
    </div>
    {!collapsed && <div className="code-block">{result.lines.map((line, index) => {
      const gap = index > 0 && line.number > result.lines[index - 1].number + 1
      return <Fragment key={line.number}>
        {gap && <div className="code-gap" aria-hidden="true"><span className="code-gap-mark">···</span></div>}
        <div className={`code-line ${line.ranges.length ? 'matched' : ''}`}><span className="line-number">{line.number}</span><HighlightedLine line={line} path={result.path}/></div>
      </Fragment>
    })}</div>}
  </article>
}
