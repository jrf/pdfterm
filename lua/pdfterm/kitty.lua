-- Native Kitty remote control; session and handle policy stays in terminal.lua.
local M = {}

local function remote(arguments, callback)
  local command = { 'kitten', '@' }
  vim.list_extend(command, arguments)
  local socket = vim.env.KITTY_LISTEN_ON
  local function check(result)
    if result.code ~= 0 and (not socket or socket == '')
      and (result.stderr or ''):find('open /dev/tty', 1, true)
    then
      result.stderr = 'Kitty control requires a listen_on socket when Neovim has no controlling terminal; set listen_on in kitty.conf and restart Kitty: '
        .. result.stderr
    end
    if callback then
      callback(result)
    end
    return result
  end
  if callback then
    return vim.system(command, { text = true, timeout = 3000 }, check)
  end
  local process = vim.system(command, { text = true, timeout = 3000 })
  return { wait = function(_, timeout)
    return check(process:wait(timeout))
  end }
end

function M.launch(source, argv, callback)
  local arguments = {
    'launch',
    '--match',
    'window_id:' .. source.id,
    '--type=window',
    '--location=vsplit',
    '--keep-focus',
    '--next-to',
    'id:' .. source.id,
    '--env',
    'PATH=' .. vim.env.PATH,
    '--env',
    'XDG_CONFIG_HOME=' .. (vim.env.XDG_CONFIG_HOME or ''),
  }
  vim.list_extend(arguments, argv)
  return remote(
    { 'goto-layout', '--match', 'window_id:' .. source.id, 'splits' },
    vim.schedule_wrap(function(result)
      if result.code ~= 0 then
        callback(result)
        return
      end
      remote(arguments, callback)
    end)
  )
end

function M.focus(source, callback)
  return remote({ 'focus-window', '--match', 'id:' .. source.id }, callback)
end

function M.close(split)
  local found = remote({ 'ls' }):wait()
  if found.code ~= 0 then
    error('pdfterm: could not locate PDF split: ' .. (found.stderr or ''))
  end
  for _, os_window in ipairs(vim.json.decode(found.stdout)) do
    for _, tab in ipairs(os_window.tabs) do
      for _, window in ipairs(tab.windows) do
        if tostring(window.id) == split.id then
          local result = remote({ 'send-text', '--match', 'id:' .. split.id, '\003' }):wait()
          if result.code ~= 0 then
            error('pdfterm: could not quit owned PDF split: ' .. (result.stderr or ''))
          end
          return
        end
      end
    end
  end
end

function M.capture(callback)
  callback(nil, vim.env.KITTY_WINDOW_ID)
end

return M
