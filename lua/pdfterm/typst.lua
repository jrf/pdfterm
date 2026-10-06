-- Tinymist's preview uses Typst source spans, not PDF text matching. Its LSP
-- scrollPreview command delivers document positions over the local data plane.
local M = {}
local active = {}
local retained = {}
local closing = {}

local function canonical(path, cwd)
  local absolute = vim.fs.normalize(vim.startswith(path, '/') and path or cwd .. '/' .. path)
  return vim.uv.fs_realpath(absolute) or absolute
end

local function literal_word(source, byte_column)
  -- Bound the literal excerpt for the 4096-byte forward socket, even on long
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
  local stat = assert(vim.uv.fs_stat(path), 'cannot stat Typst PDF: ' .. path)
  return {
    device = stat.dev, inode = stat.ino, length = stat.size,
    modified_seconds = stat.mtime.sec, modified_nanoseconds = stat.mtime.nsec,
    changed_seconds = stat.ctime.sec, changed_nanoseconds = stat.ctime.nsec,
  }
end

-- Track compiler-accessed files, not output directories, swap files, or other
-- unrelated root entries. Keep symlink identity as well as target metadata.
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

local function unchanged(inputs)
  for path, value in pairs(inputs) do
    if not vim.deep_equal(value, fingerprint(path)) then return false end
  end
  return true
end

local function saved_bytes(path)
  local fd, error = vim.uv.fs_open(path, 'r', 0)
  if not fd then return nil, error end
  local stat = vim.uv.fs_fstat(fd)
  local bytes, reason
  if stat and stat.type == 'file' then bytes, reason = vim.uv.fs_read(fd, stat.size, 0)
  else reason = 'not a regular file' end
  vim.uv.fs_close(fd)
  return bytes, reason
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
local function location(path, row, character, inputs)
  assert(type(path) == 'string' and path:sub(1, 1) == '/' and not path:find('%z'),
    'Tinymist returned an invalid source path')
  path = canonical(path, '/')
  assert(inputs[path] and inputs[path][1] == 'file', 'mapped source is outside the saved project inputs')
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

