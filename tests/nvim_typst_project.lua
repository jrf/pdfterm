-- Run from the repository: nvim --headless -u NONE -l tests/nvim_typst_project.lua
-- Requires typst on PATH for the compilation scenario.
vim.opt.runtimepath:prepend(vim.fn.getcwd())
local project = require('pdfterm.project')
local directory = vim.fn.tempname() .. ' typst project'
vim.fn.mkdir(directory .. '/sources', 'p')
vim.fn.mkdir(directory .. '/output', 'p')
directory = assert(vim.uv.fs_realpath(directory))
local function put(path, lines)
  vim.fn.writefile(lines, directory .. '/' .. path)
end
local function equal(expected, actual)
  assert(vim.deep_equal(expected, actual), vim.inspect({ expected = expected, actual = actual }))
end
local ok, failure = xpcall(function()
  -- Typst text that resembles TeX must not trigger root directives or graph scanning.
  put('sources/current.typ', { '% !TeX root = missing.tex', 'A Typst document.' })
  put('owner.tex', { '\\documentclass{article}', '\\input{sources/current.typ}' })
  local current = project.describe(nil, directory .. '/sources/current.typ')
  equal(directory .. '/sources/current.typ', current.main)
  equal(directory .. '/sources/current.pdf', current.pdf)
  equal(directory .. '/sources', current.cwd)

  put('sources/main.typ', { '= Explicit main', 'Compiled from the configured project.' })
  local configured = project.describe({
    main = 'sources/main.typ',
    cwd = directory,
    pdf = 'output/custom name.pdf',
  }, directory .. '/sources/current.typ')
  equal(directory .. '/sources/main.typ', configured.main)
  equal(directory .. '/output/custom name.pdf', configured.pdf)
  equal(directory, configured.cwd)

  -- Exercise the real serialized build path, including spaces and an output outside
  -- the source directory. A missing explicit output would silently write main.pdf.
  local result
  project.build(configured, project.next(), function(value)
    result = value
  end)
  assert(vim.wait(30000, function() return result ~= nil end, 10), 'Typst build timed out')
  assert(result.code == 0, result.stderr)
  local pdf = assert(io.open(configured.pdf, 'rb'))
  local header = pdf:read(5)
  pdf:close()
  equal('%PDF-', header)
  equal(0, vim.fn.filereadable(directory .. '/sources/main.pdf'))

  local custom = { 'custom-builder', 'argument with spaces' }
  local overridden = project.describe({
    main = 'sources/main.typ', cwd = directory,
    pdf = directory .. '/output/absolute.pdf', build = custom,
  })
  equal(custom, overridden.build)
  equal(directory .. '/output/absolute.pdf', overridden.pdf)
  assert(not pcall(project.describe, { main = 'sources/main.typ', cwd = directory, build = {} }))

  -- Existing TeX root directives still choose the owning document.
  put('sources/chapter.tex', { '% !TeX root = ../owner.tex', 'A chapter.' })
  local tex = project.describe(nil, directory .. '/sources/chapter.tex')
  equal(directory .. '/owner.tex', tex.main)
  equal(directory .. '/owner.pdf', tex.pdf)
  equal({
    'latexmk', '-pdf', '-interaction=nonstopmode', '-synctex=1',
    '-outdir=' .. directory, '-jobname=owner', tex.main,
  }, tex.build)
end, debug.traceback)
project.close()
vim.fn.delete(directory, 'rf')
assert(ok, failure)
print('Typst project tests passed')
