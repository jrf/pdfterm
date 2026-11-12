-- Terminal control, not graphics: Kitty, Ghostty, and WezTerm render Kitty protocol.
-- Handles identify exact surfaces; closing a handle gracefully quits its reader.
local platform = require('pdfterm.platform')
local M = {
  root = vim.fn.fnamemodify(debug.getinfo(1, 'S').source:sub(2), ':h:h:h'),
}
package.loaded['pdfterm.terminal'] = M

function M.shell_join(argv)
  return table.concat(vim.tbl_map(function(arg)
    return "'" .. tostring(arg):gsub("'", "'\\''") .. "'"
  end, argv), ' ')
end

function M.rollback_result(id, problem, closed, close_error, label)
  local result = { code = 1, stdout = closed and '' or id, stderr = problem }
  if not closed then
    result.unclosed = true
    result.stderr = problem .. '; could not close unowned ' .. label .. ': ' .. tostring(close_error)
  end
  return result
end

function M.finish_launch(result, callback, close_fn, label)
  local ok, failure = xpcall(callback, debug.traceback, result)
  if ok then return end
  local id = vim.trim(result.stdout or '')
  if (result.code == 0 or result.unclosed) and id ~= '' then
    local closed, close_error = pcall(close_fn, { id = id })
    if not closed then
      result.code, result.unclosed = 1, true
      result.stderr = failure .. '; could not close unowned ' .. label .. ': ' .. tostring(close_error)
      error(result.stderr)
    end
    result.unclosed = nil
  end
  error(failure)
end

local ghostty_control = require('pdfterm.ghostty')
local ghostty, neovide = {}, {}
local adapters = {
  kitty = require('pdfterm.kitty'),
  ghostty = ghostty,
  wezterm = require('pdfterm.wezterm'),
  ssh = require('pdfterm.ssh'),
  neovide = neovide,
}

local split_script = [[
on run argv
  tell application "Ghostty"
    set sourceTerminal to terminal id (item 2 of argv)
    set cfg to new surface configuration
    set command of cfg to item 1 of argv
    set wait after command of cfg to false
    set environment variables of cfg to {"PATH=" & (item 3 of argv), "XDG_CONFIG_HOME=" & (item 4 of argv)}
    set viewer to split sourceTerminal direction right with configuration cfg
    return id of viewer
  end tell
end run
]]

local function finish_ghostty_launch(result, callback)
  M.finish_launch(result, callback, ghostty.close, 'Ghostty surface')
end

-- Shared with scripts/pdfterm-ssh, which wraps the same body in the Ghostty tell block.
local close_script_path = M.root .. '/scripts/ghostty-close.applescript'
local close_script

local function ensure_close_script()
  if not close_script then
    local file = assert(io.open(close_script_path, 'r'))
    close_script = 'on run argv\ntell application "Ghostty"\n' .. file:read('*a') .. 'end tell\nend run\n'
    file:close()
  end
  return close_script
end

function ghostty.launch(source, argv, callback)
  ensure_close_script()
  return platform.applescript(
    split_script,
    { M.shell_join(argv), source.id, vim.env.PATH, vim.env.XDG_CONFIG_HOME or '' },
    vim.schedule_wrap(function(split)
      if split.code ~= 0 then
        finish_ghostty_launch(split, callback)
        return
      end
      -- Acquire the exact child before any fallible post-create operation.
      local id = vim.trim(split.stdout or '')
      if id == '' or id == source.id then
        finish_ghostty_launch({
          code = 1,
          stderr = 'Ghostty split returned no distinct terminal ID; created surface ownership is unknown',
        }, callback)
        return
      end
      local function refocused(focus)
        if focus.code == 0 then
          finish_ghostty_launch(split, callback)
          return
        end
        local closed, close_error = pcall(ghostty.close, { id = id })
        finish_ghostty_launch(M.rollback_result(
          id,
          'pdfterm: could not refocus Ghostty source: ' .. (focus.stderr or ''),
          closed,
          close_error,
          'Ghostty surface'
        ), callback)
      end
      local focused, focus_error = pcall(ghostty.focus, source, vim.schedule_wrap(refocused))
      if not focused then
        refocused({ code = 1, stderr = tostring(focus_error) })
      end
    end)
  )
end

function ghostty.focus(source, callback)
  return ghostty_control.request('focus', source.id, callback)
end

function ghostty.close(split)
  local result = platform.applescript(ensure_close_script(), { split.id }):wait()
  if result.code ~= 0 then
    error('pdfterm: could not close owned PDF split: ' .. (result.stderr or ''))
  end
end

