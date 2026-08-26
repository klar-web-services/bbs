export interface Bootstrap { csrf_token: string; version: string; authenticated: boolean }
export interface Repository { uuid: string; workspace: string; slug: string; name: string; full_name: string; default_branch?: string; web_url: string }
export interface Catalog { discovered_at: string; repositories: Repository[] }
export interface MatchRange { start: number; end: number; atom: number }
export interface ResultLine { number: number; text: string; ranges: MatchRange[]; is_context: boolean }
export interface SearchResult { repository: string; repository_name: string; path: string; branch: string; commit: string; web_url: string; score: number; match_count: number; lines: ResultLine[]; stale: boolean }
export interface SearchResponse { query: string[]; results: SearchResult[]; repositories_searched: number; files_searched: number; elapsed_ms: number; cached: boolean; truncated: boolean }
export type SearchEvent =
  | { type: 'progress'; phase: string; message: string; current: number; total: number }
  | { type: 'result'; result: SearchResult }
  | { type: 'warning'; message: string }
  | { type: 'error'; message: string }
  | { type: 'done'; response: SearchResponse }
