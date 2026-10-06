-- WezTerm pane IDs are scoped to their GUI socket, not the currently focused window.
local M = {}

local function decode(id)
  local ok, handle = pcall(vim.json.decode, id)
  if not ok or type(handle) ~= 'table' or type(handle.socket) ~= 'string' or handle.socket == ''
    or type(handle.pane) ~= 'string' or not handle.pane:match('^%d+$')
  then
    error('pdfterm: invalid WezTerm pane handle')
  end
  return handle
end

local function encode(socket, pane)
  return vim.json.encode({ socket = socket, pane = pane })
end

local function remote(socket, arguments, callback)
  local command = { 'wezterm', 'cli', '--no-auto-start' }
  vim.list_extend(command, arguments)
  return vim.system(command, {
    text = true,
    timeout = 2000,
    env = { WEZTERM_UNIX_SOCKET = socket },
  }, callback)
end

local function panes(result)
  if result.code ~= 0 then
    error('pdfterm: could not list WezTerm panes: ' .. (result.stderr or ''))
  end
  local ok, rows = pcall(vim.json.decode, result.stdout or '')
  if not ok or type(rows) ~= 'table' or not vim.islist(rows) then
    error('pdfterm: invalid WezTerm pane listing')
  end
  local found = {}
  for _, row in ipairs(rows) do
    if type(row) ~= 'table' or type(row.pane_id) ~= 'number'
      or row.pane_id < 0 or row.pane_id % 1 ~= 0
    then
      error('pdfterm: invalid WezTerm pane listing')
    end
    found[tostring(row.pane_id)] = row
  end
  return found
end

local function report(callback, problem)
  callback({ code = 1, stdout = '', stderr = problem })
end

function M.capture(callback)
  local socket, pane = vim.env.WEZTERM_UNIX_SOCKET, vim.env.WEZTERM_PANE
  if not socket or socket == '' or not pane or not pane:match('^%d+$') then
    callback('WezTerm control requires WEZTERM_UNIX_SOCKET and WEZTERM_PANE from the source terminal')
    return
  end
  return remote(socket, { 'list', '--format', 'json' }, vim.schedule_wrap(function(result)
    local ok, found = pcall(panes, result)
    if not ok then
      callback(tostring(found))
    elseif not found[pane] then
      callback('WezTerm source pane ' .. pane .. ' is not in its captured GUI socket')
    else
      callback(nil, encode(socket, pane))
    end
  end))
end

function M.launch(source, argv, callback)
  local handle = decode(source.id)
  return remote(handle.socket, { 'list', '--format', 'json' }, vim.schedule_wrap(function(result)
    local ok, found = pcall(panes, result)
    if not ok then
      report(callback, tostring(found))
      return
    end
    if not found[handle.pane] then
      report(callback, 'pdfterm: WezTerm source pane has exited')
      return
    end
    local arguments = {
      'split-pane', '--pane-id', handle.pane, '--right', '--cwd', vim.fn.getcwd(), '--',
      'env', 'PATH=' .. vim.env.PATH, 'XDG_CONFIG_HOME=' .. (vim.env.XDG_CONFIG_HOME or ''),
    }
    vim.list_extend(arguments, argv)
    remote(handle.socket, arguments, vim.schedule_wrap(function(split)
      if split.code ~= 0 then
        callback(split)
        return
      end
      local viewer = vim.trim(split.stdout or '')
      if not viewer:match('^%d+$') then
        report(callback, 'pdfterm: WezTerm split returned an invalid pane ID')
        return
      end
      remote(handle.socket, { 'activate-pane', '--pane-id', handle.pane }, vim.schedule_wrap(function(focus)
        if focus.code ~= 0 then
          -- A successful split is provisional until the editor receives its handle.
          remote(handle.socket, { 'kill-pane', '--pane-id', viewer }, vim.schedule_wrap(function(rollback)
            local problem = 'pdfterm: could not refocus WezTerm source: ' .. (focus.stderr or '')
            if rollback.code ~= 0 then
              -- Keep the exact handle for VimLeavePre if the provisional pane survived.
              callback({
                code = 1,
                stdout = encode(handle.socket, viewer),
                stderr = problem .. '; could not close unowned pane: ' .. (rollback.stderr or ''),
                unclosed = true,
              })
            else
              report(callback, problem)
            end
          end))
          return
        end
        split.stdout = encode(handle.socket, viewer)
        local accepted, failure = xpcall(callback, debug.traceback, split)
        if not accepted then
          local closed, close_error = pcall(M.close, { id = split.stdout })
          if not closed then
            error(failure .. '; could not close unowned WezTerm pane: ' .. tostring(close_error))
          end
          error(failure)
        end
      end))
    end))
  end))
end

function M.focus(source, callback)
  local handle = decode(source.id)
  return remote(handle.socket, { 'activate-pane', '--pane-id', handle.pane }, callback)
end

function M.close(split)
  local handle = decode(split.id)
  local found = panes(remote(handle.socket, { 'list', '--format', 'json' }):wait())
  if not found[handle.pane] then
    return
  end
  local result = remote(handle.socket, {
    'send-text', '--no-paste', '--pane-id', handle.pane, '\003',
  }):wait()
  if result.code ~= 0 then
    error('pdfterm: could not quit owned WezTerm pane: ' .. (result.stderr or ''))
  end
end

return M
