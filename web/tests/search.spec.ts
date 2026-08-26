import { expect, test } from '@playwright/test'

test('runs a search and renders highlighted results', async ({ page }) => {
  await page.route('**/api/v1/bootstrap', route => route.fulfill({ json: { csrf_token: 'test-csrf', version: '0.1.0', authenticated: true } }))
  await page.route('**/api/v1/repositories', route => route.fulfill({ json: {
    discovered_at: '2026-08-26T00:00:00Z', workspaces: [], repositories: [
      { uuid: '{repo}', workspace: 'team', slug: 'api', name: 'API', full_name: 'team/api', default_branch: 'main', web_url: 'https://bitbucket.org/team/api' },
    ],
  }}))
  await page.route('**/api/v1/search', async route => {
    expect((await route.request().postDataJSON()).queries).toEqual(['wanted_symbol'])
    expect(route.request().headers()['x-bbs-csrf']).toBe('test-csrf')
    await route.fulfill({ json: { id: '11111111-1111-4111-8111-111111111111' } })
  })
  const result = {
    repository: 'team/api', repository_name: 'API', path: 'src/service.rs', branch: 'main', commit: 'abcdef1234567890',
    web_url: 'https://bitbucket.org/team/api/src/abcdef/src/service.rs#lines-8', score: 42, match_count: 1, stale: false,
    lines: [{ number: 8, text: 'fn wanted_symbol() {}', ranges: [{ start: 3, end: 16, atom: 0 }], is_context: false }],
  }
  const response = { query: ['wanted_symbol'], results: [result], repositories_searched: 1, files_searched: 12, elapsed_ms: 8, cached: false, truncated: false }
  await page.route('**/api/v1/search/*/events', route => route.fulfill({
    status: 200,
    contentType: 'text/event-stream',
    body: `data: ${JSON.stringify({ type: 'progress', phase: 'search', message: 'Scanning synchronized snapshots', current: 0, total: 1 })}\n\ndata: ${JSON.stringify({ type: 'done', response })}\n\n`,
  }))

  await page.goto('/')
  await page.getByLabel('Search query').fill('wanted_symbol')
  await page.getByRole('button', { name: 'Search' }).click()
  const resultCard = page.getByRole('article')
  await expect(resultCard.getByText('team/api')).toBeVisible()
  await expect(resultCard.getByText('src/service.rs')).toBeVisible()
  await expect(resultCard.locator('mark')).toContainText('wanted_symbol')
  await expect(page.getByText('1 repos · 12 files · 8 ms')).toBeVisible()
})

test('shows the empty search guidance', async ({ page }) => {
  await page.route('**/api/v1/bootstrap', route => route.fulfill({ json: { csrf_token: 'test', version: '0.1.0', authenticated: false } }))
  await page.route('**/api/v1/repositories?offline=true', route => route.fulfill({ json: { discovered_at: '2026-08-26T00:00:00Z', workspaces: [], repositories: [] } }))
  await page.goto('/')
  await expect(page.getByText('Find the line that matters')).toBeVisible()
})

test('keeps the header and filters inside a narrow viewport', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 })
  await page.route('**/api/v1/bootstrap', route => route.fulfill({ json: { csrf_token: 'test', version: '0.1.0', authenticated: false } }))
  await page.route('**/api/v1/repositories?offline=true', route => route.fulfill({ json: { discovered_at: '2026-08-26T00:00:00Z', workspaces: [], repositories: [] } }))
  await page.goto('/')

  await expect(page.getByText('Search the code you can actually access.')).toHaveCount(0)
  await expect(page.locator('.brand-mark svg')).toBeVisible()

  const overflow = await page.locator('.filters').evaluate((filters) => {
    const viewportWidth = document.documentElement.clientWidth
    return [filters, ...filters.querySelectorAll('label, input, select')]
      .map(element => ({ className: element.className, ...element.getBoundingClientRect().toJSON() }))
      .filter(rect => rect.left < -0.5 || rect.right > viewportWidth + 0.5)
  })
  expect(overflow).toEqual([])
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(390)
})

