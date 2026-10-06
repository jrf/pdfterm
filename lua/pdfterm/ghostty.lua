-- Reuse the interpreter, never the focused terminal identity.
local M = {}
local root = vim.fn.fnamemodify(debug.getinfo(1, 'S').source:sub(2), ':h:h:h')
local process, failure
local pending, sequence, count = {}, 0, 0
local output, stderr = '', ''

local function finish(seq, result)
  local request = pending[seq]
  pending[seq], count = nil, count - 1
  request.timer:stop()
  request.timer:close()
  request.callback(result)
end

local function fail(message)
  if failure then
    return
  end
  failure = message
  if process then
    process:kill(9)
  end
  local requests = pending
  pending, count = {}, 0
  for _, request in pairs(requests) do
    request.timer:stop()
    request.timer:close()
    request.callback({ code = 1, stdout = '', stderr = message })
  end
end

function M.close()
  fail('Ghostty control stopped')
  if process then
    process:wait(1000)
  end
end

local function receive(error, data)
  if failure then
    return
  end
  if error then
    fail('Ghostty control output: ' .. error)
    return
  end
  if not data then
    return
  end
  output = output .. data
  while not failure do
    local newline = output:find('\n', 1, true)
    if not newline then
      if #output > 4096 then
        fail('Ghostty control output exceeds 4096 bytes')
      end
      return
    end
    if newline > 4097 then
      fail('Ghostty control output exceeds 4096 bytes')
      return
    end
    local ok, reply = pcall(vim.json.decode, output:sub(1, newline - 1))
    output = output:sub(newline + 1)
    local request = ok and type(reply) == 'table' and pending[reply.seq]
    if
      not request
      or type(reply.ok) ~= 'boolean'
      or (reply.ok and request.action == 'capture' and (type(reply.id) ~= 'string' or reply.id == ''))
      or (not reply.ok and type(reply.error) ~= 'string')
    then
      fail('Invalid Ghostty control reply')
      return
    end
    finish(reply.seq, {
      code = reply.ok and 0 or 1,
      stdout = reply.ok and reply.id or '',
      stderr = reply.ok and '' or reply.error,
    })
  end
end

function M.request(action, id, callback)
  if failure then
    callback({ code = 1, stdout = '', stderr = failure })
    return
  end
  if vim.uv.os_uname().sysname ~= 'Darwin' then
    callback({ code = 1, stdout = '', stderr = 'Ghostty control requires macOS' })
    return
  end
  if not process then
    local ok, result = pcall(
      vim.system,
      { 'osascript', '-l', 'JavaScript', root .. '/scripts/ghostty-control.js' },
      {
        stdin = true,
        text = true,
        stdout = vim.schedule_wrap(receive),
        stderr = vim.schedule_wrap(function(error, data)
          if failure then
            return
          end
          if error then
            fail('Ghostty control stderr: ' .. error)
            return
          end
          stderr = stderr .. (data or '')
          if #stderr > 4096 then
            fail('Ghostty control stderr exceeds 4096 bytes')
          end
        end),
      },
      vim.schedule_wrap(function(result)
        fail('Ghostty control exited (' .. result.code .. '): ' .. stderr)
      end)
    )
    if not ok then
      fail('Could not start Ghostty control: ' .. tostring(result))
      callback({ code = 1, stdout = '', stderr = failure })
      return
    end
    process = result
    vim.api.nvim_create_autocmd('VimLeavePre', { once = true, callback = M.close })
  end
  if count >= 32 then
    callback({ code = 1, stdout = '', stderr = 'Too many pending Ghostty control requests' })
    return
  end
  sequence, count = sequence + 1, count + 1
  local seq = sequence
  local timer = assert(vim.uv.new_timer())
  pending[sequence] = { action = action, callback = callback, timer = timer }
  -- The worker is serial: a timed-out AppleEvent could still focus a terminal.
  -- Kill it and fail every pending request rather than allow a late side effect.
  timer:start(
    3000,
    0,
    vim.schedule_wrap(function()
      if pending[seq] then
        fail('Ghostty control request timed out; restart Neovim to retry')
      end
    end)
  )
  local ok, error = pcall(process.write, process, vim.json.encode({ seq = sequence, action = action, id = id }) .. '\n')
  if not ok then
    fail('Ghostty control input: ' .. tostring(error))
  end
end

return M
