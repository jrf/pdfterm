-- Socket transport does not require terminal/window control.
local M = {}

function M.request(path, payload, callback, timeout, acknowledge)
  local host, port = path:match('^tcp://([^:]+):(%d+)$')
  local pipe
  if host then
    port = tonumber(port)
    if host ~= '127.0.0.1' or not port or port < 1 or port > 65535 then
      error('pdfterm: invalid loopback TCP endpoint')
    end
    pipe = assert(vim.uv.new_tcp())
  elseif path:match('^tcp://') then
    error('pdfterm: invalid loopback TCP endpoint')
  else
    pipe = assert(vim.uv.new_pipe(false))
  end
  local timer = assert(vim.uv.new_timer())
  local chunks, size, done = {}, 0, false
  local offered
  local function finish(error, connection_error, reply)
    if done then
      return
    end
    done = true
    timer:stop()
    timer:close()
    if not pipe:is_closing() then
      pipe:close()
    end
    vim.schedule(function()
      callback(error, connection_error, reply or offered)
    end)
  end
  local function response(data)
    local ok, reply = pcall(vim.json.decode, data)
    if not ok or type(reply) ~= 'table' or reply.ok ~= true then
      finish(
        ok and type(reply) == 'table' and reply.error or 'socket request rejected or invalid reply'
      )
    elseif acknowledge and (type(reply.id) ~= 'string' or reply.id == '') then
      finish('launch reply has no window ID')
    elseif acknowledge and not offered then
      if reply.confirmed then
        finish('launch confirmation arrived before offer')
        return
      end
      offered = reply
      -- Keep the deadline active while waiting for the editor event loop.
      -- Success is delivered only after the bridge confirms the receipt.
      vim.schedule(function()
        if done then
          return
        end
        local sent, write_error = pipe:write('\006', function(error)
          if error then
            finish('launch ownership receipt failed: ' .. error)
          end
        end)
        if not sent then
          finish('launch ownership receipt failed: ' .. write_error)
        end
      end)
    elseif acknowledge and (not reply.confirmed or reply.id ~= offered.id) then
      finish('invalid launch ownership confirmation')
    else
      finish(nil, nil, reply)
    end
  end
  timer:start(timeout or 31000, 0, function()
    finish('socket connection/reply timed out')
  end)
  local connect = function(callback)
    if host then
      pipe:connect(host, port, callback)
    else
      pipe:connect(path, callback)
    end
  end
  connect(function(error)
    if done then
      return
    end
    if error then
      finish(error, true)
      return
    end
    pipe:read_start(function(read_error, chunk)
      if done then
        return
      end
      if read_error then
        finish(read_error)
      elseif chunk then
        size = size + #chunk
        if size > 4096 then
          finish('socket reply exceeds 4096 bytes')
        else
          chunks[#chunks + 1] = chunk
          if acknowledge then
            local data = table.concat(chunks)
            while not done do
              local line, rest = data:match('^(.-)\n(.*)$')
              if not line then
                break
              end
              response(line)
              data = rest
            end
            chunks = { data }
          end
        end
      else
        if acknowledge then
          finish('launcher closed before ownership confirmation')
        else
          response(table.concat(chunks))
        end
      end
    end)
    pipe:write(payload .. (acknowledge and '\n' or ''), function(write_error)
      if done then
        return
      end
      if write_error then
        finish(write_error)
        return
      end
      if not acknowledge then
        pipe:shutdown(function(shutdown_error)
          if shutdown_error then
            finish(shutdown_error)
          end
        end)
      end
    end)
  end)
  return function()
    finish('socket request cancelled')
  end
end

-- Diagnose only: never unlink an endpoint another editor may still own.
function M.diagnose(path, callback)
  local identity = vim.uv.fs_lstat(path)
  if not identity or identity.type ~= 'socket' or identity.uid ~= vim.uv.getuid() then
    callback('endpoint is absent or is not a current-user-owned socket')
    return
  end
  local pipe = assert(vim.uv.new_pipe(false))
  local timer = assert(vim.uv.new_timer())
  local done = false
  local function finish(status)
    if done then
      return
    end
    done = true
    timer:stop()
    timer:close()
    pipe:close()
    vim.schedule(function()
      callback(status)
    end)
  end
  timer:start(200, 0, function()
    finish('listener probe timed out; endpoint left untouched')
  end)
  pipe:connect(path, function(error)
    if not error then
      finish('a live editor owns this session; choose a different session')
    elseif error:match('ECONNREFUSED') then
      finish(
        'stale socket (connection refused); remove it only after stopping editors using this session'
      )
    else
      finish('listener probe failed: ' .. error .. '; endpoint left untouched')
    end
  end)
end

function M.listen(path, on_location)
  local parent = assert(vim.uv.fs_lstat(vim.fs.dirname(path)))
  assert(
    parent.type == 'directory' and parent.uid == vim.uv.getuid() and bit.band(parent.mode, 63) == 0,
    'pdfterm: socket parent must be a current-user-owned mode-0700 directory'
  )
  local server = assert(vim.uv.new_pipe(false))
  local ok, bind_error = server:bind(path)
  if not ok then
    server:close()
    error('pdfterm: cannot bind ' .. path .. ': ' .. tostring(bind_error))
  end
  local identity, stat_error = vim.uv.fs_lstat(path)
  if not identity then
    server:close()
    error('pdfterm: cannot identify socket: ' .. tostring(stat_error))
  end
  local clients, count = {}, 0
  local function close()
    for finish in pairs(clients) do
      finish('editor stopped')
    end
    if not server:is_closing() then
      server:close()
    end
    local current = vim.uv.fs_lstat(path)
    if current and current.dev == identity.dev and current.ino == identity.ino then
      assert(vim.uv.fs_unlink(path))
    end
  end
  local secured, chmod_error = vim.uv.fs_chmod(path, 384)
  if not secured then
    close()
    error('pdfterm: cannot secure socket: ' .. tostring(chmod_error))
  end
  local listening, listen_failure = server:listen(16, function(listen_error)
    if listen_error then
      vim.schedule(function()
        vim.notify('pdfterm: ' .. listen_error, vim.log.levels.ERROR)
      end)
      return
    end
    local client = assert(vim.uv.new_pipe(false))
    server:accept(client)
    if count >= 16 then
      client:close()
      return
    end
    count = count + 1
    local timer = assert(vim.uv.new_timer())
    local chunks, size = {}, 0
    local finish
    finish = function(message, deliver)
      if client:is_closing() then
        return
      end
      clients[finish] = nil
      count = count - 1
      timer:stop()
      timer:close()
      client:close()
      vim.schedule(function()
        if message then
          vim.notify('pdfterm inverse search: ' .. message, vim.log.levels.ERROR)
        elseif deliver and size > 0 then
          local decoded, location = pcall(vim.json.decode, table.concat(chunks))
          if
            not decoded
            or type(location) ~= 'table'
            or type(location.file) ~= 'string'
            or location.file:sub(1, 1) ~= '/'
            or location.file:find('%z')
            or type(location.line) ~= 'number'
            or location.line < 1
            or location.line % 1 ~= 0
            or type(location.byte_column) ~= 'number'
            or location.byte_column < 0
            or location.byte_column % 1 ~= 0
          then
            vim.notify('pdfterm: invalid inverse-search JSON location', vim.log.levels.ERROR)
          else
            on_location(location)
          end
        end
      end)
    end
    clients[finish] = true
    timer:start(1000, 0, function()
      finish('request timed out')
    end)
    client:read_start(function(read_error, chunk)
      if read_error then
        finish(read_error)
      elseif not chunk then
        finish(nil, true)
      else
        size = size + #chunk
        if size > 16384 then
          finish('request exceeds 16384 bytes')
        else
          chunks[#chunks + 1] = chunk
        end
      end
    end)
  end)
  if not listening then
    close()
    error('pdfterm: cannot listen: ' .. tostring(listen_failure))
  end
  return close
end
return M