-- Neovide cannot render Kitty graphics in :terminal. Give its viewer a real
-- Ghostty surface, without treating an inherited terminal ID as the GUI.
function neovide.capture(callback)
  callback(nil, tostring(vim.fn.getpid()))
end

function neovide.launch(_, argv, callback)
  ensure_close_script()
  return platform.applescript(
    [[
on run argv
  tell application "Ghostty"
    set cfg to new surface configuration
    set command of cfg to item 1 of argv
    set wait after command of cfg to false
    set environment variables of cfg to {"PATH=" & (item 2 of argv), "XDG_CONFIG_HOME=" & (item 3 of argv)}
    set viewerWindow to new window with configuration cfg
    return id of focused terminal of selected tab of viewerWindow
  end tell
end run
]],
    {
      M.shell_join(argv),
      vim.env.PATH,
      vim.env.XDG_CONFIG_HOME or '',
    },
    vim.schedule_wrap(function(result)
      finish_ghostty_launch(result, callback)
    end)
  )
end

function neovide.focus(_, callback)
  callback({ code = 1, stderr = 'Neovide source focus is unavailable; disable focus_on_inverse' })
end

function neovide.close()
  error('pdfterm: a Neovide source is not an owned viewer')
end

local function adapter(handle)
  local result = adapters[handle.kind]
  if not result or type(handle.id) ~= 'string' or handle.id == '' then
    error('pdfterm: invalid terminal handle')
  end
  return result
end

function ghostty.capture(callback)
  return ghostty_control.request(
    'capture',
    nil,
    vim.schedule_wrap(function(result)
      callback(result.code ~= 0 and result.stderr or nil, vim.trim(result.stdout or ''))
    end)
  )
end

-- A backend implements the entire contract; session/argv/handle policy stays here.
for name, backend in pairs(adapters) do
  for _, action in ipairs({ 'capture', 'launch', 'focus', 'close' }) do
    assert(type(backend[action]) == 'function', name .. ' terminal lacks ' .. action)
  end
end

function M.capture_source(callback)
  local kind = (vim.env.PDFTERM_LAUNCH_SOCKET or '') ~= '' and 'ssh'
    or vim.g.neovide and 'neovide'
    or vim.env.TERM_PROGRAM == 'WezTerm' and 'wezterm'
    or vim.env.TERM_PROGRAM == 'ghostty' and 'ghostty'
    or vim.env.KITTY_WINDOW_ID and 'kitty'
  if not kind then
    callback(
      'automatic split requires Kitty, Ghostty, or WezTerm control; use :PdfTermViewerCommand to launch in another Kitty-graphics-compatible terminal'
    )
    return
  end
  local ok, error = pcall(adapters[kind].capture, function(problem, id)
    if problem or not id or id == '' then
      callback(problem or ('could not identify source ' .. kind .. ' terminal'))
    else
      callback(nil, { kind = kind, id = id })
    end
  end)
  if not ok then
    callback(tostring(error))
  end
end

-- Callback runs before scheduling editor work so VimLeavePre can retain ownership
-- even when it is waiting for an in-flight launch to finish.
function M.launch_split(source, executable, pdf, callback, session, focus_token)
  local argv = { executable, pdf, '--synctex-best-guess' }
  if session then
    vim.list_extend(argv, { '--session', session })
  end
  if focus_token then
    vim.list_extend(argv, { '--focus-token', focus_token })
  end
  local done, reply, callback_failure = false, nil, nil
  adapter(source).launch(source, argv, function(result)
    local ok, failure = xpcall(function()
      local id = vim.trim(result.stdout or '')
      local kind = source.kind == 'neovide' and 'ghostty' or source.kind
      -- Failed rollback still transfers the exact provisional handle for exit cleanup.
      callback(result, (result.code == 0 or result.unclosed) and id ~= '' and { kind = kind, id = id } or nil)
    end, debug.traceback)
    done, reply = true, result
    if not ok then
      -- wait() must not report transport success when ownership recording failed.
      -- Rethrow as well: backends with provisional handles roll them back.
      callback_failure = failure
      error(failure)
    end
  end)
  return {
    wait = function(_, timeout)
      -- Local Ghostty may create, refocus, then roll back at three seconds each.
      if not vim.wait(timeout or (source.kind == 'ghostty' and 10000 or 8500), function()
        return done
      end, 10) then
        error('pdfterm: terminal launch timed out')
      end
      if callback_failure then
        error(reply.unclosed and reply.stderr or callback_failure)
      end
      return reply
    end,
  }
end

function M.focus(source, callback)
  return adapter(source).focus(source, callback)
end

function M.close(split)
  return adapter(split).close(split)
end

return M
