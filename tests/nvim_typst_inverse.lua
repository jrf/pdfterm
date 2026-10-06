-- Run: nvim --headless -u NONE -l tests/nvim_typst_inverse.lua
-- Real Tinymist compiler spans, private EOF-framed inverse sockets, no editor effects.
vim.opt.runtimepath:prepend(vim.fn.getcwd())
local project = require('pdfterm.project')
local typst = require('pdfterm.typst')
local directories, endpoints = {}, {}
local function directory()
  local path = assert(vim.uv.fs_mkdtemp('/tmp/pdfterm-inverse-test-XXXXXX'))
  path = assert(vim.uv.fs_realpath(path))
  directories[#directories + 1] = path
  return path
end
local function build(p)
  local result = vim.system(p.build, { cwd = p.cwd, text = true }):wait()
  assert(result.code == 0, result.stderr)
end
local function revision(path)
  local stat = assert(vim.uv.fs_stat(path), 'missing compiled PDF: ' .. path)
  return {
    device = stat.dev, inode = stat.ino, length = stat.size,
    modified_seconds = stat.mtime.sec, modified_nanoseconds = stat.mtime.nsec,
    changed_seconds = stat.ctime.sec, changed_nanoseconds = stat.ctime.nsec,
  }
end
local function current(mapped, pdf)
  local value = vim.deepcopy(mapped)
  value.revision = revision(pdf)
  return value
end
local function private_endpoint(endpoint)
  local socket = assert(vim.uv.fs_lstat(endpoint), 'inverse endpoint disappeared')
  local parent = assert(vim.uv.fs_lstat(vim.fs.dirname(endpoint)))
  assert(socket.type == 'socket' and socket.uid == vim.uv.getuid() and bit.band(socket.mode, 511) == 384)
  assert(parent.type == 'directory' and parent.uid == vim.uv.getuid() and bit.band(parent.mode, 511) == 448)
end
local function forward(p, file, row, column)
  local result, calls = nil, 0
  local cancel = typst.resolve(p, file, row, column, function(value)
    result, calls = value, calls + 1
  end)
  assert(vim.wait(60000, function() return result ~= nil end, 10), 'forward source map timed out')
  assert(result.code == 0, result.stderr)
  local mapped = vim.json.decode(result.stdout)
  assert(type(mapped.inverse_search) == 'string' and mapped.inverse_search:sub(1, 1) == '/')
  endpoints[#endpoints + 1] = mapped.inverse_search
  private_endpoint(mapped.inverse_search)
  -- Forward cancellation is finished: it must not kill a published inverse map.
  cancel()
  return mapped, function() assert(calls == 1, 'forward callback repeated after publication') end
end
local function request(mapped, override, raw, abandon)
  local pipe = assert(vim.uv.new_pipe(false))
  local state = { data = '', finished = false }
  local function close()
    if not pipe:is_closing() then pipe:read_stop(); pipe:close() end
  end
  pipe:connect(mapped.inverse_search, function(err)
    if err then state.failure, state.finished = err, true; close(); return end
    if not abandon then
      pipe:read_start(function(read_error, chunk)
        if read_error then state.failure = read_error end
        if not chunk or read_error then
          state.finished = true
          close()
        else
          state.data = state.data .. chunk
          assert(#state.data <= 4096, 'oversized inverse response')
        end
      end)
    end
    local value = vim.tbl_extend('force', {
      revision = mapped.revision, page = mapped.page, x = mapped.h + 0.5, y = mapped.v - 0.5,
    }, override or {})
    pipe:write(raw or vim.json.encode(value), function(write_error)
      if write_error then state.failure, state.finished = write_error, true; close(); return end
      pipe:shutdown(function(shutdown_error)
        if shutdown_error then state.failure, state.finished = shutdown_error, true; close(); return end
        if abandon then state.finished = true; close() end
      end)
    end)
  end)
  return state
end
local function finish(state)
  assert(vim.wait(15000, function() return state.finished end, 10), 'inverse source map timed out')
  assert(not state.failure, state.failure)
  return vim.json.decode(state.data)
end
local function query(mapped, override, raw)
  return finish(request(mapped, override, raw))
end
local function abandon(mapped, override)
  local state = request(mapped, override, nil, true)
  assert(vim.wait(5000, function() return state.finished end, 10), 'inverse request was not abandoned')
  assert(not state.failure, state.failure)
end
local function precise_response(response, file, row, source, anchor)
  assert(response.ok, vim.inspect(response))
  local loc = response.location
  assert(loc.precise and loc.file == file and loc.line == row, vim.inspect(response))
  local start = assert(source:find(anchor, 1, true)) - 1
  assert(loc.byte_column >= start and loc.byte_column < start + #anchor, vim.inspect(response))
  local following = source:byte(loc.byte_column + 1)
  assert(not following or following < 128 or following >= 192, 'inverse column splits UTF-8')
  local prefix = source:sub(1, loc.byte_column)
  local scalars = vim.fn.strchars(prefix)
  local _, astral = prefix:gsub('[\240-\244]', '')
  assert(loc.column_char == scalars + 1 and loc.column == scalars + astral + 1, vim.inspect(response))
end
local function precise(mapped, file, row, source, anchor)
  precise_response(query(mapped), file, row, source, anchor)
end
local function rejected(response)
  assert(response.ok == false, vim.inspect(response))
end
-- A CLI build and Tinymist export need not have identical PDF bytes. Wait for
-- the service's coherent export, then require two valid queries at the same
-- observed PDF revision. No second forward operation supplies a refreshed map.
local function refreshed(mapped, pdf, before, cli, file, row, source, anchor)
  local result, candidate
  assert(vim.wait(60000, function()
    private_endpoint(mapped.inverse_search)
    local observed = current(mapped, pdf)
    if vim.deep_equal(observed.revision, before) or vim.deep_equal(observed.revision, cli) then return false end
    local response = query(observed)
    if not response.ok or not vim.deep_equal(observed.revision, revision(pdf)) then
      candidate = nil
      return false
    end
    precise_response(response, file, row, source, anchor)
    if candidate and vim.deep_equal(candidate, observed.revision) then result = observed; return true end
    candidate = observed.revision
    return false
  end, 20), 'saved compile never published a coherent inverse map')
  return result
end
local function recovered(mapped, pdf, file, row, source, anchor)
  local result
  assert(vim.wait(60000, function()
    private_endpoint(mapped.inverse_search)
    local observed = current(mapped, pdf)
    local response = query(observed)
    if not response.ok or not vim.deep_equal(observed.revision, revision(pdf)) then return false end
    -- A reply from the abandoned included-page request must never satisfy this
    -- main-page request: successful but misattributed locations fail outright.
    precise_response(response, file, row, source, anchor)
    result = observed
    return true
  end, 20), 'inverse service did not recover through its existing endpoint')
  return result
end
local ok, failure = xpcall(function()
  local root = directory()
  local main_line = 'Plain café λ 𝔸 *anchor* on page two.'
  local included_line = 'λ café 𝔸 *includedanchor* on page three.'
  local function save_main(first_page, saved)
    local lines = { '#set page(width: 360pt, height: 400pt, margin: 30pt)' }
    -- Non-rendering source lines deliberately move the source row while the
    -- anchor's page/geometry stays fixed. No guessed refreshed coordinates.
    if saved then lines[#lines + 1] = '#let saved_revision = 1' end
    vim.list_extend(lines, {
      first_page, '#pagebreak()', main_line, '#pagebreak()', '#include "chapter.typ"',
    })
    vim.fn.writefile(lines, root .. '/main.typ')
  end
  save_main('First page.', false)
  -- CRLF is part of the saved source; returned columns never include its CR.
  vim.fn.writefile({ '= Included page\r', included_line .. '\r' }, root .. '/chapter.typ')
  local p = project.describe({ main = 'main.typ', cwd = root })
  build(p)
  local main, main_once = forward(p, p.main, 4, #'Plain café λ 𝔸 *')
  assert(main.page == 2, vim.inspect(main))
  precise(main, p.main, 4, main_line, 'anchor')
  local stale = vim.deepcopy(main.revision)
  stale.length = stale.length + 1
  rejected(query(main, { revision = stale }))
  rejected(query(main, { page = 0 }))
  rejected(query(main, nil, '{not JSON'))
  rejected(query(main, nil, string.rep('x', 4097)))
  precise(main, p.main, 4, main_line, 'anchor')

  -- A new explicit forward operation still replaces and cleans its old
  -- endpoint. Internal refreshes below must not require another operation.
  local included, included_once = forward(p, root .. '/chapter.typ', 2, #'λ café 𝔸 *')
  assert(included.page == 3, vim.inspect(included))
  assert(vim.wait(5000, function()
    return not vim.uv.fs_lstat(vim.fs.dirname(main.inverse_search))
  end, 10), 'explicit forward replacement retained its old private endpoint')
  local endpoint = included.inverse_search
  local main_point = current(main, p.pdf)
  main_point.inverse_search = endpoint
  precise(included, root .. '/chapter.typ', 2, included_line, 'includedanchor')
  for _ = 1, 12 do
    precise(main_point, p.main, 4, main_line, 'anchor')
    precise(included, root .. '/chapter.typ', 2, included_line, 'includedanchor')
  end
  main_once()

  local other_root = directory()
  local independent_line = 'Independent source *anchor*.'
  vim.fn.writefile({ independent_line }, other_root .. '/main.typ')
  local other = project.describe({ main = 'main.typ', cwd = other_root })
  build(other)
  local independent, independent_once = forward(other, other.main, 1, #'Independent source *')
  precise(independent, other.main, 1, independent_line, 'anchor')
  precise(included, root .. '/chapter.typ', 2, included_line, 'includedanchor')

  -- Unread root files and atomic/swap-style saves are not dependencies. Keep
  -- querying the original revision over several automatic polling cycles.
  vim.fn.writefile({ 'unrelated' }, root .. '/notes.txt')
  vim.fn.writefile({ 'swap replacement' }, root .. '/.main.typ.swp')
  assert(vim.uv.fs_rename(root .. '/.main.typ.swp', root .. '/notes.txt'))
  local until_ns = vim.uv.hrtime() + 1000000000
  assert(vim.wait(5000, function()
    precise(main_point, p.main, 4, main_line, 'anchor')
    assert(vim.deep_equal(main_point.revision, revision(p.pdf)), 'unread root file invalidated the PDF map')
    return vim.uv.hrtime() >= until_ns
  end, 25), 'unrelated-file stability check timed out')
  assert(vim.uv.fs_unlink(root .. '/notes.txt'))
  precise(main_point, p.main, 4, main_line, 'anchor')

  -- Main save plus normal CLI recompilation automatically exports a coherent
  -- map. The unchanged Text leaf now belongs to source row five, not four.
  local old_main = vim.deepcopy(main_point)
  save_main('First page changed after saving main.', true)
  build(p)
  local cli = revision(p.pdf)
  main_point = refreshed(main_point, p.pdf, old_main.revision, cli, p.main, 5, main_line, 'anchor')
  assert(main_point.inverse_search == endpoint)
  rejected(query(old_main))
  included = current(included, p.pdf)
  precise(included, root .. '/chapter.typ', 2, included_line, 'includedanchor')
  precise(independent, other.main, 1, independent_line, 'anchor')

  -- Included saves shift source rows but leave the anchor's rendered position
  -- intact: the added comment renders nothing, and new text follows the leaf.
  local old_included = vim.deepcopy(included)
  included_line = included_line .. ' Changed after saving the dependency.'
  vim.fn.writefile({
    '// Saved dependency\r', '= Included page\r', included_line .. '\r',
  }, root .. '/chapter.typ')
  build(p)
  cli = revision(p.pdf)
  included = refreshed(included, p.pdf, old_included.revision, cli,
    root .. '/chapter.typ', 3, included_line, 'includedanchor')
  assert(included.inverse_search == endpoint)
  rejected(query(old_included))
  main_point = current(main_point, p.pdf)
  precise(main_point, p.main, 5, main_line, 'anchor')
  precise(independent, other.main, 1, independent_line, 'anchor')

  -- Tinymist sends no source notification for an unrendered point. Its timeout
  -- must retire only the ambiguous compiler generation, never the endpoint.
  local before_timeout = vim.deepcopy(main_point)
  rejected(query(main_point, { page = 4294967295 }))
  main_point = recovered(main_point, p.pdf, p.main, 5, main_line, 'anchor')
  included = current(included, p.pdf)
  precise(included, root .. '/chapter.typ', 3, included_line, 'includedanchor')
  if not vim.deep_equal(before_timeout.revision, main_point.revision) then
    rejected(query(before_timeout))
  end

  -- Cancel a real included-page request, then request a different location.
  -- A late notification must not be delivered as the newer main-page reply.
  abandon(included)
  main_point = recovered(main_point, p.pdf, p.main, 5, main_line, 'anchor')
  included = current(included, p.pdf)
  precise(included, root .. '/chapter.typ', 3, included_line, 'includedanchor')
  abandon(main_point, { page = 4294967295 })
  main_point = recovered(main_point, p.pdf, p.main, 5, main_line, 'anchor')
  included = current(included, p.pdf)
  precise(included, root .. '/chapter.typ', 3, included_line, 'includedanchor')

  -- Rapid competing requests may be safely superseded during a generation
  -- restart, but every successful reply must belong to its own requested page.
  local burst = {
    { request(included), root .. '/chapter.typ', 3, included_line, 'includedanchor' },
    { request(main_point), p.main, 5, main_line, 'anchor' },
    { request(included), root .. '/chapter.typ', 3, included_line, 'includedanchor' },
    { request(main_point), p.main, 5, main_line, 'anchor' },
  }
  for _, item in ipairs(burst) do
    local response = finish(item[1])
    if response.ok then precise_response(response, item[2], item[3], item[4], item[5])
    else rejected(response) end
  end
  main_point = recovered(main_point, p.pdf, p.main, 5, main_line, 'anchor')
  included = current(included, p.pdf)
  precise(included, root .. '/chapter.typ', 3, included_line, 'includedanchor')
  rejected(query(main_point, nil, 'null'))
  precise(main_point, p.main, 5, main_line, 'anchor')

  -- A failed compile preserves the private service, rejects the old map, and
  -- recovers automatically after the next successful save/CLI compile.
  local before_failure = vim.deepcopy(main_point)
  vim.fn.writefile({ '#panic("deliberate inverse regression compile failure")' }, p.main)
  local failed_build = vim.system(p.build, { cwd = p.cwd, text = true }):wait()
  assert(failed_build.code ~= 0, 'deliberately invalid source unexpectedly compiled')
  rejected(query(current(main_point, p.pdf)))
  private_endpoint(endpoint)
  precise(independent, other.main, 1, independent_line, 'anchor')
  save_main('First page recovered after a failed compile.', true)
  build(p)
  cli = revision(p.pdf)
  main_point = refreshed(main_point, p.pdf, before_failure.revision, cli,
    p.main, 5, main_line, 'anchor')
  rejected(query(before_failure))
  included = current(included, p.pdf)
  precise(included, root .. '/chapter.typ', 3, included_line, 'includedanchor')

  -- An independently rewritten PDF is stale too, without poisoning either
  -- document. Refresh it through its existing endpoint, not another forward.
  local old_independent = vim.deepcopy(independent)
  local fd = assert(vim.uv.fs_open(other.pdf, 'a', 384))
  assert(vim.uv.fs_write(fd, '\n', -1))
  assert(vim.uv.fs_close(fd))
  rejected(query(old_independent))
  independent = recovered(independent, other.pdf, other.main, 1, independent_line, 'anchor')
  rejected(query(old_independent))
  precise(main_point, p.main, 5, main_line, 'anchor')
  precise(included, root .. '/chapter.typ', 3, included_line, 'includedanchor')

  -- Releasing this PDF to another selected source/build must prevent its old
  -- automatic publisher from overwriting the replacement document.
  typst.release(p.pdf)
  assert(not vim.uv.fs_lstat(endpoint), 'released PDF retained its inverse endpoint')
  local replacement = vim.system({ 'typst', 'compile', other.main, p.pdf }, {
    cwd = other.cwd, text = true,
  }):wait()
  assert(replacement.code == 0, replacement.stderr)
  local foreign = revision(p.pdf)
  vim.wait(600, function() return false end, 10)
  assert(vim.deep_equal(foreign, revision(p.pdf)), 'released map overwrote another builder output')
  precise(independent, other.main, 1, independent_line, 'anchor')
  main_once(); included_once(); independent_once()
end, debug.traceback)
vim.api.nvim_exec_autocmds('VimLeavePre', {})
local cleaned = vim.wait(5000, function()
  for _, endpoint in ipairs(endpoints) do
    if vim.uv.fs_lstat(vim.fs.dirname(endpoint)) then return false end
  end
  return true
end, 10)
for _, path in ipairs(directories) do vim.fn.delete(path, 'rf') end
assert(ok, failure)
assert(cleaned, 'retained Typst service did not clean up on Neovim exit')
print('Typst inverse source-map tests passed')
