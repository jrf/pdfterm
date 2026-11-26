-- Run: nvim --headless -u NONE -i NONE -n -l tests/nvim_source.lua
vim.opt.runtimepath:prepend(vim.fn.getcwd())
local source = require('pdfterm.source')
local directory = vim.fn.tempname()
vim.fn.mkdir(directory, 'p')
local file = directory .. '/saved.tex'
local function bytes(text)
  local fd = assert(vim.uv.fs_open(file, 'w', 384))
  assert(vim.uv.fs_write(fd, text, 0))
  assert(vim.uv.fs_close(fd))
end
local ok, failure = xpcall(function()
  local text = 'Copper élan 雪\nAmber zephyr.\n'
  bytes(text)
  assert(source.read_input(file) == text)
  vim.cmd.edit(vim.fn.fnameescape(file))
  vim.api.nvim_buf_set_lines(0, 0, 0, false, { 'Unsaved preface.' })
  assert(source.read_input(file) == text, 'saved input reader must ignore live buffer edits')
  vim.cmd('silent write')
  assert(source.read_input(file) == 'Unsaved preface.\n' .. text)
  for _, invalid in ipairs({ '\0', '\192\128', '\237\160\128', '\244\144\128\128', '\226\130' }) do
    bytes(invalid)
    local value, reason = source.read_input(file)
    assert(value == nil and reason == 'source is not valid UTF-8 text')
  end
  bytes(string.rep('x', 2 * 1024 * 1024 + 1))
  local value, reason = source.read_input(file)
  assert(value == nil and reason:find('exceeds', 1, true))
  assert(source.read_input(directory) == nil)
  assert(source.read_input(directory .. '/missing.tex') == nil)
  bytes('% !TEX root = saved.tex\n\\documentclass{article}\n' .. string.rep('zibble quuxle\n', 160000))
  assert(source.read_header(file, 20):find('!TEX root', 1, true))
  assert(require('pdfterm.project').describe({}, file).main == (vim.uv.fs_realpath(file) or file))

  -- Synthetic include graphs exercise both discovery and saved-metadata caches.
  local project = require('pdfterm.project')
  local graph = directory .. '/graph'
  vim.fn.mkdir(graph .. '/parts', 'p')
  vim.fn.mkdir(graph .. '/.jj')
  graph = assert(vim.uv.fs_realpath(graph))
  local function put(name, text)
    vim.fn.writefile(vim.split(text, '\n', { plain = true }), graph .. '/' .. name)
  end
  put('main.tex', '\\documentclass{article}\n\\input{bridge.inc}')
  put('bridge.inc', '\\input{parts/leaf}')
  put('parts/leaf.tex', 'Zibble quarn.')
  local leaf, main = graph .. '/parts/leaf.tex', graph .. '/main.tex'
  local read_input, scandir = source.read_input, vim.uv.fs_scandir
  local reads, scans = {}, 0
  source.read_input = function(path)
    reads[path] = (reads[path] or 0) + 1
    return read_input(path)
  end
  vim.uv.fs_scandir = function(...)
    scans = scans + 1
    return scandir(...)
  end
  local checked, reason = xpcall(function()
    local function describe(text, config)
      return project.describe(config, leaf, text).main
    end
    assert(describe() == main)
    local initial_scans = scans
    assert(describe() == main)
    assert(scans == initial_scans, 'unchanged root discovery rescanned directories')
    assert(reads[main] == 1 and reads[graph .. '/bridge.inc'] == 1,
      'unchanged saved include metadata was reread')
    put('unrelated.txt', 'Flarn.')
    assert(describe() == main)
    assert(scans > initial_scans and reads[main] == 1,
      'directory invalidation discarded unchanged saved metadata')
    assert(describe('\\documentclass{article}') == leaf, 'live document metadata remained cached')
    local live_scans = scans
    assert(describe('Zibble quarn.') == main)
    assert(scans > live_scans, 'live buffer text must not populate root_cache')
    assert(describe() == main, 'saved cache was poisoned by live text')
    assert(describe('% !TEX root = ../main.tex') == main, 'live root directive remained cached')
    put('other.tex', '\\documentclass{article}\n\\input{parts/leaf}')
    assert(not pcall(describe), 'new ambiguous root was missed')
    assert(vim.uv.fs_unlink(graph .. '/other.tex'))
    assert(describe() == main, 'removed candidate remained cached')
    put('main.tex', '\\documentclass{article}')
    assert(describe() == leaf, 'saved candidate revision was ignored')
    put('main.tex', '\\documentclass{article}\n\\input{bridge.inc}')
    assert(describe() == main)
    put('bridge.inc', 'Plonk.')
    assert(describe() == leaf, 'non-TeX include revision was ignored')
    put('bridge.inc', '\\input{parts/leaf}')
    assert(describe() == main)
    put('bridge.inc.tex', 'Dribbet.')
    assert(describe() == leaf, 'new preferred include path was missed')
    assert(vim.uv.fs_unlink(graph .. '/bridge.inc.tex'))
    assert(describe() == main)
    assert(describe(nil, { cwd = graph .. '/parts' }) == leaf,
      'root discovery cache ignored the configured compilation directory')
    assert(describe() == main)
    put('parts/leaf.tex', '\\documentclass{article}')
    assert(describe() == leaf)
    assert(describe('Zibble quarn.') == main, 'live fragment reused saved document metadata')
    put('parts/leaf.tex', 'Zibble quarn.')
    assert(describe() == main, 'saved source revision remained cached')

    put('other.tex', '\233')
    assert(describe() == main)
    local invalid_reads, invalid_scans = reads[graph .. '/other.tex'], scans
    assert(invalid_reads ~= nil)
    assert(describe() == main)
    assert(reads[graph .. '/other.tex'] == invalid_reads and scans == invalid_scans,
      'stable non-UTF-8 candidate defeated the include or root cache')
    assert(vim.uv.fs_unlink(graph .. '/other.tex'))

    -- Simulate a short read during concurrent truncation.
    -- Failed reads must not populate either discovery or metadata caches.
    put('main.tex', '\\documentclass{article}\n\\input{parts/leaf}\n% Flarn.')
    local fs_read, shorted = vim.uv.fs_read, false
    vim.uv.fs_read = function(fd, count, offset)
      local chunk, err = fs_read(fd, count, offset)
      if chunk and not shorted and chunk:find('Flarn', 1, true) then
        shorted = true
        return chunk:sub(1, #chunk - 1)
      end
      return chunk, err
    end
    local attempted, result = pcall(describe)
    vim.uv.fs_read = fs_read
    assert(attempted and result == leaf and shorted,
      'truncated read failure was ignored')
    local failed_reads = reads[main]
    assert(describe() == main and reads[main] == failed_reads + 1,
      'failed saved read poisoned the discovery or metadata cache')
  end, debug.traceback)
  source.read_input, vim.uv.fs_scandir = read_input, scandir
  assert(checked, reason)
  local mb = directory .. '/multibyte-header.tex'
  local prefix = string.rep('z', 65534)
  local f = assert(io.open(mb, 'wb')); f:write(prefix .. '𐍈quarn\n'); f:close()
  local hdr, hdr_err = source.read_header(mb)
  assert(hdr == prefix and hdr_err == nil, 'read_header must trim incomplete trailing UTF-8 sequence')
  print('saved source readers passed: saved bytes, Unicode, invalid text, bounds and file types')
end, debug.traceback)
vim.fn.delete(directory, 'rf')
if not ok then io.stderr:write(failure .. '\n'); vim.cmd('cquit 1') else vim.cmd('qa!') end
