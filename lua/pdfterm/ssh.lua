-- The client-side pdfterm-ssh wrapper owns windows; this side owns only requests.
local socket = require('pdfterm.socket')
local M = {}

local function launch_token()
  local path = vim.env.PDFTERM_LAUNCH_TOKEN_FILE
  if not path or path == '' then
    return nil, 'pdfterm: missing PDFTERM_LAUNCH_TOKEN_FILE'
  end
  local ok, lines = pcall(vim.fn.readfile, path)
  if not ok then
    return nil, 'pdfterm: cannot read launch token file: ' .. tostring(lines)
  end
  local token = lines[1]
  if #lines ~= 1 or not token or #token ~= 64 or not token:match('^%x+$') then
    return nil, 'pdfterm: invalid launch token file'
  end
  return token
end

local function request(payload, callback, timeout)
  local token, problem = launch_token()
  if problem then
    local result = { code = 1, stdout = '', stderr = problem }
    callback(result)
    return { wait = function() return result end }
  end
  payload.token = token
  local done, result = false, nil
  local callback_failure
  local cancel = socket.request(
    vim.env.PDFTERM_LAUNCH_SOCKET,
    vim.json.encode(payload),
    function(transport_error, _, reply)
      local function complete(problem)
        result =
          { code = problem and 1 or 0, stdout = reply and reply.id or '', stderr = problem or '' }
        local ok, failure = xpcall(function()
          callback(result)
        end, debug.traceback)
        if not ok then
          if payload.action == 'launch' and reply and reply.id then
            local cleaned, closed = pcall(function()
              return request({ action = 'close', id = reply.id }, function() end, 2000):wait(2500)
            end)
            if not cleaned or closed.code ~= 0 then
              failure = failure .. '; cleanup: ' .. (cleaned and closed.stderr or tostring(closed))
            end
          end
          callback_failure = failure
          result = { code = 1, stdout = '', stderr = failure }
          done = true
          error(failure)
        end
        done = true
      end
      if transport_error and payload.action == 'launch' and reply and reply.id then
        -- Confirmation may be lost after the bridge accepted the receipt.
        -- Close the exact offered handle before reporting failed ownership.
        request({ action = 'close', id = reply.id }, function(closed)
          complete(transport_error .. (closed.code ~= 0 and ('; cleanup: ' .. closed.stderr) or ''))
        end, 2000)
      else
        complete(transport_error)
      end
    end,
    timeout or 6000,
    payload.action == 'launch'
  )
  return {
    wait = function(_, timeout)
      if not vim.wait(timeout or 8500, function()
        return done
      end, 10) then
        cancel()
        error('pdfterm: client terminal control timed out')
      end
      if callback_failure then
        error(callback_failure)
      end
      return result
    end,
  }
end

function M.capture(callback)
  local _, problem = launch_token()
  if problem then
    callback(problem)
  else
    callback(nil, 'source')
  end
end

function M.launch(_, argv, callback)
  return request(
    { action = 'launch', argv = argv, path = vim.env.PATH, config_home = vim.env.XDG_CONFIG_HOME },
    callback
  )
end

function M.focus(source, callback)
  return request({ action = 'focus', id = source.id }, callback)
end

function M.close(split)
  local result = request({ action = 'close', id = split.id }, function() end):wait()
  if result.code ~= 0 then
    error('pdfterm: could not close client viewer: ' .. result.stderr)
  end
end

return M
