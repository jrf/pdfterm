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

local function absolute_path(path)
  return type(path) == 'string' and path:sub(1, 1) == '/' and not path:find('%z')
end

local function document_context(location)
  local warning = location.warning
  return warning == nil or warning == vim.NIL
    or type(warning) == 'string' and #warning <= 4096 and not warning:find('%z')
end

function M.listen(path, on_location)
  local parent = assert(vim.uv.fs_lstat(vim.fs.dirname(path)))
  assert(
    parent.type == 'directory' and parent.uid == vim.uv.getuid() and bit.band(parent.mode, 63) == 0,
    'pdfterm: socket parent must be a current-user-owned mode-0700 directory'
  )
  assert(absolute_path(path) and #path <= 103, 'pdfterm: socket path must be absolute and at most 103 bytes')
  local directory = vim.fs.dirname(path)
  local random = assert(vim.uv.random(8)):gsub('.', function(byte)
    return string.format('%02x', byte:byte())
  end)
  local endpoint = directory .. '/' .. random:sub(1, 102 - #directory)
  -- A 101-byte parent leaves one hex digit, which may equal a 1-byte public name.
  if endpoint == path then
    endpoint = directory .. '/' .. (endpoint:sub(-1) == '0' and '1' or '0')
  end
  assert(endpoint ~= path, 'pdfterm: private socket name collides with public path')
  local server = assert(vim.uv.new_pipe(false))
  -- libuv unlinks its bound name on close; never bind the replaceable public name.
  local ok, bind_error = server:bind(endpoint)
  if not ok then
    server:close()
    error('pdfterm: cannot bind private socket: ' .. tostring(bind_error))
  end
  local identity, stat_error = vim.uv.fs_lstat(endpoint)
  if not identity then
    server:close()
    error('pdfterm: cannot identify socket: ' .. tostring(stat_error))
  end
  local clients, count, closed = {}, 0, false
  local function close()
    if closed then return end
    closed = true
    for finish in pairs(clients) do
      finish('editor stopped')
    end
    local current = vim.uv.fs_lstat(path)
    local removed, remove_error = true, nil
    if current and current.dev == identity.dev and current.ino == identity.ino then
      removed, remove_error = vim.uv.fs_unlink(path)
    end
    if not server:is_closing() then
      server:close()
    end
    assert(removed, remove_error)
  end
  local secured, chmod_error = vim.uv.fs_chmod(endpoint, 384)
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
    if not server:accept(client) or count >= 16 then
      client:close()
      return
    end
    count = count + 1
    local timer = assert(vim.uv.new_timer())
    local chunks, size, done = {}, 0, false
    local function finish(message)
      if done then return end
      done = true
      clients[finish] = nil
      count = count - 1
      timer:stop()
      timer:close()
      if not client:is_closing() then client:close() end
      if message and not (closed and message == 'editor stopped') then
        vim.schedule(function()
          vim.notify('pdfterm inverse search: ' .. message, vim.log.levels.ERROR)
        end)
      end
    end
    clients[finish] = true
    timer:start(1000, 0, function()
      finish('request timed out')
    end)
    client:read_start(function(read_error, chunk)
      if done then return end
      if read_error then
        finish(read_error)
      elseif not chunk then
        -- Complete input must survive a busy main loop without a read timeout.
        timer:stop()
        client:read_stop()
        vim.schedule(function()
          if done or closed then return end
          if size == 0 then finish(); return end
          local decoded, location = pcall(vim.json.decode, table.concat(chunks))
          if not decoded or type(location) ~= 'table' then
            finish('invalid inverse-search JSON request')
            return
          end
          if location.action ~= nil then
            finish('unsupported editor action')
            return
          end
          finish()
          if
            not absolute_path(location.file)
            or type(location.line) ~= 'number'
            or location.line < 1
            or location.line % 1 ~= 0
            or type(location.byte_column) ~= 'number'
            or location.byte_column < 0
            or location.byte_column % 1 ~= 0
            or not document_context(location)
          then
            vim.notify('pdfterm: invalid inverse-search JSON location', vim.log.levels.ERROR)
          else
            local call_ok, call_error = pcall(on_location, location)
            if not call_ok then
              vim.notify('pdfterm: ' .. tostring(call_error), vim.log.levels.ERROR)
            end
          end
        end)
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
  local published, publish_error = vim.uv.fs_link(endpoint, path)
  if not published then
    close()
    error('pdfterm: cannot publish socket ' .. path .. ': ' .. tostring(publish_error))
  end
  return close
end
return M
