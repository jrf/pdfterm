-- OS-specific editor operations. Unix sockets and terminal escape sequences do
-- not belong here: their existing Unix/protocol implementations are shared.
local M = {}

local function require_macos(feature)
  if vim.uv.os_uname().sysname ~= 'Darwin' then
    error('pdfterm: ' .. feature .. ' requires macOS')
  end
end

function M.applescript(script, arguments, callback)
  require_macos('AppleScript terminal control')
  local command = { 'osascript', '-e', script }
  vim.list_extend(command, arguments or {})
  return vim.system(command, { text = true, timeout = 3000 }, callback)
end

return M
