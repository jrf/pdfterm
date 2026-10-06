-- Run: nvim --headless -u NONE -l tests/nvim_ghostty.lua
local root = vim.fn.getcwd()
vim.opt.runtimepath:prepend(root)
local case, directory = vim.env.PDFTERM_GHOSTTY_CASE, vim.env.PDFTERM_GHOSTTY_DIRECTORY
local function wait(predicate)
  assert(vim.wait(6000, predicate, 2), 'Ghostty helper operation timed out')
end
if case then
  local system = vim.system
  local uname = vim.uv.os_uname()
  uname.sysname = 'Darwin' -- The fake worker exercises pipe/lifetime logic on Linux too.
  vim.uv.os_uname = function()
    return uname
  end
  local child, spawns = nil, 0
  vim.system = function(command, options, callback)
    assert(command[1] == 'osascript')
    spawns = spawns + 1
    child = system({ 'python3', directory .. '/worker.py', case }, options, callback)
    return child
  end
  local control = require('pdfterm.ghostty')
  local completed, results = 0, {}
  local function request(index)
    local action = index % 3 == 0 and 'focus' or 'capture'
    control.request(action, 'surface', function(result)
      assert(not results[index], 'request completed twice')
      results[index], completed = result, completed + 1
      if case == 'success' then
        assert(result.code == 0, result.stderr)
        assert(result.stdout == (action == 'capture' and 'surface-' .. index or ''))
      end
    end)
  end
  if case == 'success' then
    -- Seeded fragmentation, coalescing and reversed pairs exercise reply routing.
    for batch = 0, 63 do
      for index = batch * 16 + 1, batch * 16 + 16 do
        request(index)
      end
      wait(function()
        return completed == (batch + 1) * 16
      end)
    end
    control.close()
  elseif case == 'exit' then
    request(1)
    wait(function()
      return vim.uv.fs_stat(directory .. '/pid') ~= nil
    end)
    vim.cmd('qa!') -- VimLeavePre must kill a worker blocked with a pending request.
  else
    local total = case == 'hang' and 33 or 2
    for index = 1, total do
      request(index)
    end
    wait(function()
      return completed == total
    end)
    local expected = ({ hang = 'timed out', crash = 'exited', malformed = 'Invalid', overflow = 'exceeds' })[case]
    for index = 1, total do
      assert(results[index].code ~= 0)
      assert(results[index].stderr:find(index == 33 and 'Too many' or expected, 1, true), results[index].stderr)
    end
    request(total + 1)
    assert(completed == total + 1 and results[total + 1].code ~= 0, 'dead worker silently restarted')
    control.close()
  end
  assert(spawns == 1)
  assert(child:wait(1000).code ~= nil)
  assert(not vim.uv.kill(child.pid, 0), 'owned worker survived cleanup')
  return
end

directory = vim.fn.tempname()
vim.fn.mkdir(directory, 'p')
local worker = [[
import json, os, random, sys, time
mode = sys.argv[1]
with open(os.path.join(os.path.dirname(__file__), 'pid'), 'w') as f:
    f.write(str(os.getpid()))
random.seed(314159)
if mode in ('hang', 'exit'):
    time.sleep(30)
elif mode == 'crash':
    sys.stderr.write('intentional worker failure\n')
    sys.exit(7)
elif mode == 'malformed':
    print('{"seq":9999,"ok":true}', flush=True)
    time.sleep(30)
elif mode == 'overflow':
    print('x' * 4097, flush=True)
    time.sleep(30)
else:
    while True:
        lines = [sys.stdin.readline(), sys.stdin.readline()]
        if not all(lines):
            break
        replies = []
        for line in reversed(lines):
            request = json.loads(line)
            reply = {'seq': request['seq'], 'ok': True}
            if request['action'] == 'capture':
                reply['id'] = 'surface-' + str(request['seq'])
            replies.append(json.dumps(reply) + '\n')
        data = ''.join(replies).encode()
        while data:
            size = random.randint(1, len(data))
            os.write(1, data[:size])
            data = data[size:]
]]
vim.fn.writefile(vim.split(worker, '\n'), directory .. '/worker.py')
local ok, error = pcall(function()
  for _, name in ipairs({ 'success', 'hang', 'crash', 'malformed', 'overflow', 'exit' }) do
    vim.fn.delete(directory .. '/pid')
    local result = vim
      .system({ vim.v.progpath, '--headless', '-u', 'NONE', '-l', root .. '/tests/nvim_ghostty.lua' }, {
        text = true,
        timeout = 10000,
        env = { PDFTERM_GHOSTTY_CASE = name, PDFTERM_GHOSTTY_DIRECTORY = directory },
      })
      :wait()
    assert(result.code == 0 and result.stderr == '', name .. ': ' .. result.stderr)
    local pid = tonumber(vim.fn.readfile(directory .. '/pid')[1])
    assert(not vim.uv.kill(pid, 0), name .. ': worker survived editor exit')
  end
end)
vim.fn.delete(directory, 'rf')
assert(ok, error)
print(
  'Ghostty helper regressions passed: 1024 routed requests, timeout, crash, malformed/oversized replies, editor exit'
)
