-- Run with PDFTERM_EXECUTABLE set, like tests/nvim.lua.
local root = vim.fn.getcwd()
vim.opt.runtimepath:prepend(root)
local binary = assert(vim.env.PDFTERM_EXECUTABLE)
local function wait(predicate)
  assert(vim.wait(10000, predicate, 2), 'session operation timed out')
end
local case = vim.env.PDFTERM_SESSION_CASE
if case then
  local directory = assert(vim.env.XDG_CONFIG_HOME)
  if case == 'defaults' then
    vim.opt.runtimepath:prepend(directory .. '/plugin')
  end
  if case == 'make_stale' then
    local config = vim.json.decode(
      vim.system({ binary, '--session', 'shared', '--print-config' }, { text = true }):wait().stdout
    )
    local server = assert(vim.uv.new_pipe(false))
    assert(server:bind(config.editor.path))
    assert(server:listen(1, function() end))
    vim.uv.kill(vim.fn.getpid(), 9)
    return
  end
  local messages = {}
  vim.notify = function(message)
    messages[#messages + 1] = tostring(message)
  end
  local copied
  vim.g.clipboard = {
    name = 'test clipboard',
    copy = {
      ['+'] = function(lines)
        copied = table.concat(lines, '\n')
      end,
      ['*'] = function() end,
    },
    paste = {
      ['+'] = function() return {}, 'v' end,
      ['*'] = function() return {}, 'v' end,
    },
  }
  vim.env.SSH_CONNECTION = '127.0.0.1 50000 127.0.0.1 22'
  -- Even forwarded terminal identifiers must not trigger local window control.
  vim.env.KITTY_WINDOW_ID, vim.env.TERM_PROGRAM = '123', 'ghostty'
  local terminal = require('pdfterm.terminal')
  terminal.capture_source = function()
    error('SSH must not capture local terminals')
  end
  if case == 'auto_tty' then
    vim.env.SSH_CONNECTION, vim.env.SSH_TTY = nil, '/dev/pts/1'
  else
    vim.env.SSH_TTY = nil
  end
  terminal.focus = function()
    error('SSH must not focus local terminals')
  end
  terminal.launch_split = function()
    error('SSH must not launch local terminals')
  end
  local adapter = require('pdfterm')
  local started = vim.uv.hrtime()
  if case == 'defaults' then
    adapter.setup()
  else
    adapter.setup({
      executable = case == 'missing' and directory .. '/missing-viewer'
        or directory .. '/slow-viewer',
      session = case == 'invalid' and '../invalid'
        or (case == 'live' or case == 'stale') and 'shared'
        or nil,
      keys = { forward = '', build = '<F6>', main_file = '<F5>', compile = '' },
      open_pdf = true,
    })
  end
  local setup_ns = vim.uv.hrtime() - started
  assert(vim.fn.exists(':PdfTermViewerCommand') == 2)
  vim.cmd('enew') -- Startup and ordinary editing survive every configuration/socket failure.
  vim.api.nvim_buf_set_lines(0, 0, -1, false, { 'still editable' })
  if case == 'missing' or case == 'invalid' then
    wait(function()
      return #messages > 0
    end)
    assert(messages[1]:match('configuration:'))
    assert(vim.api.nvim_get_current_line() == 'still editable')
    local before = #messages
    vim.api.nvim_buf_set_name(0, directory .. '/failure.tex')
    vim.fn.maparg('<F5>', 'n', false, true).callback()
    wait(function()
      return #messages > before
    end)
    assert(messages[#messages]:match('configuration:'), 'broken integration lost its action error')
    vim.bo.modified = false
    before = #messages
    vim.cmd.edit(vim.fn.fnameescape(directory .. '/paper.pdf'))
    wait(function()
      return #messages > before
    end)
    assert(vim.bo.buftype == 'nofile' and not vim.bo.modifiable and not vim.bo.modified)
    before = #messages
    vim.fn.maparg('<CR>', 'n', false, true).callback()
    wait(function()
      return #messages > before
    end)
    assert(messages[#messages]:match('configuration:'), 'PDF retry lost its dependency error')
    print(vim.json.encode({ case = case, setup_ns = setup_ns }))
    vim.cmd('qa!')
    return
  end
  local ticks = 0
  local timer = assert(vim.uv.new_timer())
  timer:start(1, 5, function()
    ticks = ticks + 1
  end)
  local pdf = directory .. '/a document\'s.pdf'
  adapter.viewer_command(pdf)
  wait(function()
    return #messages > 0
  end)
  timer:stop()
  timer:close()
  if case ~= 'defaults' then
    assert(ticks > 2, 'configuration blocked editor events')
  end
  if case == 'live' or case == 'stale' then
    assert(messages[1]:match(case == 'live' and 'live editor' or 'stale socket'), messages[1])
    assert(vim.api.nvim_get_current_line() == 'still editable')
    print(vim.json.encode({ case = case, setup_ns = setup_ns, ticks = ticks }))
    vim.cmd('qa!')
    return
  end
  local command = messages[1]
  assert(copied == command, 'viewer command was not sent to clipboard')
  local resolved = vim
    .system({ '/bin/sh', '-c', command .. ' --print-config' }, { text = true })
    :wait(10000)
  assert(resolved.code == 0, resolved.stderr)
  local config = vim.json.decode(resolved.stdout)
  if case == 'defaults' then
    assert(not config.nvim.focus_on_forward and not config.nvim.focus_on_inverse)
  end
  assert(config.editor.path:match('/n%x+%-editor.sock$'), command)
  local source = directory .. '/source-' .. vim.fn.getpid() .. '.tex'
  vim.fn.writefile({ 'first line', 'second line' }, source)
  local client = assert(vim.uv.new_pipe(false))
  client:connect(config.editor.path, function(error)
    assert(not error, error)
    client:write(vim.json.encode({ file = source, line = 2, byte_column = 3 }), function()
      client:shutdown(function()
        client:close()
      end)
    end)
  end)
  wait(function()
    return vim.api.nvim_buf_get_name(0) == source
  end)
  assert(vim.deep_equal(vim.api.nvim_win_get_cursor(0), { 2, 3 }), 'inverse-first pairing failed')
  adapter.forward_search(pdf, '{}', { kind = 'kitty', id = '123' })
  wait(function()
    return #messages > 1
  end)
  assert(messages[2]:find('PdfTermViewerCommand', 1, true), messages[2])
  assert(messages[2]:match('viewer unavailable'), messages[2])
  print(vim.json.encode({
    case = case,
    setup_ns = setup_ns,
    ticks = ticks,
    endpoint = config.editor.path,
  }))
  vim.cmd('qa!')
  return
end

local directory = assert(vim.uv.fs_mkdtemp(root .. '/.sessions-XXXXXX'))
-- Exercise the real zero-options default without requiring a release build in
-- the checkout. Only this disposable plugin root supplies the selected binary.
vim.fn.mkdir(directory .. '/plugin/target/release', 'p')
assert(vim.uv.fs_symlink(root .. '/lua', directory .. '/plugin/lua', { dir = true }))
assert(
  vim.uv.fs_symlink(vim.fn.fnamemodify(binary, ':p'), directory .. '/plugin/target/release/pdfterm')
)
vim.fn.writefile(
  { '#!/bin/sh', 'sleep .2', 'exec ' .. vim.fn.shellescape(binary) .. ' "$@"' },
  directory .. '/slow-viewer'
)
assert(vim.uv.fs_chmod(directory .. '/slow-viewer', 448))
local listener
local ok, failure = xpcall(function()
  local function spawn(name, callback)
    return vim.system(
      { vim.v.progpath, '--headless', '-u', 'NONE', '-l', root .. '/tests/nvim_sessions.lua' },
      {
        text = true,
        timeout = 15000,
        env = { PDFTERM_SESSION_CASE = name, XDG_CONFIG_HOME = directory },
      },
      callback
    )
  end
  local function run(name)
    local result
    spawn(name, function(value)
      result = value
    end)
    wait(function()
      return result ~= nil
    end)
    assert(result.code == 0, result.stderr)
    return vim.json.decode(vim.trim(result.stdout ~= '' and result.stdout or result.stderr))
  end
  local config_result = vim
    .system(
      { binary, '--session', 'shared', '--print-config' },
      { text = true, env = { XDG_CONFIG_HOME = directory } }
    )
    :wait(10000)
  assert(config_result.code == 0, config_result.stderr)
  local config = vim.json.decode(config_result.stdout)
  listener = require('pdfterm.socket').listen(config.editor.path, function()
    error('probe delivered a location')
  end)
  local inode = assert(vim.uv.fs_lstat(config.editor.path)).ino
  local live = run('live')
  assert(vim.uv.fs_lstat(config.editor.path).ino == inode, 'live endpoint was stolen')
  local missing = run('missing')
  run('invalid')
  local defaults = run('defaults')
  assert(not vim.uv.fs_lstat(defaults.endpoint), 'default setup leaked its inverse endpoint')
  local results = {}
  for index = 1, 2 do
    spawn(index == 1 and 'auto' or 'auto_tty', function(value)
      results[index] = value
    end)
  end
  wait(function()
    return results[1] and results[2]
  end)
  local endpoints = {}
  for _, result in ipairs(results) do
    assert(result.code == 0, result.stderr)
    local value = vim.json.decode(vim.trim(result.stdout ~= '' and result.stdout or result.stderr))
    assert(not endpoints[value.endpoint], 'automatic sessions collided')
    endpoints[value.endpoint] = true
    assert(not vim.uv.fs_lstat(value.endpoint), 'clean editor exit leaked its endpoint')
  end
  listener()
  listener = nil
  local stale
  spawn('make_stale', function(value)
    stale = value
  end)
  wait(function()
    return stale ~= nil
  end)
  assert(stale.signal == 9 and vim.uv.fs_lstat(config.editor.path), 'stale fixture missing')
  inode = vim.uv.fs_lstat(config.editor.path).ino
  run('stale')
  assert(vim.uv.fs_lstat(config.editor.path).ino == inode, 'diagnostic unlinked stale endpoint')
  print(
    'session regressions passed: nonblocking startup, missing executable, live/stale collisions, concurrent auto sessions, SSH inverse-first pairing, quoted command, cleanup; setup ns='
      .. live.setup_ns
      .. '/'
      .. missing.setup_ns
  )
end, debug.traceback)
if listener then
  listener()
end
vim.fn.delete(directory, 'rf')
if not ok then
  io.stderr:write(failure .. '\n')
  vim.cmd('cquit 1')
else
  vim.cmd('qa!')
end
