-- Read saved inputs only. PDF/source history and live-edit projection are not tracked.
local M = {}

function M.saved_revision(stat)
  if not stat then return nil end
  return { device = stat.dev, inode = stat.ino, length = stat.size,
    modified_seconds = stat.mtime.sec, modified_nanoseconds = stat.mtime.nsec,
    changed_seconds = stat.ctime.sec, changed_nanoseconds = stat.ctime.nsec }
end

local function read_prefix(path, limit, allow_truncate)
  local constants = vim.uv.constants
  local flags = bit.bor(assert(constants.O_RDONLY), assert(constants.O_NONBLOCK))
  local fd, reason = vim.uv.fs_open(path, flags, 0)
  if not fd then return nil, reason end
  local stat = vim.uv.fs_fstat(fd)
  local bytes, count
  if not stat or stat.type ~= 'file' then
    reason = 'not a regular file'
  elseif not allow_truncate and limit and stat.size > limit then
    reason = 'source file exceeds ' .. limit .. ' bytes'
  else
    count = limit and math.min(stat.size, limit) or stat.size
    bytes, reason = vim.uv.fs_read(fd, count, 0)
  end
  local closed, close_error = vim.uv.fs_close(fd)
  if not closed then return nil, close_error end
  if not bytes then return nil, reason end
  if #bytes ~= count then return nil, 'file changed while reading' end
  return bytes, nil, M.saved_revision(stat), stat.size > count
end

function M.read_bytes(path, bound)
  local bytes, reason, revision = read_prefix(path, bound, false)
  return bytes, reason, revision
end

local function scalar_length(first)
  return first >= 194 and first <= 223 and 2
    or first >= 224 and first <= 239 and 3
    or first >= 240 and first <= 244 and 4
    or nil
end

-- Reject overlong UTF-8, surrogates, incomplete scalars and NUL before scanning TeX.
local function next_scalar(text, index)
  local first = text:byte(index)
  if not first or first == 0 then return nil end
  if first < 128 then return index + 1 end
  local length = scalar_length(first)
  if not length then return nil end
  for at = index + 1, index + length - 1 do
    local byte = text:byte(at)
    if not byte or byte < 128 or byte > 191 then return nil end
  end
  local second = text:byte(index + 1)
  if first == 224 and second < 160 or first == 237 and second >= 160
    or first == 240 and second < 144 or first == 244 and second >= 144 then return nil end
  return index + length
end

local function validate_utf8(bytes, revision)
  local index = 1
  while true do
    -- Skip runs of non-NUL ASCII natively; only NUL and multibyte leads need decoding.
    index = bytes:find('[%z\128-\255]', index)
    if not index then return bytes, nil, revision end
    index = next_scalar(bytes, index)
    if not index then return nil, 'source is not valid UTF-8 text', revision end
  end
end

-- The third return identifies a stable byte read, even if UTF-8 validation fails.
function M.read_input(path)
  local bytes, reason, revision = read_prefix(path, 2 * 1024 * 1024, false)
  if not bytes then return nil, reason end
  return validate_utf8(bytes, revision)
end

function M.read_header(path, max_bytes)
  local bytes, reason, revision, truncated = read_prefix(path, max_bytes or 65536, true)
  if not bytes then return nil, reason end
  if truncated and #bytes > 0 then
    local s = #bytes
    while s > 1 and #bytes - s < 3 and bytes:byte(s) >= 128 and bytes:byte(s) <= 191 do s = s - 1 end
    local need = scalar_length(bytes:byte(s))
    if need and #bytes - s + 1 < need then bytes = bytes:sub(1, s - 1) end
  end
  return validate_utf8(bytes, revision)
end

return M