const catalog = {
  discovered_at: '2026-08-27T00:00:00Z', workspaces: [], repositories: [
    { uuid: '{repo}', workspace: 'team', slug: 'api', name: 'API', full_name: 'team/api', default_branch: 'main', web_url: 'https://bitbucket.org/team/api' },
  ],
}

const gappedResult = {
  repository: 'team/api', repository_name: 'API', path: 'src/service.rs', branch: 'main', commit: '89fb65ab2c1d',
  web_url: 'https://bitbucket.org/team/api/src/89fb65a/src/service.rs#lines-111', score: 42, match_count: 2, stale: false,
  lines: [
    { number: 110, text: 'fn first() {}', ranges: [], is_context: true },
    { number: 111, text: 'let wanted = 1;', ranges: [{ start: 4, end: 10, atom: 0 }], is_context: false },
    { number: 253, text: 'let wanted = 2;', ranges: [{ start: 4, end: 10, atom: 0 }], is_context: false },
    { number: 254, text: 'fn last() {}', ranges: [], is_context: true },
  ],
}

async function renderGappedResult(page: import('@playwright/test').Page) {
  await page.route('**/api/v1/bootstrap', route => route.fulfill({ json: { csrf_token: 'test-csrf', version: '0.2.0', authenticated: true } }))
  await page.route('**/api/v1/repositories', route => route.fulfill({ json: catalog }))
  await page.route('**/api/v1/search', route => route.fulfill({ json: { id: '22222222-2222-4222-8222-222222222222' } }))
  const response = { query: ['wanted'], results: [gappedResult], repositories_searched: 1, files_searched: 3, elapsed_ms: 5, cached: false, truncated: false }
  await page.route('**/api/v1/search/*/events', route => route.fulfill({
    status: 200, contentType: 'text/event-stream',
    body: `data: ${JSON.stringify({ type: 'done', response })}\n\n`,
  }))
  await page.goto('/')
  await page.getByLabel('Search query').fill('wanted')
  await page.getByRole('button', { name: 'Search' }).click()
  await expect(page.getByRole('article')).toBeVisible()
}

test('marks a line jump as its own row instead of overlapping the gutter', async ({ page }) => {
  await renderGappedResult(page)
  const gap = page.locator('.code-gap')
  await expect(gap).toHaveCount(1)

  // the marker must sit in the gutter, not float over the line numbers
  const [gapBox, numberBox] = await Promise.all([
    gap.locator('.code-gap-mark').boundingBox(),
    page.locator('.code-line', { hasText: 'let wanted = 2;' }).locator('.line-number').boundingBox(),
  ])
  const gapBottom = gapBox!.y + gapBox!.height
  expect(gapBottom).toBeLessThanOrEqual(numberBox!.y + 1)

  // and 111 must not sit directly on top of 253
  const rows = await page.locator('.code-line, .code-gap').evaluateAll(els => els.map(e => e.className))
  expect(rows).toEqual(['code-line ', 'code-line matched', 'code-gap', 'code-line matched', 'code-line '])
})

test('collapses and expands a result', async ({ page }) => {
  await renderGappedResult(page)
  const card = page.getByRole('article')
  await expect(card.locator('.code-block')).toBeVisible()

  await card.getByRole('button', { name: 'Collapse result' }).click()
  await expect(card.locator('.code-block')).toHaveCount(0)

  await card.getByRole('button', { name: 'Expand result' }).click()
  await expect(card.locator('.code-block')).toBeVisible()
})

test('shows the platform shortcut and a monospaced grammar row', async ({ page }) => {
  await page.route('**/api/v1/bootstrap', route => route.fulfill({ json: { csrf_token: 'test', version: '0.2.0', authenticated: false } }))
  await page.route('**/api/v1/repositories?offline=true', route => route.fulfill({ json: { discovered_at: '2026-08-27T00:00:00Z', workspaces: [], repositories: [] } }))
  await page.goto('/')

  // the CI runner and this dev box are both linux, so the label must not say command
  await expect(page.locator('kbd')).toHaveText('Ctrl K')

  const family = await page.locator('.grammar').evaluate(el => getComputedStyle(el).fontFamily)
  expect(family).toContain('monospace')
})
