-- Run: nvim --headless -u NONE -l tests/nvim_typst.lua
-- Requires typst and tinymist on PATH; uses real compiler source positions.
vim.opt.runtimepath:prepend(vim.fn.getcwd())
local project = require('pdfterm.project')
local typst = require('pdfterm.typst')
local directory = vim.fn.tempname() .. ' typst navigation'
vim.fn.mkdir(directory, 'p')
directory = assert(vim.uv.fs_realpath(directory))
local alias = directory .. ' alias'
local original_root, original_fonts = vim.env.TYPST_ROOT, vim.env.TYPST_FONT_PATHS
vim.env.TYPST_ROOT, vim.env.TYPST_FONT_PATHS = nil, nil
local start_rpc = vim.lsp.rpc.start
local endpoints, initialized_args = {}, nil
vim.lsp.rpc.start = function(...)
  local rpc = start_rpc(...)
  local request = rpc.request
  rpc.request = function(method, params, ...)
    if method == 'initialize' then initialized_args = params.initializationOptions.typstExtraArgs end
    return request(method, params, ...)
  end
  return rpc
end
local function resolve(p, file, line, column)
  local result
  local cancel = typst.resolve(p, file, line, column, function(value)
    result = value
  end)
  if not vim.wait(60000, function() return result ~= nil end, 10) then
    cancel()
    error('Typst source resolution timed out')
  end
  assert(result.code == 0, result.stderr)
  local mapped = vim.json.decode(result.stdout)
  endpoints[#endpoints + 1] = assert(mapped.inverse_search)
  return mapped
end
local ok, failure = xpcall(function()
  vim.fn.writefile({
    '#set page(width: 300pt, height: 400pt, margin: 30pt)',
    '= First page',
    'First page anchor.',
    '#pagebreak()',
    '= Second page',
    'Unicode café λ anchor on page two.',
    '#pagebreak()',
    '#include "chapter.typ"',
  }, directory .. '/main.typ')
  vim.fn.writefile({ '= Third page', 'λ Included source anchor on page three.' }, directory .. '/chapter.typ')
  local p = project.describe({ main = 'main.typ', cwd = directory, pdf = 'custom output.pdf' })
  local built
  project.build(p, project.next(), function(result) built = result end)
  assert(vim.wait(30000, function() return built ~= nil end, 10), 'Typst build timed out')
  assert(built.code == 0, built.stderr)
  local second = resolve(p, p.main, 6, #'Unicode café λ ')
  assert(second.page == 2, vim.inspect(second))
  assert(second.pdf == p.pdf and second.h >= 30 and second.v >= 30, vim.inspect(second))
  local stat = assert(vim.uv.fs_stat(p.pdf))
  assert(second.revision.length == stat.size and second.revision.inode == stat.ino)
  -- Normal-mode cursors at line starts must address the character under them,
  -- including a multibyte character, rather than the preceding newline.
  local line_start = resolve(p, p.main, 6, 0)
  assert(line_start.page == 2, vim.inspect(line_start))
  assert(typst.is_coherent(p))
  local warm_done
  local warm_cancel = typst.resolve(p, directory .. '/chapter.typ', 2, 0, function(v) warm_done = v end)
  assert(vim.uv.fs_stat(line_start.inverse_search) ~= nil)
  if not vim.wait(60000, function() return warm_done ~= nil end, 10) then
    warm_cancel()
    error('Warm Typst resolution timed out')
  end
  assert(warm_done.code == 0, warm_done.stderr)
  local third = vim.json.decode(warm_done.stdout)
  endpoints[#endpoints + 1] = assert(third.inverse_search)
  assert(vim.uv.fs_stat(line_start.inverse_search) == nil)
  assert(vim.uv.fs_stat(third.inverse_search) ~= nil)
  assert(third.page == 3, vim.inspect(third))
  assert(third.pdf == p.pdf and third.v >= 30, vim.inspect(third))

  -- Cold forward with a missing PDF and missing output parent directory compiles
  -- directly, warm blank/comment timeout or cancel preserves the service and
  -- inverse socket, and typst.build refreshes through the retained service.
  vim.fn.mkdir(directory .. '/cold')
  vim.fn.writefile({ '// Unrendered comment.', 'Cold anchor.' }, directory .. '/cold/main.typ')
  local cold = project.describe({
    main = 'cold/main.typ', cwd = directory, pdf = 'cold-out/sub/output.pdf',
    build = { 'typst', 'compile', directory .. '/cold/main.typ', directory .. '/cold-out/sub/output.pdf' },
  })
  assert(vim.uv.fs_stat(cold.pdf) == nil)
  local cold_mapped = resolve(cold, cold.main, 2, 0)
  assert(cold_mapped.page == 1 and vim.uv.fs_stat(cold.pdf) ~= nil, vim.inspect(cold_mapped))
  assert(typst.is_coherent(cold))
  local cold_sock = cold_mapped.inverse_search
  local blank_res
  typst.resolve(cold, cold.main, 1, 0, function(v) blank_res = v end)
  assert(vim.wait(10000, function() return blank_res ~= nil end, 10), 'blank-line forward did not time out')
  assert(blank_res.code == 1 and blank_res.stderr:find('timed out', 1, true), vim.inspect(blank_res))
  assert(typst.is_coherent(cold) and vim.uv.fs_lstat(cold_sock) ~= nil)
  local warm_cancelled = false
  local cancel_warm = typst.resolve(cold, cold.main, 2, 0, function() warm_cancelled = true end)
  cancel_warm()
  assert(not warm_cancelled and typst.is_coherent(cold) and vim.uv.fs_lstat(cold_sock) ~= nil)
  local before_service_build = assert(vim.uv.fs_stat(cold.pdf))
  vim.fn.writefile({ '// Unrendered comment.', '#pagebreak()', 'Cold anchor.' }, cold.main)
  local build_notes, saved_notify = {}, vim.notify
  vim.notify = function(msg, _, opts) build_notes[#build_notes + 1] = { msg = msg, opts = opts or {} } end
  local ok_notify, err_notify = xpcall(function()
    local build_id = project.next()
    assert(typst.build(cold, build_id) == true)
    project.next() -- Superseding intent must still clear sticky Compiling… toast.
    assert(vim.wait(30000, function()
      local st = vim.uv.fs_stat(cold.pdf)
      return st ~= nil and st.ino ~= before_service_build.ino and typst.is_coherent(cold)
    end, 10), 'retained typst.build did not publish updated PDF')
    assert(#build_notes >= 2 and build_notes[1].msg == 'Compiling…' and build_notes[1].opts.timeout == false
      and build_notes[#build_notes].msg == 'Compilation OK' and build_notes[#build_notes].opts.timeout == 5000
      and build_notes[#build_notes].opts.id == build_notes[1].opts.id, vim.inspect(build_notes))
  end, debug.traceback)
  vim.notify = saved_notify
  assert(ok_notify, err_notify)
  assert(vim.uv.fs_lstat(cold_sock) ~= nil)
  local rebuilt_mapped = resolve(cold, cold.main, 3, 0)
  assert(rebuilt_mapped.page == 2, vim.inspect(rebuilt_mapped))

  -- Releasing a PDF while a cold resolve is still compiling stops Tinymist before publication.
  local release_res
  typst.resolve({
    main = cold.main, cwd = directory, pdf = directory .. '/released-cold.pdf',
    build = { 'typst', 'compile', cold.main, directory .. '/released-cold.pdf' },
  }, cold.main, 3, 0, function(v) release_res = v end)
  typst.release(directory .. '/released-cold.pdf')
  assert(vim.wait(5000, function() return release_res ~= nil end, 10), 'cold release did not finish')
  assert(release_res.code == 1 and vim.uv.fs_stat(directory .. '/released-cold.pdf') == nil)

  assert(vim.uv.fs_symlink(directory, alias))
  local alias_cold = {
    main = alias .. '/cold/main.typ', cwd = alias, pdf = alias .. '/nested-out/sub/output.pdf',
    build = { 'typst', 'compile', alias .. '/cold/main.typ', alias .. '/nested-out/sub/output.pdf' },
  }
  local alias_mapped = resolve(alias_cold, alias_cold.main, 3, 0)
  assert(alias_mapped.page == 2 and typst.is_coherent(alias_cold))
  assert(typst.is_coherent({
    main = directory .. '/cold/main.typ', cwd = directory, pdf = directory .. '/nested-out/sub/output.pdf',
    build = { 'typst', 'compile', directory .. '/cold/main.typ', directory .. '/nested-out/sub/output.pdf' },
  }), 'symlinked missing output directories split the retained service key')
  local linked = project.describe({
    main = alias .. '/main.typ', cwd = alias, pdf = alias .. '/custom output.pdf',
    build = { 'typst', 'compile', '--root', alias, directory .. '/main.typ', p.pdf },
  })
  local linked_third = resolve(linked, directory .. '/chapter.typ', 2, 9)
  assert(linked_third.page == 3, vim.inspect(linked_third))
  local linked_second = resolve(linked, alias .. '/main.typ', 6, #'Unicode café λ ')
  assert(linked_second.page == 2, vim.inspect(linked_second))

  -- A workspace directory is not Typst's implicit root: absolute imports
  -- default to the entry file's directory, even when the process cwd differs.
  vim.fn.mkdir(directory .. '/sources')
  vim.fn.writefile({ 'First page', '#include "/shared.typ"', 'Nested root anchor.' },
    directory .. '/sources/main.typ')
  vim.fn.writefile({ '#pagebreak()' }, directory .. '/sources/shared.typ')
  vim.fn.writefile({ 'Wrong workspace-root dependency.' }, directory .. '/shared.typ')
  local nested = project.describe({ main = 'sources/main.typ', cwd = directory })
  local nested_build = vim.system(nested.build, { cwd = nested.cwd, text = true }):wait()
  assert(nested_build.code == 0, nested_build.stderr)
  local nested_position = resolve(nested, nested.main, 3, 7)
  assert(nested_position.page == 2, vim.inspect(nested_position))
  vim.env.TYPST_ROOT = directory
  nested_build = vim.system(nested.build, { cwd = nested.cwd, text = true }):wait()
  assert(nested_build.code == 0, nested_build.stderr)
  local env_position = resolve(nested, nested.main, 3, 7)
  assert(env_position.page == 1, vim.inspect(env_position))
  nested.build = { 'typst', 'compile', '--root', directory .. '/sources', nested.main, nested.pdf }
  nested_build = vim.system(nested.build, { cwd = nested.cwd, text = true }):wait()
  assert(nested_build.code == 0, nested_build.stderr)
  local explicit_position = resolve(nested, nested.main, 3, 7)
  assert(explicit_position.page == 2, vim.inspect(explicit_position))

  -- The font CLI has path-list semantics, but its relative paths are resolved
  -- from the build cwd, independently of the legitimate Typst import root.
  local separator = vim.fn.has('win32') == 1 and ';' or ':'
  for _, name in ipairs({ 'cli one', 'cli-two', 'absolute', 'env one', 'env-two', 'ignored' }) do
    vim.fn.mkdir(directory .. '/fonts/' .. name, 'p')
  end
  local saved_build = nested.build
  local function font_paths(argv, inherited, expected, page)
    nested.build, vim.env.TYPST_FONT_PATHS = argv, inherited
    local built_fonts = vim.system(nested.build, { cwd = nested.cwd, text = true }):wait()
    assert(built_fonts.code == 0, built_fonts.stderr)
    local mapped = resolve(nested, nested.main, 3, 7)
    assert(mapped.page == page, vim.inspect(mapped))
    assert(vim.deep_equal(initialized_args, expected), vim.inspect(initialized_args))
  end
  vim.env.TYPST_ROOT = nil
  font_paths({
    'typst', 'compile', '--ignore-system-fonts',
    '--font-path=fonts/cli one' .. separator .. 'fonts/cli-two',
    '--font-path', directory .. '/fonts/absolute',
    '--package-path', 'packages', '--package-cache-path', 'cache', nested.main, nested.pdf,
  }, 'fonts/ignored', {
    '--ignore-system-fonts',
    '--font-path', directory .. '/fonts/cli one',
    '--font-path', directory .. '/fonts/cli-two',
    '--font-path', directory .. '/fonts/absolute',
    '--package-path', 'packages', '--package-cache-path', 'cache',
    '--root', directory .. '/sources', nested.main,
  }, 2)
  font_paths({
    'typst', 'compile', '--ignore-system-fonts', '--root', 'sources', nested.main, nested.pdf,
  }, 'fonts/env one' .. separator .. 'fonts/env-two', {
    '--ignore-system-fonts', '--root', directory .. '/sources',
    '--font-path', directory .. '/fonts/env one',
    '--font-path', directory .. '/fonts/env-two', nested.main,
  }, 2)
  font_paths({
    'typst', 'compile', '--root', directory, '--font-path', directory .. '/fonts/absolute',
    nested.main, nested.pdf,
  }, nil, {
    '--root', directory, '--font-path', directory .. '/fonts/absolute', nested.main,
  }, 1)
  nested.build, vim.env.TYPST_FONT_PATHS, vim.env.TYPST_ROOT = saved_build, nil, directory
  nested_build = vim.system(nested.build, { cwd = nested.cwd, text = true }):wait()
  assert(nested_build.code == 0, nested_build.stderr)

  -- Save an included source after the real PDF export, before the real jump.
  -- The resolver must keep the original PDF instead of publishing mixed state.
  local unchanged = assert(vim.uv.fs_stat(nested.pdf))
  vim.lsp.rpc.start = function(...)
    local rpc = start_rpc(...)
    local request = rpc.request
    rpc.request = function(method, params, ...)
      if method == 'workspace/executeCommand' and params.command == 'tinymist.scrollPreview' then
        vim.fn.writefile({ '#pagebreak()', '#pagebreak()' }, directory .. '/sources/shared.typ')
      end
      return request(method, params, ...)
    end
    return rpc
  end
  local interrupted
  local cancel = typst.resolve(nested, nested.main, 3, 7, function(value) interrupted = value end)
  if not vim.wait(30000, function() return interrupted ~= nil end, 10) then
    cancel()
    error('Source-change navigation did not finish')
  end
  assert(interrupted.code ~= 0 and interrupted.stderr:find('changed', 1, true), vim.inspect(interrupted))
  local retained = assert(vim.uv.fs_stat(nested.pdf))
  assert(retained.ino == unchanged.ino and vim.deep_equal(retained.mtime, unchanged.mtime))
end, debug.traceback)
vim.lsp.rpc.start = start_rpc
vim.env.TYPST_ROOT, vim.env.TYPST_FONT_PATHS = original_root, original_fonts
project.close()
vim.api.nvim_exec_autocmds('VimLeavePre', {})
local cleaned = vim.wait(5000, function()
  for _, endpoint in ipairs(endpoints) do
    if vim.uv.fs_lstat(vim.fs.dirname(endpoint)) then return false end
  end
  return true
end, 10)
vim.uv.fs_unlink(alias)
vim.fn.delete(directory, 'rf')
assert(ok, failure)
assert(cleaned, 'Typst source-map service did not clean up on Neovim exit')
print('Typst source-position tests passed')