function M.resolve(project, file, line, byte_column, callback)
  local completed, stopped, private, close_listener, monitor
  local generation, waiting, published, endpoint, args, source, character
  local stop, launch, refresh, run_waiting
  local function complete(error, payload)
    if completed then return end
    completed = true
    local value = { code = error and 1 or 0, stdout = payload or '', stderr = error or '' }
    vim.schedule(function() callback(value) end)
  end
  local function cleanup(g)
    closing[g] = nil
    remove_directory(g.temporary)
    g.temporary = nil
  end
  local function retire(g)
    if not g or g.stopped then return end
    g.stopped = true
    if g.timer then g.timer:stop(); g.timer:close(); g.timer = nil end
    if g.settle then g.settle:stop(); g.settle:close(); g.settle = nil end
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
    if retained[project.pdf] == stop then retained[project.pdf] = nil end
    complete(error or 'Typst source-map service stopped')
    answer(waiting, { ok = false, error = error or 'Typst source-map service stopped' })
    waiting = nil
    if generation then
      answer(generation.query, { ok = false, error = error or 'Typst source-map service stopped' })
      retire(generation)
    end
    if monitor then monitor:stop(); monitor:close(); monitor = nil end
    if close_listener then close_listener(); close_listener = nil end
    remove_directory(private)
    private = nil
  end
  local function fail(g, reason)
    if stopped or generation ~= g or g.stopped then return end
    g.error = 'Typst source navigation: ' .. tostring(reason)
    answer(g.query, { ok = false, error = g.error })
    g.query = nil
    answer(waiting, { ok = false, error = g.error })
    waiting = nil
    retire(g)
    if not completed then stop(g.error) end
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
  local function validate(g, request)
    assert(generation == g and not g.stopped and g.ready, 'Typst source map is refreshing')
    assert(vim.deep_equal(published, revision(project.pdf)), 'PDF changed; source map is refreshing')
    assert(vim.deep_equal(request.revision, published), 'stale PDF revision; retry from the updated view')
    assert(unchanged(g.inputs), 'saved Typst inputs changed; source map is refreshing')
  end
  refresh = function(reason)
    if stopped then return end
    if not completed then
      stop(reason or 'Typst inputs or PDF changed during forward search; repeat navigation')
      return
    end
    if generation then
      answer(generation.query, { ok = false, error = reason or 'Typst source map is refreshing' })
      generation.query = nil
      retire(generation)
    end
    launch()
  end
  run_waiting = function()
    local g, query = generation, waiting
    if not g or not g.ready or not query then return end
    waiting = nil
    local ok, reason = pcall(validate, g, query.request)
    if not ok then
      answer(query, { ok = false, error = tostring(reason) })
      if not unchanged(g.inputs) or not vim.deep_equal(published, revision(project.pdf)) then
        refresh()
      end
      return
    end
    g.query = query
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
    if g and g.query then
      -- An ID-less response cannot be reassigned to the newer click. Fence all
      -- callbacks by retiring the whole compiler, not just its websocket.
      refresh('Typst inverse request superseded')
    elseif not g or g.stopped then
      refresh()
    elseif not unchanged(g.inputs) or (g.ready and not vim.deep_equal(published, revision(project.pdf))) then
      refresh()
    else
      run_waiting()
    end
    return function()
      if waiting == query then
        waiting = nil
      elseif generation and generation.query == query then
        query.reply = nil
        refresh('Typst inverse request cancelled')
      end
    end
  end
  launch = function()
    if stopped then return end
    local g = { inputs = {}, before = revision(project.pdf) }
    generation = g
    local start = guarded(g, function()
      g.temporary = canonical(assert(vim.uv.fs_mkdtemp(vim.fs.dirname(project.pdf) .. '/.pdfterm-XXXXXX')),
        project.cwd)
      local output = g.temporary .. '/mapped.pdf'
      local function publish()
        if not unchanged(g.inputs) or not vim.deep_equal(g.export_inputs, g.inputs)
          or not vim.deep_equal(g.before, revision(project.pdf)) then
          refresh('Typst inputs or PDF changed during source-map refresh')
          return
        end
        assert(vim.uv.fs_rename(output, project.pdf))
        published, g.ready = revision(project.pdf), true
        if g.timer then g.timer:stop(); g.timer:close(); g.timer = nil end
        if not completed then
          private = canonical(assert(vim.uv.fs_mkdtemp('/tmp/pdfterm-XXXXXX')), '/')
          assert(vim.uv.fs_chmod(private, 448))
          local owner = assert(vim.uv.fs_lstat(private))
          assert(owner.uid == vim.uv.getuid() and bit.band(owner.mode, 511) == 448,
            'Typst inverse socket directory is not private')
          endpoint = private .. '/inverse.sock'
          close_listener = inverse_listener(endpoint, inverse)
          retained[project.pdf] = stop
          complete(nil, vim.json.encode({ pdf = project.pdf, revision = published,
            inverse_search = endpoint, page = g.position.page, h = g.position.x, v = g.position.y,
            width = 0, height = 0, word = literal_word(source, byte_column) }))
        end
        run_waiting()
      end
      local function export()
        if g.exporting or g.ready then return end
        g.exporting = true
        g.export_inputs = vim.deepcopy(g.inputs)
        command(g, 'tinymist.exportPdf', { project.main }, function(value)
          assert(value ~= vim.NIL and value ~= nil and vim.uv.fs_stat(output),
            'Tinymist could not export the current saved document')
          if completed then
            publish()
          else
            g.exported = true
            command(g, 'tinymist.scrollPreview', { 'pdfterm', {
              event = 'panelScrollTo', filepath = file, line = line - 1, character = character,
            } }, function() end)
          end
        end)
      end
      local function source_position(path, row, column)
        local query = g.query
        if not query then return end
        g.query = nil
        local ok, mapped = pcall(function()
          validate(g, query.request)
          local value = location(path, row, column, g.inputs)
          validate(g, query.request)
          return value
        end)
        answer(query, ok and { ok = true, location = mapped } or { ok = false, error = tostring(mapped) })
        if not unchanged(g.inputs) or not vim.deep_equal(published, revision(project.pdf)) then refresh() end
      end
      local function on_message(data)
        if data:match('^new,') or data:match('^diff%-v1,') then
          g.rendered = true
          if not unchanged(g.inputs) then refresh(); return end
          export()
        elseif g.exported and not completed and data:match('^jump,') then
          local page, x, y = data:match('^jump,(%d+) ([^ ,]+) ([^ ,]+)')
          page, x, y = tonumber(page), tonumber(x), tonumber(y)
          assert(page and page > 0 and x and y and x == x and y == y
            and math.abs(x) < math.huge and math.abs(y) < math.huge, 'invalid Tinymist document position')
          g.position = { page = page, x = x, y = y }
          publish()
        end
      end
      local function filesystem(params)
        assert(type(params) == 'table' and type(params.inserts) == 'table',
          'Tinymist returned an invalid input footprint')
        local inserts = {}
        for _, uri in ipairs(params.inserts) do
          local path = vim.uri_to_fname(uri)
          local before = fingerprint(path)
          if g.ready or (g.inputs[path] and not vim.deep_equal(g.inputs[path], before)) then
            vim.schedule(guarded(g, function() refresh() end))
            return
          end
          g.inputs[path] = before
          local real = canonical(path, '/')
          g.inputs[real] = fingerprint(real)
          local bytes, reason = saved_bytes(path)
          assert(vim.deep_equal(before, fingerprint(path)), 'Typst input changed while reading saved bytes')
          inserts[#inserts + 1] = { uri = uri, content = bytes
            and { type = 'ok', content = vim.base64.encode(bytes) }
            or { type = 'err', error = tostring(reason) } }
        end
        vim.schedule(guarded(g, function()
          assert(g.rpc.request('tinymist/fsChange', {
            inserts = inserts, removes = params.removes or {}, isSync = false,
          }, guarded(g, function(err)
            if err then fail(g, err.message or vim.inspect(err)) end
          end)), 'Tinymist saved inputs could not be sent')
        end))
      end
      g.rpc = vim.lsp.rpc.start({ 'tinymist', 'lsp' }, {
        notification = guarded(g, function(method, params)
          if method == 'window/showMessage' and params.type <= 2 then
            fail(g, params.message)
          elseif method == 'tinymist/preview/scrollSource' and g.query then
            assert(type(params) == 'table' and type(params.start) == 'table',
              'Tinymist returned a missing source position')
            source_position(params.filepath, params.start[1], params.start[2])
          elseif method == 'tinymist/compileStatus' and params.status == 'compileError' and not g.ready then
            -- Missing delegated inputs can temporarily fail a compile. Wait
            -- for its footprint to settle, then use explicit export (which
            -- never falls back to the previous successful preview).
            if not g.settle then g.settle = assert(vim.uv.new_timer()) end
            g.settle:start(300, 0, function()
              vim.schedule(guarded(g, function()
                if g.preview_started and not g.rendered then export() end
              end))
            end)
          end
        end),
        server_request = function(method, params)
          if stopped or generation ~= g or g.stopped then return vim.NIL end
          local ok, reason = pcall(function()
            if method == 'tinymist/fs/watch' then
              filesystem(params)
            elseif method == 'window/showDocument' and g.query then
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
      g.timer = assert(vim.uv.new_timer())
      g.timer:start(30000, 0, function()
        vim.schedule(guarded(g, function() fail(g, 'source mapping timed out (the cursor may have no rendered position)') end))
      end)
      assert(g.rpc.request('initialize', {
        processId = vim.fn.getpid(), rootUri = vim.uri_from_fname(project.cwd),
        capabilities = { general = { positionEncodings = { 'utf-16' } } },
        initializationOptions = {
          exportPdf = 'never', outputPath = g.temporary .. '/mapped', typstExtraArgs = args,
          delegateFsRequests = true, compileStatus = 'enable', customizedShowDocument = true,
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
            g.socket = websocket(port, guarded(g, function(send) g.send = send; send() end),
              function(data) vim.schedule(guarded(g, function() on_message(data) end)) end,
              function(error) vim.schedule(guarded(g, function() fail(g, error) end)) end)
          end)
        end)
      end)), 'Tinymist initialization could not be sent')
    end)
    start()
  end
  active[stop] = true
  local ok, reason = pcall(function()
    assert(vim.fn.executable('tinymist') == 1, 'tinymist is required for Typst cursor navigation')
    project = vim.tbl_extend('force', project, {
      main = canonical(project.main, project.cwd),
      cwd = canonical(project.cwd, project.cwd),
      pdf = canonical(project.pdf, project.cwd),
    })
    file = canonical(file, project.cwd)
    args = compile_args(project)
    source = vim.fn.readfile(file, '', line)[line]
    assert(source and byte_column >= 0 and byte_column <= #source, 'cursor is outside the saved Typst source')
    -- Preview looks up the leaf before the scalar boundary; normal-mode is ON
    -- the character, so send its trailing boundary.
    character = vim.fn.strchars(source:sub(1, byte_column)) + (byte_column < #source and 1 or 0)
    -- A new explicit forward search owns this PDF before starting its compiler.
    -- The previous automatic publisher must not race the replacement export.
    local previous = retained[project.pdf]
    if previous then previous('Typst source map replaced') end
    monitor = assert(vim.uv.new_timer())
    monitor:start(200, 200, function()
      vim.schedule(function()
        if stopped or not generation then return end
        local g = generation
        local valid, changed = pcall(function()
          return not unchanged(g.inputs)
            or not vim.deep_equal(g.ready and published or g.before, revision(project.pdf))
        end)
        if valid and changed then refresh() end
      end)
    end)
    launch()
  end)
  if not ok then stop('Typst source navigation: ' .. tostring(reason)) end
  return function()
    if not completed then stop('navigation cancelled') end
  end
end

-- A different builder must be able to take ownership of the same PDF path.
function M.release(pdf)
  local stop = retained[canonical(pdf, vim.fn.getcwd())]
  if stop then stop('Typst source-map ownership released') end
end

vim.api.nvim_create_autocmd('VimLeavePre', {
  callback = function()
    for finish in pairs(active) do finish('Neovim is exiting') end
    vim.wait(2000, function() return next(closing) == nil end, 10)
  end,
})

return M
