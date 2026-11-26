-- Run: nvim --headless -u NONE -l tests/nvim_socket.lua
local root, uv = vim.fn.getcwd(), vim.uv
vim.opt.runtimepath:prepend(root)
local socket = require('pdfterm.socket')
local directory = assert(uv.fs_mkdtemp(root .. '/.nvim-socket-XXXXXX'))
local closers = {}
local function listen(path)
  local close = socket.listen(path, function() end)
  closers[#closers + 1] = close
  return close
end
local function entries(path)
  local names, scan = {}, assert(uv.fs_scandir(path))
  while true do
    local name = uv.fs_scandir_next(scan)
    if not name then return names end
    names[#names + 1] = name
  end
end
local function connect(path)
  local client, done, failure = assert(uv.new_pipe(false)), false, nil
  client:connect(path, function(error)
    failure, done = error, true
    client:close()
  end)
  assert(vim.wait(1000, function() return done end, 5), 'connection timed out')
  assert(not failure, failure)
end
local original_random = uv.random
local ok, failure = xpcall(function()
  local path, moved = directory .. '/editor.sock', directory .. '/moved.sock'
  -- libuv must never unlink a replacement at the published path.
  local close = listen(path)
  assert(bit.band(assert(uv.fs_lstat(path)).mode, 511) == 384)
  assert(uv.fs_rename(path, moved))
  vim.fn.writefile({ 'replacement file' }, path)
  close()
  close()
  assert(vim.fn.readfile(path)[1] == 'replacement file', 'close removed replacement file')
  assert(#entries(directory) == 2, 'close retained private endpoint')
  assert(uv.fs_unlink(path))
  assert(uv.fs_unlink(moved))

  close = listen(path)
  assert(uv.fs_rename(path, moved))
  local replacement = assert(uv.new_pipe(false))
  assert(replacement:bind(path))
  closers[#closers + 1] = function()
    if not replacement:is_closing() then replacement:close() end
  end
  local accepted = 0
  assert(replacement:listen(4, function(error)
    assert(not error, error)
    local client = assert(uv.new_pipe(false))
    assert(replacement:accept(client))
    accepted = accepted + 1
    client:close()
  end))
  close()
  assert(not pcall(socket.listen, path, function() end), 'occupied listener was overwritten')
  assert(#entries(directory) == 2, 'failed publication retained a private endpoint')
  close()
  connect(path)
  assert(vim.wait(1000, function() return accepted == 1 end, 5), 'replacement listener was lost')
  replacement:close()
  assert(uv.fs_unlink(moved))
  assert(#entries(directory) == 0, 'close retained private endpoint')

  -- Occupied public names and colliding private names are never overwritten.
  vim.fn.writefile({ 'occupied' }, path)
  assert(not pcall(socket.listen, path, function() end))
  assert(vim.fn.readfile(path)[1] == 'occupied')
  assert(#entries(directory) == 1, 'failed publication retained private endpoint')
  assert(uv.fs_unlink(path))
  uv.random = function() return string.rep('\0', 8) end
  local collision = directory .. '/' .. string.rep('0', math.min(16, 102 - #directory))
  vim.fn.writefile({ 'private collision' }, collision)
  assert(not pcall(socket.listen, path, function() end))
  assert(vim.fn.readfile(collision)[1] == 'private collision')
  assert(not uv.fs_lstat(path))
  assert(uv.fs_unlink(collision))
  uv.random = original_random

  -- Internal publication must not reduce the supported public pathname length.
  local parent = directory .. '/' .. string.rep('d', 100 - #directory)
  assert(#parent == 101)
  assert(uv.fs_mkdir(parent, 448))
  local longest = parent .. '/z'
  assert(#longest == 103)
  close = listen(longest)
  connect(longest)
  close()
  close()
  assert(#entries(parent) == 0, 'normal close retained socket artifacts')
  -- A one-hex-digit private name must not collide with a one-byte public name.
  for byte, name in pairs({ ['\0'] = '0', ['\16'] = '1' }) do
    uv.random = function() return string.rep(byte, 8) end
    local single = parent .. '/' .. name
    close = listen(single)
    uv.random = original_random
    connect(single)
    assert(#entries(parent) == 2, 'private endpoint reused the public name')
    close()
    assert(#entries(parent) == 0, 'single-byte close retained socket artifacts')
  end
  assert(uv.fs_rmdir(parent))
  assert(#entries(directory) == 0)
end, debug.traceback)
uv.random = original_random
for _, close in ipairs(closers) do pcall(close) end
vim.fn.delete(directory, 'rf')
if not ok then error(failure) end
print('socket lifecycle regressions passed')
