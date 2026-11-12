-- Tinymist's preview uses Typst source spans, not PDF text matching. Its LSP
-- scrollPreview command delivers document positions over the local data plane.
local project_mod = require('pdfterm.project')
local source = require('pdfterm.source')
local M = {}
local active = {}
local retained = {}
local closing = {}

local function canonical(path, cwd)
  local absolute = vim.fs.normalize(vim.startswith(path, '/') and path or cwd .. '/' .. path)
  local prefix, suffix = absolute, nil
  while true do
    local real = vim.uv.fs_realpath(prefix)
    if real then
      return suffix and (real:gsub('/+$', '') .. '/' .. suffix) or real
    end
    local parent = vim.fs.dirname(prefix)
    if not parent or parent == prefix then return absolute end
    local base = vim.fs.basename(prefix)
    suffix = suffix and (base .. '/' .. suffix) or base
    prefix = parent
  end
end

local function literal_word(source, byte_column)
  -- Bound the literal excerpt for the forward socket, even on long
  -- prose lines. Cut only at whitespace so UTF-8 and edge words remain whole.
  -- Rust owns Unicode tokenization; do not duplicate it with Vim's ASCII classes.
  local first, last = math.max(1, byte_column - 255), math.min(#source, byte_column + 256)
  if first > 1 then
    first = source:find('%s', first)
    if not first or first > byte_column then return vim.NIL end
    first = first + 1
  end
  if last < #source then
    local boundary = source:sub(first, last):match('.*()%s')
    if not boundary then return vim.NIL end
    last = first + boundary - 2
  end
  if byte_column < first - 1 or byte_column >= last then return vim.NIL end
  return { text = source:sub(first, last), byte_column = byte_column - first + 1 }
end

local function compile_args(project)
  local argv = project.build
  local program = type(argv) == 'table' and vim.fs.basename(argv[1] or '') or ''
  assert((program == 'typst' or program == 'tinymist') and argv[2] == 'compile',
    'Typst forward search requires a typst/tinymist compile command, not a custom build wrapper')
  local values = {
    ['--root'] = true, ['--input'] = true, ['--font-path'] = true,
    ['--package-path'] = true, ['--package-cache-path'] = true,
    ['--creation-timestamp'] = true, ['--pdf-standard'] = true,
  }
  local switches = { ['--ignore-system-fonts'] = true, ['--no-pdf-tags'] = true }
  local args, positional, index, root, cli_fonts = {}, {}, 3, nil, false
  local separator = vim.fn.has('win32') == 1 and ';' or ':'
  local function fonts(value)
    for _, path in ipairs(vim.split(value, separator, { plain = true })) do
      assert(path ~= '', 'Typst font path must not be empty')
      args[#args + 1], args[#args + 2] = '--font-path', canonical(path, project.cwd)
    end
  end
  while index <= #argv do
    local arg = argv[index]
    local flag, value = arg:match('^(%-%-[^=]+)=(.*)$')
    flag = flag or arg
    if values[flag] or flag == '--format' or flag == '-f' then
      if not value then
        index = index + 1
        value = assert(argv[index], 'missing value for ' .. flag)
      end
      if flag == '--format' or flag == '-f' then
        assert(value == 'pdf', 'Typst forward search requires PDF output')
      elseif flag == '--font-path' then
        cli_fonts = true
        fonts(value)
      else
        if flag == '--root' then
          value = canonical(value, project.cwd)
          root = value
        end
        args[#args + 1], args[#args + 2] = flag, value
      end
    elseif switches[arg] then
      args[#args + 1] = arg
    elseif arg:sub(1, 1) == '-' then
      error('Typst forward search does not support build option ' .. arg)
    else
      positional[#positional + 1] = arg
    end
    index = index + 1
  end
  assert(#positional >= 1 and #positional <= 2
    and canonical(positional[1], project.cwd) == project.main
    and (not positional[2] or canonical(positional[2], project.cwd) == canonical(project.pdf, project.cwd)),
    'Typst build input/output do not match the selected project')
  -- CLI font paths replace the inherited list; Tinymist otherwise resolves
  -- relative font paths against the import root instead of the build cwd.
  if not cli_fonts and vim.env.TYPST_FONT_PATHS ~= nil then
    fonts(vim.env.TYPST_FONT_PATHS)
  end
  -- LSP workspace roots otherwise override the CLI's entry-directory default.
  if not root then
    root = canonical(vim.env.TYPST_ROOT or vim.fs.dirname(project.main), project.cwd)
    args[#args + 1], args[#args + 2] = '--root', root
  end
  args[#args + 1] = project.main
  return args, root
end

local function revision(path)
  return source.saved_revision(vim.uv.fs_stat(path))
end

-- Track project Typst files, not output directories, swap files, or unrelated
-- non-Typst root entries. Keep symlink identity as well as target metadata.
local function fingerprint(path)
  local stat = vim.uv.fs_lstat(path)
  if not stat then return { 'missing' } end
  local value = { stat.type, stat.dev, stat.ino, stat.size, stat.mtime, stat.ctime }
  if stat.type == 'link' then
    local target = vim.uv.fs_realpath(path)
    local resolved = target and vim.uv.fs_stat(target)
    value[7] = target or false
    value[8] = resolved and { resolved.type, resolved.dev, resolved.ino, resolved.size,
      resolved.mtime, resolved.ctime } or false
  end
  return value
end

local function scan_typst_inputs(root, main)
  local inputs = { [main] = fingerprint(main) }
  local visited = {}
  local scanned = 1
  local function walk(dir, depth)
    if depth > 8 or scanned >= 512 then return end
    local real_dir = canonical(dir, '/')
    if visited[real_dir] then return end
    visited[real_dir] = true
    local handle = vim.uv.fs_scandir(dir)
    if not handle then return end
    while scanned < 512 do
      local name, kind = vim.uv.fs_scandir_next(handle)
      if not name then break end
      scanned = scanned + 1
      if name:sub(1, 1) ~= '.' and name ~= 'target' and name ~= 'node_modules' then
        local full = dir .. '/' .. name
        kind = kind or (vim.uv.fs_lstat(full) or {}).type
        if kind == 'directory' or (kind == 'link' and (vim.uv.fs_stat(full) or {}).type == 'directory') then
          walk(full, depth + 1)
        elseif name:sub(-4):lower() == '.typ' then
          inputs[full] = fingerprint(full)
          local real = canonical(full, '/')
          inputs[real] = fingerprint(real)
        end
      end
    end
  end
  walk(root, 1)
  return inputs
end

local function unchanged(inputs)
  for path, value in pairs(inputs) do
    if not vim.deep_equal(value, fingerprint(path)) then return false end
  end
  return true
end

-- Only a local Tinymist data-plane connection is accepted. Frames are drained
-- incrementally, retaining at most 512 bytes: SVG updates can be very large.
local function websocket(port, ready, message, failure)
  local socket = assert(vim.uv.new_tcp())
  local buffer, upgraded, frame = '', false, nil
  local function send(opcode, payload)
    local mask = assert(vim.uv.random(4))
    local bytes = {}
    for i = 1, #payload do
      bytes[i] = string.char(bit.bxor(payload:byte(i), mask:byte((i - 1) % 4 + 1)))
    end
    socket:write(string.char(128 + opcode, 128 + #payload) .. mask .. table.concat(bytes))
  end
  local function consume(data)
    buffer = buffer .. data
    if not upgraded then
      local ending = buffer:find('\r\n\r\n', 1, true)
      if not ending then
        assert(#buffer <= 8192, 'oversized Tinymist websocket handshake')
        return
      end
      local header = buffer:sub(1, ending + 3)
      -- Fixed RFC 6455 nonce is sufficient for this private one-shot connection;
      -- checking its known response also rejects ordinary HTTP endpoints.
      assert(header:match('^HTTP/1%.1 101 ') and header:find('s3pPLMBiTxaQ9kYGzzhZRbK+xOo=', 1, true),
        'Tinymist refused the preview websocket connection')
      buffer, upgraded = buffer:sub(ending + 4), true
      ready(function(text)
        text = text or 'current'
        assert(#text <= 125, 'oversized Tinymist websocket command')
        send(1, text)
      end)
    end
    while true do
      if not frame then
        if #buffer < 2 then return end
        local first, second = buffer:byte(1, 2)
        assert(first >= 128 and first < 144 and second < 128,
          'unsupported Tinymist websocket frame')
        local length, offset = second, 3
        if length == 126 then
          if #buffer < 4 then return end
          length, offset = buffer:byte(3) * 256 + buffer:byte(4), 5
        elseif length == 127 then
          if #buffer < 10 then return end
          length, offset = 0, 11
          for i = 3, 10 do length = length * 256 + buffer:byte(i) end
        end
        assert(length <= 128 * 1024 * 1024, 'Tinymist preview frame exceeds 128 MiB')
        frame = { remaining = length, prefix = '', opcode = first - 128 }
        buffer = buffer:sub(offset)
      end
      local count = math.min(frame.remaining, #buffer)
      frame.prefix = frame.prefix .. buffer:sub(1, math.min(count, 512 - #frame.prefix))
      frame.remaining = frame.remaining - count
      buffer = buffer:sub(count + 1)
      if frame.remaining > 0 then return end
      local completed = frame
      frame = nil
      if completed.opcode == 8 then
        error('Tinymist closed the preview connection')
      elseif completed.opcode == 9 then
        assert(#completed.prefix <= 125, 'invalid Tinymist websocket ping')
        send(10, completed.prefix)
      elseif completed.opcode == 1 or completed.opcode == 2 then
        message(completed.prefix)
      end
    end
  end
  socket:connect('127.0.0.1', port, function(err)
    if socket:is_closing() then return end
    if err then failure(tostring(err)); return end
    socket:read_start(function(read_error, data)
      if read_error or not data then
        failure(tostring(read_error or 'Tinymist preview disconnected'))
        return
      end
      local ok, reason = pcall(consume, data)
      if not ok then failure(tostring(reason)) end
    end)
    socket:write(table.concat({
      'GET / HTTP/1.1', 'Host: 127.0.0.1:' .. port,
      'Origin: http://127.0.0.1:' .. port, 'Upgrade: websocket', 'Connection: Upgrade',
      'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==', 'Sec-WebSocket-Version: 13', '', '',
    }, '\r\n'))
  end)
  return socket
end

local function remove_directory(path)
  if not path then return end
  local entries = vim.uv.fs_scandir(path)
  if entries then
    while true do
      local name = vim.uv.fs_scandir_next(entries)
      if not name then break end
      vim.uv.fs_unlink(path .. '/' .. name)
    end
  end
  vim.uv.fs_rmdir(path)
end

local function integer(value, minimum)
  return type(value) == 'number' and value >= minimum and value < math.huge and value % 1 == 0
end

-- Preview source positions count Unicode scalars, including Tinymist's legacy
-- showDocument wrapper. UTF-16 is computed from the saved, compiled source.
local function location(path, row, character)
  assert(type(path) == 'string' and path:sub(1, 1) == '/' and not path:find('%z'),
    'Tinymist returned an invalid source path')
  path = canonical(path, '/')
  local stat = vim.uv.fs_stat(path)
  assert(stat and stat.type == 'file', 'mapped source is outside the saved project inputs')
  assert(integer(row, 0) and integer(character, 0), 'Tinymist returned an invalid source position')
  local lines = vim.fn.readfile(path, 'b', row + 2)
  local source = assert(lines[row + 1], 'Tinymist source line is outside the saved file')
  if lines[row + 2] then source = source:gsub('\r$', '') end
  local bytes, scalars, units = 0, 0, 0
  while scalars < character do
    local lead = assert(source:byte(bytes + 1), 'Tinymist source column is outside the saved line')
    local length = lead < 128 and 1 or lead >= 240 and 4 or lead >= 224 and 3 or lead >= 194 and 2
    assert(length and bytes + length <= #source, 'invalid UTF-8 in saved Typst source')
    for offset = 2, length do
      local continuation = source:byte(bytes + offset)
      assert(continuation >= 128 and continuation < 192, 'invalid UTF-8 in saved Typst source')
    end
    bytes, scalars, units = bytes + length, scalars + 1, units + (length == 4 and 2 or 1)
  end
  return { file = path, line = row + 1, byte_column = bytes,
    column = units + 1, column_char = scalars + 1, precise = true }
end

-- EOF frames both directions. The logical endpoint outlives individual
-- requests and compiler generations; dispatch owns ID-less reply isolation.
local function inverse_listener(path, dispatch)
  local server = assert(vim.uv.new_pipe(false))
  local clients, count = {}, 0
  local function close()
    if not server:is_closing() then server:close() end
    for close_client, is_answered in pairs(clients) do
      if not is_answered() then close_client() end
    end
    vim.uv.fs_unlink(path)
  end
  local ok, reason = pcall(function()
    assert(server:bind(path))
    assert(vim.uv.fs_chmod(path, 384))
    assert(server:listen(8, function(err)
      if err or server:is_closing() then return end
      local client = assert(vim.uv.new_pipe(false))
      if not server:accept(client) or count >= 8 then client:close(); return end
      count = count + 1
      local timer = assert(vim.uv.new_timer())
      local chunks, size, answered, cancel = {}, 0, false, nil
      local close_client
      close_client = function()
        if not clients[close_client] then return end
        clients[close_client], count = nil, count - 1
        timer:stop(); timer:close()
        if not client:is_closing() then client:read_stop(); client:close() end
      end
      clients[close_client] = function() return answered end
      local function abandon()
        local pending = cancel
        cancel = nil
        close_client()
        if pending then vim.schedule(pending) end
      end
      local function reply(value)
        if answered or client:is_closing() then return end
        answered, cancel = true, nil
        local encoded = vim.json.encode(value)
        if #encoded > 4096 then encoded = '{"ok":false,"error":"inverse response exceeds 4096 bytes"}' end
        client:write(encoded, function(write_error)
          if client:is_closing() then return end
          if write_error then close_client(); return end
          client:shutdown(function() close_client() end)
        end)
      end
      timer:start(9000, 0, function()
        if answered then close_client(); return end
        local pending = cancel
        reply({ ok = false, error = 'Typst inverse search timed out; the point may have no rendered source' })
        timer:start(1000, 0, close_client)
        if pending then vim.schedule(pending) end
      end)
      client:read_start(function(read_error, chunk)
        if read_error then abandon(); return end
        if chunk then
          size = size + #chunk
          if size > 4096 then
            client:read_stop()
            vim.schedule(function() reply({ ok = false, error = 'inverse request exceeds 4096 bytes' }) end)
          else
            chunks[#chunks + 1] = chunk
          end
          return
        end
        client:read_stop()
        vim.schedule(function()
          if answered or client:is_closing() then return end
          local decoded, request = pcall(vim.json.decode, table.concat(chunks))
          if not decoded then reply({ ok = false, error = 'invalid inverse request JSON' }); return end
          local dispatched, result = pcall(dispatch, request, reply)
          if not dispatched then reply({ ok = false, error = tostring(result) })
          elseif not answered then cancel = result end
        end)
      end)
    end))
  end)
  if not ok then close(); error(reason) end
  return close
end

local function normalize_project(project)
  return vim.tbl_extend('force', project, {
    main = canonical(project.main, project.cwd),
    cwd = canonical(project.cwd, project.cwd),
    pdf = canonical(project.pdf, project.cwd),
  })
end

function M.resolve(project, file, line, byte_column, callback)
  local ok_prep, prep = pcall(function()
    assert(vim.fn.executable('tinymist') == 1, 'tinymist is required for Typst cursor navigation')
    project = normalize_project(project)
    file = canonical(file, project.cwd)
    local parsed_args, parsed_root = compile_args(project)
    local src_line = vim.fn.readfile(file, '', line)[line]
    assert(src_line and byte_column >= 0 and byte_column <= #src_line, 'cursor is outside the saved Typst source')
    -- Preview looks up the leaf before the scalar boundary; normal-mode is ON
    -- the character, so send its trailing boundary.
    local char_col = vim.fn.strchars(src_line:sub(1, byte_column)) + (byte_column < #src_line and 1 or 0)
    return { args = parsed_args, root = parsed_root, source = src_line, character = char_col }
  end)
  if not ok_prep then
    local err_msg = 'Typst source navigation: ' .. tostring(prep)
    vim.schedule(function() callback({ code = 1, stdout = '', stderr = err_msg }) end)
    return function() end
  end
  local args, root, src_line, character = prep.args, prep.root, prep.source, prep.character
  local previous = retained[project.pdf]
  if previous and previous.matches(project, args) then
    return previous.forward(file, line, byte_column, src_line, character, callback)
  end
  if previous then previous.stop('Typst source map replaced') end

  local initial_completed, stopped, private, close_listener, debounce, fallback_timer = false, false
  local watchers = {}
  local generation, waiting, published, endpoint, service
  local pending_forward = {
    file = file, line = line, byte_column = byte_column,
    source = src_line, character = character, callback = callback,
    awaiting_jump = false, ready_for_jump = false,
  }
  local pending_build
  local fenced_until = 0
  local start_fn = vim.lsp.rpc.start
  local stop, launch, refresh, run_waiting, sync_watchers, schedule_watch_check

  local function close_timer(timer)
    if timer then timer:stop(); timer:close() end
  end
  local function clear_forward(pf)
    if not pf then return end
    close_timer(pf.timer)
    pf.timer = nil
    if pending_forward == pf then pending_forward = nil end
  end
  local function finish_forward(pf, error, payload)
    if not pf or pf.done then return end
    pf.done = true
    clear_forward(pf)
    local cb = pf.callback
    local value = { code = error and 1 or 0, stdout = payload or '', stderr = error or '' }
    vim.schedule(function() cb(value) end)
  end
  local function finish_build(error)
    local build = pending_build
    if not build then return end
    pending_build = nil
    build.finish(error)
  end
  local function clear_query(g)
    if not g or not g.query then return end
    close_timer(g.query.timer)
    g.query.timer = nil
    g.query = nil
  end
  local function close_watchers()
    close_timer(debounce); debounce = nil
    close_timer(fallback_timer); fallback_timer = nil
    for dir, entry in pairs(watchers) do
      watchers[dir] = nil
      entry.handle:stop()
      entry.handle:close()
    end
  end
  local function cleanup(g)
    closing[g] = nil
    remove_directory(g.temporary)
    g.temporary = nil
  end
  local function retire(g)
    if not g or g.stopped then return end
    g.stopped = true
    close_timer(g.timer); g.timer = nil
    if g.query then close_timer(g.query.timer); g.query.timer = nil end
    if g.socket and not g.socket:is_closing() then g.socket:read_stop(); g.socket:close() end
    if g.rpc and not g.exited then
      -- Each generation has a separate export directory: a terminating export
      -- cannot overwrite or recreate the next generation's output.
      closing[g] = true
      g.rpc.terminate()
    else
      cleanup(g)
    end
  end
  local function answer(query, value)
    if query and query.reply then
      local reply = query.reply
      query.reply = nil
      reply(value)
    end
  end
  stop = function(error)
    if stopped then return end
    stopped = true
    active[stop] = nil
    if retained[project.pdf] == service then retained[project.pdf] = nil end
    local message = error or 'Typst source-map service stopped'
    finish_forward(pending_forward, message)
    finish_build(message)
    answer(waiting, { ok = false, error = message })
    waiting = nil
    if generation then
      answer(generation.query, { ok = false, error = message })
      clear_query(generation)
      retire(generation)
    end
    close_watchers()
    if close_listener then close_listener(); close_listener = nil end
    remove_directory(private)
    private = nil
  end
  local function fail(g, reason)
    if stopped or generation ~= g or g.stopped then return end
    g.inputs[project.main] = g.inputs[project.main] or fingerprint(project.main)
    g.before = revision(project.pdf)
    sync_watchers()
    g.error = 'Typst source navigation: ' .. tostring(reason)
    finish_build(tostring(reason))
    answer(g.query, { ok = false, error = g.error })
    clear_query(g)
    answer(waiting, { ok = false, error = g.error })
    waiting = nil
    retire(g)
    if not initial_completed then
      stop(g.error)
    else
      finish_forward(pending_forward, g.error)
    end
  end
  local function guarded(g, fn)
    return function(...)
      if stopped or generation ~= g or g.stopped then return end
      local ok, reason = pcall(fn, ...)
      if not ok then fail(g, reason) end
    end
  end
  local function command(g, name, arguments, next_step)
    assert(g.rpc.request('workspace/executeCommand', { command = name, arguments = arguments },
      guarded(g, function(err, value)
        if err then fail(g, err.message or vim.inspect(err)); return end
        next_step(value)
      end)), 'Tinymist request could not be sent')
  end
  local function start_generation_timer(g)
    close_timer(g.timer)
    g.timer = assert(vim.uv.new_timer())
    g.timer:start(30000, 0, function()
      vim.schedule(guarded(g, function() fail(g, 'source mapping timed out (the cursor may have no rendered position)') end))
    end)
  end
  local function arm_forward_timeout(g, pf, timeout_ms, on_timeout)
    close_timer(pf.timer)
    pf.timer = assert(vim.uv.new_timer())
    pf.timer:start(timeout_ms, 0, function()
      vim.schedule(function()
        if stopped or pending_forward ~= pf or pf.done then return end
        if pf.awaiting_jump then
          fenced_until = math.max(fenced_until, vim.uv.hrtime() + 40000000)
        end
        local message = 'source mapping timed out (the cursor may have no rendered position)'
        if generation == g and not g.stopped and (g.scroll_inflight or 0) > 0 then
          if initial_completed then
            finish_forward(pf, 'Typst source navigation: ' .. message)
            refresh()
          else
            fail(g, message)
          end
          return
        end
        finish_forward(pf, 'Typst source navigation: ' .. message)
        if on_timeout then on_timeout() end
      end)
    end)
  end
  local function scroll_preview(g, pf, on_timeout)
    if pending_forward ~= pf or pf.done then return end
    arm_forward_timeout(g, pf, 2000, on_timeout)
    local function send_scroll()
      if stopped or generation ~= g or g.stopped or pending_forward ~= pf or pf.done then return end
      if (g.scroll_inflight or 0) > 0 then
        g.after_scroll = send_scroll
        return
      end
      local delay_ms = math.ceil((fenced_until - vim.uv.hrtime()) / 1000000)
      if delay_ms > 0 then
        vim.defer_fn(send_scroll, delay_ms)
        return
      end
      g.scroll_inflight = (g.scroll_inflight or 0) + 1
      pf.awaiting_jump = true
      pf.ready_for_jump = true
      local ok, err = pcall(command, g, 'tinymist.scrollPreview', { 'pdfterm', {
        event = 'panelScrollTo', filepath = pf.file, line = pf.line - 1, character = pf.character,
      } }, function()
        g.scroll_inflight = math.max(0, (g.scroll_inflight or 1) - 1)
        if g.scroll_inflight == 0 and g.after_scroll then
          local next_scroll = g.after_scroll
          g.after_scroll = nil
          fenced_until = math.max(fenced_until, vim.uv.hrtime() + 40000000)
          next_scroll()
        end
      end)
      if not ok then
        g.scroll_inflight = math.max(0, (g.scroll_inflight or 1) - 1)
        if initial_completed then
          finish_forward(pf, 'Typst source navigation: ' .. tostring(err))
        else
          fail(g, err)
        end
      end
    end
    send_scroll()
  end
  local function validate(g, request)
    assert(generation == g and not g.stopped and g.ready, 'Typst source map is refreshing')
    assert(published ~= nil and vim.deep_equal(published, revision(project.pdf)), 'PDF changed; source map is refreshing')
    assert(vim.deep_equal(request.revision, published), 'stale PDF revision; retry from the updated view')
    assert(unchanged(g.inputs), 'saved Typst inputs changed; source map is refreshing')
  end
  local debounce_pending = false
  local function ensure_fallback_poll()
    if stopped or fallback_timer then return end
    fallback_timer = assert(vim.uv.new_timer())
    fallback_timer:start(500, 500, function() schedule_watch_check() end)
  end
  schedule_watch_check = function()
    if stopped or debounce_pending then return end
    debounce_pending = true
    if not debounce then debounce = assert(vim.uv.new_timer()) end
    debounce:start(50, 0, function()
      debounce_pending = false
      vim.schedule(function()
        if stopped or not generation then return end
        for dir, entry in pairs(watchers) do
          if (vim.uv.fs_stat(dir) or {}).ino ~= entry.ino then sync_watchers(); break end
        end
        local g = generation
        local changed = not unchanged(g.inputs)
          or not vim.deep_equal(g.ready and published or g.before, revision(project.pdf))
        if changed then refresh() end
      end)
    end)
  end
  sync_watchers = function()
    if stopped then return end
    local dirs, watch_failed = {}, false
    local root_prefix = project.cwd:gsub('/+$', '') .. '/'
    local function add_path(path)
      local dir = vim.fs.dirname(path)
      while dir and dir ~= '/' and dir ~= '.' and vim.fn.isdirectory(dir) == 0 do
        dir = vim.fs.dirname(dir)
      end
      while dir and vim.fn.isdirectory(dir) == 1 do
        local resolved = canonical(dir, '/')
        dirs[resolved] = true
        if resolved == project.cwd or resolved:sub(1, #root_prefix) ~= root_prefix then break end
        dir = vim.fs.dirname(resolved)
      end
    end
    add_path(project.pdf)
    add_path(project.main)
    if generation then
      for path in pairs(generation.inputs) do add_path(path) end
    end
    for dir, entry in pairs(watchers) do
      if not dirs[dir] or (vim.uv.fs_stat(dir) or {}).ino ~= entry.ino then
        watchers[dir] = nil
        entry.handle:stop()
        entry.handle:close()
      elseif entry.failed then
        watch_failed = true
      end
    end
    for dir in pairs(dirs) do
      if not watchers[dir] then
        local ino = (vim.uv.fs_stat(dir) or {}).ino
        local handle = vim.uv.new_fs_event()
        local entry = { handle = handle, ino = ino }
        if handle and handle:start(dir, {}, function(err)
          if err then
            entry.failed = true
            vim.schedule(ensure_fallback_poll)
          end
          schedule_watch_check()
        end) then
          watchers[dir] = entry
        else
          if handle then handle:close() end
          watch_failed = true
        end
      end
    end
    if watch_failed then
      ensure_fallback_poll()
    elseif fallback_timer then
      close_timer(fallback_timer); fallback_timer = nil
    end
  end
  refresh = function(reason)
    if stopped then return end
    if not initial_completed then
      stop(reason or 'Typst inputs or PDF changed during forward search; repeat navigation')
      return
    end
    finish_forward(pending_forward, reason or 'Typst inputs or PDF changed during forward search; repeat navigation')
    if generation then
      answer(generation.query, { ok = false, error = reason or 'Typst source map is refreshing' })
      clear_query(generation)
      retire(generation)
    end
    launch()
  end
  local function arm_query_timer(g, query, timeout_ms)
    close_timer(query.timer)
    query.timer = assert(vim.uv.new_timer())
    query.timer:start(timeout_ms, 0, function()
      vim.schedule(function()
        if stopped or generation ~= g or g.query ~= query then return end
        clear_query(g)
        if not query.abandoned then
          answer(query, { ok = false, error = 'Typst inverse search timed out; the point may have no rendered source' })
        end
        run_waiting()
      end)
    end)
  end
  run_waiting = function()
    local g, query = generation, waiting
    if not g or not g.ready or g.query or not query then return end
    waiting = nil
    local ok, reason = pcall(validate, g, query.request)
    if not ok then
      answer(query, { ok = false, error = tostring(reason) })
      local msg = tostring(reason)
      if msg:find('PDF changed', 1, true) or msg:find('inputs changed', 1, true) then
        refresh()
      end
      return
    end
    g.query = query
    arm_query_timer(g, query, 1000)
    local sent, error = pcall(g.send, 'src-point ' .. vim.json.encode({
      page_no = query.request.page, x = query.request.x, y = query.request.y,
    }))
    if not sent then fail(g, error); refresh() end
  end
  local function inverse(request, reply)
    assert(type(request) == 'table' and type(request.revision) == 'table'
      and integer(request.page, 1) and request.page <= 4294967295
      and type(request.x) == 'number' and type(request.y) == 'number'
      and request.x >= 0 and request.y >= 0 and request.x < math.huge and request.y < math.huge,
      'invalid Typst inverse point')
    assert(not stopped, 'Typst source-map service stopped')
    local query = { request = request, reply = reply }
    answer(waiting, { ok = false, error = 'Typst inverse request superseded' })
    waiting = query
    local g = generation
    if not g or g.stopped then
      refresh()
    elseif g.query then
      if not g.query.abandoned then
        answer(g.query, { ok = false, error = 'Typst inverse request superseded' })
        g.query.abandoned = true
        arm_query_timer(g, g.query, 50)
      end
    elseif g.ready then
      run_waiting()
    elseif not unchanged(g.inputs) then
      refresh()
    end
    return function()
      if waiting == query then
        waiting = nil
      elseif generation and generation.query == query then
        query.reply = nil
        if not query.abandoned then
          query.abandoned = true
          arm_query_timer(generation, query, 50)
        end
      end
    end
  end
  local function ensure_endpoint()
    if close_listener then close_listener(); close_listener = nil end
    remove_directory(private)
    private, endpoint = nil, nil
    local tmp = vim.fn.isdirectory('/tmp') == 1 and '/tmp' or vim.uv.os_tmpdir():gsub('/+$', '')
    private = canonical(assert(vim.uv.fs_mkdtemp(tmp .. '/pdfterm-XXXXXX')), '/')
    assert(vim.uv.fs_chmod(private, 448))
    local owner = assert(vim.uv.fs_lstat(private))
    assert(owner.uid == vim.uv.getuid() and bit.band(owner.mode, 511) == 448,
      'Typst inverse socket directory is not private')
    endpoint = private .. '/inverse.sock'
    close_listener = inverse_listener(endpoint, inverse)
  end
  local function complete_forward(g, pf)
    close_timer(g.timer); g.timer = nil
    ensure_endpoint()
    initial_completed = true
    retained[project.pdf] = service
    finish_forward(pf, nil, vim.json.encode({ pdf = project.pdf, revision = published,
      best_guess = true, inverse_search = endpoint,
      page = g.position.page, h = g.position.x, v = g.position.y,
      width = 0, height = 0, word = literal_word(pf.source, pf.byte_column) }))
  end
  service = {
    stop = stop,
    matches = function(cand_project, cand_args)
      return not stopped and initial_completed
        and start_fn == vim.lsp.rpc.start
        and project.cwd == cand_project.cwd and project.main == cand_project.main
        and vim.deep_equal(args, cand_args)
    end,
    is_coherent = function(cand_project, cand_args)
      return not stopped and initial_completed
        and generation ~= nil and generation.ready and not generation.stopped
        and not generation.exited and not generation.query and not waiting
        and start_fn == vim.lsp.rpc.start
        and project.cwd == cand_project.cwd and project.main == cand_project.main
        and vim.deep_equal(args, cand_args)
        and published ~= nil and vim.deep_equal(published, revision(project.pdf))
        and unchanged(generation.inputs)
    end,
    forward = function(next_file, next_line, next_byte, next_source, next_char, next_callback)
      if pending_forward then
        if pending_forward.awaiting_jump then
          fenced_until = math.max(fenced_until, vim.uv.hrtime() + 40000000)
        end
        finish_forward(pending_forward, 'Typst forward request superseded')
      end
      if generation and generation.exported and not generation.ready and not generation.stopped then
        generation.publish()
      end
      local pf = {
        file = next_file, line = next_line, byte_column = next_byte,
        source = next_source, character = next_char, callback = next_callback,
        awaiting_jump = false, ready_for_jump = false,
      }
      local coherent = generation and generation.ready and not generation.stopped
        and not generation.exited and published ~= nil
        and vim.deep_equal(published, revision(project.pdf))
        and unchanged(generation.inputs)
      if coherent then
        pending_forward = pf
        generation.position = nil
        scroll_preview(generation, pf)
      else
        local refreshing_current = generation and not generation.ready and not generation.stopped
          and not generation.exited and not generation.exported
          and unchanged(generation.inputs)
          and vim.deep_equal(generation.before, revision(project.pdf))
        if not refreshing_current then
          refresh('Typst source map refreshing')
        end
        pending_forward = pf
      end
      return function()
        if pending_forward == pf then
          pf.done = true
          if pf.awaiting_jump then
            fenced_until = math.max(fenced_until, vim.uv.hrtime() + 40000000)
          end
          clear_forward(pf)
          if generation and generation.exported and not generation.ready and not generation.stopped then
            generation.publish()
          end
        end
      end
    end,
    build = function(id)
      if not project_mod.current(id) then return end
      local notification = pending_build and pending_build.notification
        or { id = 'pdfterm.build.' .. id, title = 'pdfterm: ' .. vim.fs.basename(project.pdf) }
      local finished = false
      local function notify_build(status, level)
        vim.notify(status, level, vim.tbl_extend('force', notification, { timeout = finished and 5000 or false }))
      end
      pending_build = {
        notification = notification,
        finish = function(err)
          if finished then return end
          finished = true
          if err then
            notify_build('Compilation failed\n' .. tostring(err), vim.log.levels.ERROR)
          else
            notify_build('Compilation OK', vim.log.levels.INFO)
          end
        end,
      }
      notify_build('Compiling…', vim.log.levels.INFO)
      if stopped then return end
      if not generation or generation.stopped or generation.exited or generation.ready
        or generation.exported or not unchanged(generation.inputs)
        or not vim.deep_equal(generation.before, revision(project.pdf)) then
        refresh('Typst build requested')
      end
    end,
  }
  launch = function()
    if stopped then return end
    local g = { inputs = scan_typst_inputs(root, project.main), before = revision(project.pdf) }
    generation = g
    sync_watchers()
    local start = guarded(g, function()
      local pdf_dir = vim.fs.dirname(project.pdf)
      vim.fn.mkdir(pdf_dir, 'p')
      g.temporary = canonical(assert(vim.uv.fs_mkdtemp(pdf_dir .. '/.pdfterm-XXXXXX')), project.cwd)
      local output = g.temporary .. '/mapped.pdf'
      local function publish()
        if not unchanged(g.inputs) or not vim.deep_equal(g.export_inputs, g.inputs)
          or not vim.deep_equal(g.before, revision(project.pdf)) then
          refresh('Typst inputs or PDF changed during source-map refresh')
          return false
        end
        vim.fn.mkdir(vim.fs.dirname(project.pdf), 'p')
        assert(vim.uv.fs_rename(output, project.pdf))
        published = assert(revision(project.pdf), 'cannot stat exported Typst PDF: ' .. project.pdf)
        g.ready = true
        close_timer(g.timer); g.timer = nil
        sync_watchers()
        if not pending_forward and not endpoint then
          ensure_endpoint()
        end
        if not initial_completed then
          initial_completed = true
          retained[project.pdf] = service
        end
        finish_build(nil)
        run_waiting()
        return true
      end
      g.publish = publish
      local function export()
        if g.exporting or g.ready then return end
        g.exporting = true
        g.export_inputs = vim.deepcopy(g.inputs)
        command(g, 'tinymist.exportPdf', { project.main }, function(value)
          assert(value ~= vim.NIL and value ~= nil and vim.uv.fs_stat(output),
            'Tinymist could not export the current saved document')
          if pending_forward then
            g.exported = true
            scroll_preview(g, pending_forward, function()
              if generation == g and not g.stopped and not g.ready then
                publish()
              end
            end)
          else
            publish()
          end
        end)
      end
      local function source_position(path, row, column)
        local query = g.query
        if not query then return end
        clear_query(g)
        if query.abandoned then
          run_waiting()
          return
        end
        local ok, mapped = pcall(function()
          assert(generation == g and not g.stopped and g.ready
            and vim.deep_equal(query.request.revision, published)
            and vim.deep_equal(published, revision(project.pdf))
            and vim.deep_equal(g.inputs[project.main], fingerprint(project.main)),
            'Typst source map is refreshing')
          local loc = location(path, row, column)
          assert(not g.inputs[loc.file] or vim.deep_equal(g.inputs[loc.file], fingerprint(loc.file)),
            'Typst source map is refreshing')
          return loc
        end)
        answer(query, ok and { ok = true, location = mapped } or { ok = false, error = tostring(mapped) })
        if not ok and tostring(mapped):find('source map is refreshing', 1, true)
          and generation == g and not g.stopped then
          refresh()
        else
          run_waiting()
        end
      end
      local function on_message(data)
        if data:match('^new,') or data:match('^diff%-v1,') then
          g.rendered = true
          if not unchanged(g.inputs) then refresh(); return end
          export()
        elseif (g.exported or g.ready) and pending_forward and pending_forward.ready_for_jump and data:match('^jump,') then
          local pf = pending_forward
          pf.awaiting_jump = false
          local page, x, y = data:match('^jump,(%d+) ([^ ,]+) ([^ ,]+)')
          page, x, y = tonumber(page), tonumber(x), tonumber(y)
          assert(page and page > 0 and x and y and x == x and y == y
            and math.abs(x) < math.huge and math.abs(y) < math.huge, 'invalid Tinymist document position')
          g.position = { page = page, x = x, y = y }
          if g.ready then
            if not unchanged(g.inputs) or not vim.deep_equal(published, revision(project.pdf)) then
              refresh('Typst inputs or PDF changed during forward search; repeat navigation')
              return
            end
            complete_forward(g, pf)
          else
            if publish() then complete_forward(g, pf) end
          end
        end
      end
      g.rpc = start_fn({ 'tinymist', 'lsp' }, {
        notification = guarded(g, function(method, params)
          if method == 'window/showMessage' and params.type == 1 then
            fail(g, params.message)
          elseif method == 'tinymist/preview/scrollSource' and g.query then
            assert(type(params) == 'table' and type(params.start) == 'table',
              'Tinymist returned a missing source position')
            source_position(params.filepath, params.start[1], params.start[2])
          elseif method == 'tinymist/compileStatus' and not g.ready then
            if params.status == 'compileError' then
              g.compile_failed = true
              if g.preview_started and not g.rendered then export() end
            elseif params.status == 'compiling' or params.status == 'compileSuccess' then
              g.compile_failed = false
            end
          end
        end),
        server_request = function(method, params)
          if stopped or generation ~= g or g.stopped then return vim.NIL end
          local ok, reason = pcall(function()
            if method == 'window/showDocument' and g.query then
              assert(type(params) == 'table' and type(params.selection) == 'table'
                and type(params.selection.start) == 'table', 'Tinymist returned a missing source position')
              source_position(vim.uri_to_fname(params.uri), params.selection.start.line,
                params.selection.start.character)
            end
          end)
          if not ok then fail(g, reason) end
          return method == 'window/showDocument' and { success = ok } or vim.NIL
        end,
        on_error = function(_, err)
          vim.schedule(function() fail(g, vim.inspect(err)) end)
        end,
        on_exit = function()
          g.exited = true
          if generation == g and not g.stopped then fail(g, 'Tinymist compiler exited') end
          cleanup(g)
        end,
      }, { cwd = project.cwd, detached = false })
      start_generation_timer(g)
      assert(g.rpc.request('initialize', {
        processId = vim.fn.getpid(), rootUri = vim.uri_from_fname(project.cwd),
        capabilities = { general = { positionEncodings = { 'utf-16' } } },
        initializationOptions = {
          exportPdf = 'never', outputPath = g.temporary .. '/mapped', typstExtraArgs = args,
          compileStatus = 'enable', customizedShowDocument = true,
          formatterMode = 'disable', semanticTokens = 'disable',
        },
      }, guarded(g, function(err, value)
        if err then fail(g, err.message); return end
        local commands = value.capabilities.executeCommandProvider
        assert(commands and vim.tbl_contains(commands.commands, 'tinymist.doStartPreview'),
          'installed Tinymist does not support the preview source-map protocol')
        g.rpc.notify('initialized', vim.empty_dict())
        command(g, 'tinymist.pinMain', { project.main }, function()
          command(g, 'tinymist.doStartPreview', { {
            '--task-id=pdfterm', '--data-plane-host=127.0.0.1:0', '--no-open', project.main,
          } }, function(preview)
            local port = preview.dataPlanePort
            assert(type(port) == 'number' and port > 0 and port < 65536,
              'Tinymist did not expose a local preview data plane')
            g.preview_started = true
            g.socket = websocket(port, guarded(g, function(send)
              g.send = send
              send()
              if g.compile_failed and not g.rendered then export() end
            end),
              function(data) vim.schedule(guarded(g, function() on_message(data) end)) end,
              function(error) vim.schedule(guarded(g, function() fail(g, error) end)) end)
          end)
        end)
      end)), 'Tinymist initialization could not be sent')
    end)
    start()
  end
  active[stop] = true
  retained[project.pdf] = service
  local initial_pf = pending_forward
  local ok, reason = pcall(launch)
  if not ok then stop('Typst source navigation: ' .. tostring(reason)) end
  return function()
    if pending_forward == initial_pf then
      initial_pf.done = true
      if initial_completed then
        clear_forward(initial_pf)
      else
        stop('navigation cancelled')
      end
    end
  end
end

function M.build(project, id)
  local normalized = normalize_project(project)
  local service = retained[normalized.pdf]
  if not service then return false end
  local ok, parsed_args = pcall(compile_args, normalized)
  if not ok or not service.matches(normalized, parsed_args) then
    service.stop('Typst source-map ownership released')
    return false
  end
  service.build(id)
  return true
end

function M.is_coherent(project)
  local ok, coherent = pcall(function()
    local normalized = normalize_project(project)
    local service = retained[normalized.pdf]
    return service ~= nil and service.is_coherent(normalized, compile_args(normalized))
  end)
  return ok and coherent
end

-- A different builder must be able to take ownership of the same PDF path.
function M.release(pdf)
  local service = retained[canonical(pdf, vim.fn.getcwd())]
  if service then service.stop('Typst source-map ownership released') end
end

vim.api.nvim_create_autocmd('VimLeavePre', {
  callback = function()
    for finish in pairs(active) do finish('Neovim is exiting') end
    vim.wait(2000, function() return next(closing) == nil end, 10)
  end,
})

return M
