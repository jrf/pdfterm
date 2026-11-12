-- Public editor API. Socket framing and project jobs have separate owners.
local M = {}
local socket = require('pdfterm.socket')
local project = require('pdfterm.project')
local terminal = require('pdfterm.terminal')
local saved_source = require('pdfterm.source')
local root = terminal.root
local options, main_file, initialized, exiting, paired
local cancel_forward, cancel_focus, cancel_resolution, close_listener
local owned_splits, launch_waiters = {}, nil
local viewer_handle, viewer_token
local launch_process
local setup_options, config_waiters, install_mappings
local point_namespace = vim.api.nvim_create_namespace('pdfterm.forward-intent')
local forward_point

local function remote_session()
  return ((vim.env.SSH_CONNECTION or '') ~= '' or (vim.env.SSH_TTY or '') ~= '')
    and (vim.env.PDFTERM_LAUNCH_SOCKET or '') == ''
end

local function log_path()
  local path = (options or setup_options) and (options or setup_options).log_file
  if path == vim.NIL or not path or path == '' then
    return nil
  end
  if path:sub(1, 2) == '~/' then
    local home = vim.uv.os_homedir() or vim.env.HOME or '~'
    return home .. path:sub(2)
  end
  return path
end

local function log_entry(level, message, context)
  local path = log_path()
  if not path then
    return
  end
  local timestamp = os.date('%Y-%m-%d %H:%M:%S')
  local line
  if context and context ~= '' then
    line = string.format('[%s] [%s] [%s] %s\n', timestamp, level, context, message)
  else
    line = string.format('[%s] [%s] %s\n', timestamp, level, message)
  end
  local ok, failure = pcall(function()
    local dir = vim.fs.dirname(path)
    if dir and dir ~= '' and vim.fn.isdirectory(dir) == 0 then
      assert(vim.fn.mkdir(dir, 'p', '0700') == 1, 'cannot create log directory ' .. dir)
    end
    local uv, c = vim.uv, vim.uv.constants
    local existing = uv.fs_lstat(path)
    assert(not existing or existing.type == 'file', 'log path is not a regular file')
    local flags = existing and bit.bor(c.O_WRONLY, c.O_APPEND, c.O_NONBLOCK or 0)
      or bit.bor(c.O_WRONLY, c.O_CREAT, c.O_EXCL, c.O_APPEND)
    local fd = assert(uv.fs_open(path, flags, 384))
    local ok_io, io_err = pcall(function()
      local st = assert(uv.fs_fstat(fd))
      assert(st.type == 'file' and st.uid == uv.getuid(), 'log file must be a user-owned regular file')
      assert(not existing or (st.dev == existing.dev and st.ino == existing.ino), 'log path changed during open')
      assert(uv.fs_fchmod(fd, 384))
      local offset = 0
      while offset < #line do
        local n = assert(uv.fs_write(fd, line:sub(offset + 1), -1))
        assert(n > 0, 'short log write')
        offset = offset + n
      end
    end)
    local closed, close_error = uv.fs_close(fd)
    assert(ok_io, io_err)
    assert(closed, close_error)
  end)
  if not ok then
    -- Logging failures must not recurse through notify(), which writes this log.
    vim.notify('pdfterm: cannot append log ' .. path .. ': ' .. tostring(failure), vim.log.levels.ERROR)
  end
end

M.log_path = log_path
M.log_entry = log_entry

local function notify(message, context)
  local text = message and tostring(message):gsub('%c', ' ') or ''
  if text ~= '' then
    log_entry('ERROR', text, context)
    vim.notify('pdfterm: ' .. text, vim.log.levels.ERROR)
  end
end

local function warn(message, context)
  local text = message and tostring(message):gsub('%c', ' ') or ''
  if text ~= '' then
    log_entry('WARN', text, context)
    vim.notify('pdfterm: ' .. text, vim.log.levels.WARN)
  end
end
local function alive(id)
  return not exiting and project.current(id)
end
local function release_point(point)
  if point and vim.api.nvim_buf_is_valid(point.buffer) then
    vim.api.nvim_buf_del_extmark(point.buffer, point_namespace, point.mark)
  end
  if point == forward_point then forward_point = nil end
end
local function intent()
  release_point(forward_point)
  local id = project.next()
  if cancel_forward then
    cancel_forward()
    cancel_forward = nil
  end
  if cancel_focus then
    cancel_focus()
    cancel_focus = nil
  end
  if cancel_resolution then
    cancel_resolution()
    cancel_resolution = nil
  end
  return id
end
local function with_source(id, source, callback, force)
  local config = options or setup_options or {}
  local attach_focus = not force and config.attach_only and config.focus_on_inverse and not remote_session()
  local done = false
  local function captured(error, handle)
    if not done and alive(id) then
      done = true
      callback(handle or (attach_focus and M._source_terminal or nil), error)
    end
  end
  if source or remote_session() or (not force and config.attach_only and not attach_focus) then
    captured(not source and remote_session() and 'plain SSH has no terminal focus bridge' or nil, source)
    return
  end
  local ok, error = pcall(terminal.capture_source, captured)
  if not ok then
    captured(tostring(error))
  end
end
local function command(arguments)
  local argv = { options.executable }
  if options.session then
    vim.list_extend(argv, { '--session', options.session })
  end
  return vim.list_extend(argv, arguments)
end
local function describe(main, current_source)
  return project.describe(options.project, main_file or main, not main_file and current_source or nil)
end


local function warning_text(...)
  local values, seen = {}, {}
  for index = 1, select('#', ...) do
    local message = select(index, ...)
    message = message and tostring(message):gsub('%c', ' ') or nil
    if message and message ~= '' and not seen[message] then
      seen[message], values[#values + 1] = true, message
    end
  end
  if #values > 0 then return vim.fn.strcharpart(table.concat(values, '; '), 0, 1024) end
end

local routine_diagnostics = {
  'assuming saved source is unchanged since compilation',
  'best-guess navigation: assuming saved source is unchanged since compilation',
  'best-guess navigation: using saved source and the compiler line anchor',
  'best-guess navigation: no literal source position was qualified; using the compiler line anchor',
  'line-only navigation: using the SyncTeX line anchor',
}

local function split_navigation_diagnostic(message)
  if not message or message == '' then return nil, nil end
  local remaining, info = message, {}
  for _, pattern in ipairs(routine_diagnostics) do
    local search_from = 1
    while true do
      local start, finish = remaining:find(pattern, search_from, true)
      if not start then break end
      local before_ok = start == 1 or remaining:sub(start - 2, start - 1) == '; '
      local after_ok = finish == #remaining or remaining:sub(finish + 1, finish + 2) == '; '
      if before_ok and after_ok then
        info[#info + 1] = pattern
        local before = remaining:sub(1, start - 1):gsub(';%s*$', '')
        local after = remaining:sub(finish + 1):gsub('^;%s*', '')
        remaining = (before ~= '' and after ~= '') and (before .. '; ' .. after) or (before .. after)
        search_from = 1
      else
        search_from = finish + 1
      end
    end
  end
  remaining = vim.trim(remaining)
  return (remaining ~= '' and remaining or nil), warning_text(unpack(info))
end

local function warnings(...)
  local message = warning_text(...)
  if message then
    warn(message)
  end
  return message
end
local function snapshot_point(point)
  if not point or point.buffer == nil then return end
  local buffer = point.buffer
  local valid = vim.api.nvim_buf_is_valid(buffer)
  local loaded = valid and vim.api.nvim_buf_is_loaded(buffer)
  local position = valid
    and vim.api.nvim_buf_get_extmark_by_id(buffer, point_namespace, point.mark, { details = true }) or {}
  local row, byte = position[1] or point.row, position[2] or point.column
  point.saved_row, point.saved_byte = row, byte
  point.saved_line = loaded and vim.api.nvim_buf_get_lines(buffer, row, row + 1, false)[1] or nil
  point.saved_coarse = point.boundary or not loaded or #position == 0 or position[3].invalid
end

local function current_source(buffer)
  if vim.api.nvim_buf_is_valid(buffer) and vim.api.nvim_buf_is_loaded(buffer) then
    return table.concat(vim.api.nvim_buf_get_lines(buffer, 0, -1, false), '\n')
  end
end

local function save_buffer(buffer)
  if vim.api.nvim_buf_is_valid(buffer) and vim.api.nvim_buf_is_loaded(buffer) then
    local name = vim.api.nvim_buf_get_name(buffer)
    if vim.bo[buffer].modified or not vim.uv.fs_stat(name) then
      local point = forward_point and forward_point.buffer == buffer and forward_point or nil
      local pre_tick = vim.api.nvim_buf_get_changedtick(buffer)
      local write_tick
      if point then snapshot_point(point) end
      local hook = vim.api.nvim_create_autocmd('BufWritePre', {
        buffer = buffer,
        once = true,
        callback = function()
          if point then snapshot_point(point) end
          write_tick = vim.api.nvim_buf_is_valid(buffer) and vim.api.nvim_buf_get_changedtick(buffer) or nil
        end,
      })
      local ok, err = pcall(vim.api.nvim_buf_call, buffer, function() vim.cmd('write') end)
      pcall(vim.api.nvim_del_autocmd, hook)
      if not ok then error(err, 0) end
      local valid = vim.api.nvim_buf_is_valid(buffer) and vim.api.nvim_buf_is_loaded(buffer)
      local after_tick = valid and vim.api.nvim_buf_get_changedtick(buffer) or nil
      if point then
        if not valid then
          snapshot_point(point)
        elseif write_tick and after_tick > write_tick + 1 then
          local disk = saved_source.read_input(name)
          if disk and disk:gsub('\r\n', '\n'):gsub('\n$', '') == (current_source(buffer) or '') then
            snapshot_point(point)
          end
        end
      end
      return not valid or not write_tick or write_tick ~= pre_tick
        or after_tick > write_tick + 1 or vim.bo[buffer].modified
    end
    return false
  else
    warnings('Source buffer unloaded during navigation; using saved inputs and a best-guess anchor.')
    return false
  end
end

local function with_saved_project(buffer, file, id, callback)
  if not alive(id) then return end
  local valid = vim.api.nvim_buf_is_valid(buffer)
  local tick = valid and vim.api.nvim_buf_get_changedtick(buffer) or nil
  local typst = file:lower():sub(-4) == '.typ'
  local fenc = valid and vim.bo[buffer].fileencoding:lower() or ''
  local non_utf8 = fenc ~= '' and fenc ~= 'utf-8'
  local text = not typst and valid
    and (vim.bo[buffer].modified or non_utf8 or not vim.uv.fs_stat(file))
    and current_source(buffer) or nil
  local p = describe(file, text)
  local mutated = save_buffer(buffer)
  if not alive(id) then return end
  local after_tick = vim.api.nvim_buf_is_valid(buffer) and vim.api.nvim_buf_get_changedtick(buffer) or nil
  if not typst and (mutated or (text == nil and after_tick ~= tick)) then
    local saved = (non_utf8 or not vim.uv.fs_stat(file)) and current_source(buffer) or nil
    if saved == nil or saved ~= text then
      assert(describe(file, saved).main == p.main,
        'pdfterm: source root changed while saving; repeat the command')
    end
  end
  callback(p)
end
local function inverse(location)
  if exiting then
    return
  end
  local buffer = vim.fn.bufadd(location.file)
  local window = vim.fn.win_findbuf(buffer)[1]
  if window then
    vim.api.nvim_set_current_win(window)
  else
    vim.cmd('hide buffer ' .. buffer)
  end
  assert(vim.api.nvim_get_current_buf() == buffer, 'pdfterm: source activation selected a different buffer')
  local total_lines = vim.api.nvim_buf_line_count(0)
  local line = math.max(1, math.min(location.line, total_lines))
  local text = vim.api.nvim_buf_get_lines(0, line - 1, line, false)[1] or ''
  vim.api.nvim_win_set_cursor(0, { line, math.min(location.byte_column, #text) })
  local boundary_warn = (location.line < 1 or location.line > total_lines)
    and string.format('inverse target line %d is outside buffer bounds (1..%d)', location.line, total_lines)
    or nil
  local warn_msg = warning_text(location.warning, boundary_warn)
  local context = string.format(
    'inverse: %s:%d:%d (precise=%s)',
    location.file,
    line,
    location.byte_column,
    tostring(location.precise == true)
  )
  local warn_part, info_part = split_navigation_diagnostic(warn_msg)
  if warn_part then
    warn(warn_msg, context)
  elseif info_part then
    log_entry('INFO', info_part, context)
  elseif location.precise == false then
    log_entry('INFO', 'imprecise/coarse line positioning', context)
  else
    log_entry('INFO', 'position matched', context)
  end
  vim.cmd('normal! zvzz')
  if (options or setup_options).focus_on_inverse then
    if remote_session() or not M._source_terminal then
      notify(
        'could not focus source terminal: '
          .. (remote_session() and 'plain SSH has no terminal focus bridge'
            or 'source terminal unavailable')
      )
    else
      terminal.focus(
        M._source_terminal,
        vim.schedule_wrap(function(result)
          if result.code ~= 0 then
            notify('could not focus source terminal: ' .. (result.stderr or ''))
          end
        end)
      )
    end
  end
end
M._inverse = inverse

-- Configuration may load in the background for TOML keybindings, never a listener.
-- Failed configuration can be retried by the next action without breaking setup.
local function configure(callback)
  if options then
    if callback then
      callback()
    end
    return
  end
  if config_waiters then
    if callback then
      config_waiters[#config_waiters + 1] = callback
    end
    return
  end
  config_waiters = callback and { callback } or {}
  local release = root .. '/target/release/pdfterm'
  local on_path = vim.fn.exepath('pdfterm')
  local executable = setup_options.executable
    or (vim.fn.executable(release) == 1 and release or (on_path ~= '' and on_path or release))
  project.run(
    { executable, '--print-config', '--session', setup_options.session },
    nil,
    10000,
    function(result)
      if exiting then
        return
      end
      local waiters = config_waiters
      config_waiters = nil
      local ok, error = pcall(function()
        assert(result.code == 0, 'configuration: ' .. result.stderr)
        local config = vim.json.decode(result.stdout)
        options = vim.tbl_extend('force', config.nvim, setup_options, {
          executable = setup_options.executable
            or (config.nvim.executable ~= '' and config.nvim.executable or executable),
          editor = config.editor,
          forward_socket = config.forward_socket,
        })
        install_mappings()
      end)
      if not ok then
        options = nil
        notify(error)
        return
      end
      for _, waiter in ipairs(waiters) do
        waiter()
      end
    end
  )
end

local function ready(callback, listen)
  if not initialized then
    notify('call setup() first')
    return
  end
  configure(function()
    if exiting then
      return
    end
    if listen and not close_listener and options.editor.transport == 'socket' then
      local ok, result = pcall(socket.listen, options.editor.path, inverse)
      if not ok then
        socket.diagnose(options.editor.path, function(status)
          if not exiting then
            notify(tostring(result) .. '; ' .. status)
          end
        end)
        return
      end
      close_listener = result
    end
    if listen then
      paired = true
    end
    local ok, error = pcall(callback)
    if not ok then
      notify(error)
    end
  end)
end

local function viewer_command(pdf)
  local argv = command({ pdf, '--synctex-best-guess' })
  if options.focus_on_forward then
    argv = vim.list_extend({ root .. '/scripts/pdfterm-viewer' }, argv)
  end
  if vim.env.XDG_CONFIG_HOME then
    argv = vim.list_extend({ 'env', 'XDG_CONFIG_HOME=' .. vim.env.XDG_CONFIG_HOME }, argv)
  end
  return terminal.shell_join(argv)
end

local function launch(source, source_error, callback)
  if remote_session() or options.attach_only then
    callback(
      'viewer unavailable; run on this machine in another terminal: '
        .. viewer_command(M._launch_pdf)
    )
    return
  end
  if launch_waiters then
    launch_waiters[#launch_waiters + 1] = callback
    return
  end
  launch_waiters = { callback }
  local focus_token
  local function complete(error, split)
    local waiters = launch_waiters or {}
    launch_waiters = nil
    for _, waiter in ipairs(waiters) do
      waiter(error, split, focus_token)
    end
  end
  if not source then
    complete(source_error or 'source terminal unavailable')
    return
  end
  focus_token = vim.fn.sha256(assert(vim.uv.random(16))):sub(1, 32)
  local ok, process = pcall(
    terminal.launch_split,
    source,
    options.executable,
    M._launch_pdf,
    function(result, split)
      launch_process = nil
      if split then
        owned_splits[#owned_splits + 1] = {
          handle = split,
          token = focus_token,
          unclosed = result.unclosed == true,
          ready = false,
        }
      end
      vim.schedule(function()
        if exiting then
          complete('editor stopped')
        elseif result.code ~= 0 or not split then
          complete('terminal split failed: ' .. (result.stderr or 'missing ID'))
        else
          complete(nil, split)
        end
      end)
    end,
    options.session,
    focus_token
  )
  if ok then
    launch_process = process
  else
    complete(tostring(process))
  end
end

local function deliver(pdf, payload, id, source, source_error, allow_launch, focus_forward)
  assert(initialized, 'pdfterm: call setup() first')
  if not alive(id) then
    return
  end
  if options.forward_socket == '' then
    notify('forward_socket is disabled')
    return
  end
  M._source_terminal = source
  local launched, launched_viewer, launched_token, retry_deadline = false, nil, nil, nil
  local attempt
  attempt = function()
    if not alive(id) then
      return
    end
    cancel_forward = socket.request(
      options.forward_socket,
      payload,
      function(error, connection_error, reply)
        if not alive(id) then
          return
        end
        cancel_forward = nil
        if not error then
          local valid_token = type(reply.viewer_token) == 'string'
            and reply.viewer_token:match('^[0-9a-fA-F]+$')
            and #reply.viewer_token == 32
          local matching_launch = launched_viewer
            and launched_token
            and reply.viewer_token == launched_token
          if matching_launch then
            for _, split in ipairs(owned_splits) do
              if split.handle == launched_viewer then
                split.ready = true
              end
            end
            viewer_handle, viewer_token = launched_viewer, launched_token
          end
          if focus_forward and options.focus_on_forward then
            if launched_viewer and not matching_launch then
              notify('navigation succeeded; viewer focus unavailable: no matching plugin-owned viewer')
            elseif matching_launch or (viewer_handle and reply.viewer_token == viewer_token) then
              terminal.focus(
                matching_launch and launched_viewer or viewer_handle,
                vim.schedule_wrap(function(result)
                  if alive(id) and result.code ~= 0 then
                    notify('could not focus viewer terminal: ' .. (result.stderr or ''))
                  end
                end)
              )
            elseif valid_token then
              cancel_focus = socket.request(
                options.forward_socket,
                vim.json.encode({ type = 'focus', viewer_token = reply.viewer_token }),
                function(focus_error)
                  if not alive(id) then
                    return
                  end
                  cancel_focus = nil
                  if focus_error then
                    notify('navigation succeeded; could not focus viewer: ' .. focus_error)
                  end
                end
              )
            else
              notify('navigation succeeded; viewer focus unavailable: invalid or missing viewer token')
            end
          end
          if options.focus_on_inverse and not remote_session() and not source then
            notify(
              'navigation succeeded; focus unavailable: '
                .. (source_error or 'source terminal unavailable')
            )
          end
          return
        end
        if not connection_error or not (error:match('ENOENT') or error:match('ECONNREFUSED')) then
          notify(error)
          return
        end
        if not allow_launch then
          notify(
            options.attach_only
                and 'viewer unavailable; attach_only requires an existing viewer (use :PdfTermViewerCommand for manual pairing)'
              or remote_session() and 'viewer unavailable; use :PdfTermViewerCommand for manual pairing'
              or 'viewer unavailable; use :PdfTermForwardSplit to launch one or :PdfTermViewerCommand for manual pairing'
          )
          return
        end
        if not launched then
          launched = true
          M._launch_pdf = pdf
          launch(source, source_error, function(launch_error, split, token)
            if not alive(id) then
              return
            end
            if launch_error then
              notify(launch_error)
            else
              launched_viewer, launched_token = split, token
              retry_deadline = vim.uv.hrtime() + 5e9
              attempt()
            end
          end)
        else
          if vim.uv.hrtime() >= retry_deadline then
            notify('viewer did not open its forward socket')
          else
            vim.defer_fn(attempt, 20)
          end
        end
      end
    )
  end
  attempt()
end

function M.forward_search(pdf, payload, source)
  local id = intent()
  with_source(id, source, function(captured, capture_error)
    ready(function()
      if alive(id) then
        deliver(pdf, payload, id, captured, capture_error, false, true)
      end
    end, true)
  end)
end

-- Open a standalone PDF at page one, without TeX or a SyncTeX sidecar.
local function open_pdf(pdf, id, source, source_error, allow_launch, focus_forward)
  pdf = vim.fn.fnamemodify(pdf or vim.api.nvim_buf_get_name(0), ':p')
  ready(function()
    if not alive(id) then
      return
    end
    local path = assert(vim.uv.fs_realpath(pdf))
    local stat = assert(vim.uv.fs_stat(path))
    assert(stat.type == 'file', 'PDF is not a regular file')
    local payload = vim.json.encode({
      pdf = path,
      revision = saved_source.saved_revision(stat),
      page = 1, h = 0, v = 0, width = 0, height = 0,
      best_guess = true,
    })
    deliver(path, payload, id, source, source_error, allow_launch, focus_forward)
  end, true)
end

function M.open(pdf)
  local id = intent()
  pdf = vim.fn.fnamemodify(pdf or vim.api.nvim_buf_get_name(0), ':p')
  with_source(id, nil, function(source, source_error)
    open_pdf(pdf, id, source, source_error, true, false)
  end)
end
local function release_typst(pdf)
  local compiler = package.loaded['pdfterm.typst']
  if compiler then compiler.release(pdf) end
end

function M.set_main(file)
  file = file or vim.api.nvim_buf_get_name(0)
  intent()
  ready(function()
    assert(file:match('%.tex$') or file:match('%.typ$'), 'main document must be a TeX or Typst file')
    main_file = vim.fn.fnamemodify(file, ':p')
    options.project = vim.tbl_extend('force', options.project or {}, { main = main_file })
    release_typst(describe(main_file).pdf)
    vim.notify('Set current main file to ' .. main_file)
  end)
end
function M.toggle_compile()
  ready(function()
    options.compile = not options.compile
    vim.notify('Compile flag is now: ' .. tostring(options.compile))
  end)
end
function M.viewer_command(pdf)
  local id = intent()
  local source = vim.api.nvim_buf_get_name(0)
  M._source_terminal = nil
  with_source(id, nil, function(captured_source, source_error)
    ready(function()
      if not alive(id) then
        return
      end
      M._source_terminal = captured_source
      local path = pdf and vim.fn.fnamemodify(pdf, ':p') or describe(source).pdf
      local shell_command = viewer_command(path)
      vim.fn.setreg('+', shell_command)
      vim.notify(shell_command)
      if options.focus_on_inverse and not captured_source then
        notify('source terminal focus unavailable: ' .. (source_error or 'source terminal unavailable'))
      end
    end, true)
  end, true)
end
function M.build()
  local id, buffer = intent(), vim.api.nvim_get_current_buf()
  local file = vim.api.nvim_buf_get_name(buffer)
  ready(function()
    with_saved_project(buffer, file, id, function(p)
      if not p.main:match('%.typ$') then
        release_typst(p.pdf)
        project.build(p, id)
      elseif not require('pdfterm.typst').build(p, id) then
        project.build(p, id)
      end
    end)
  end)
end
local function forward(allow_launch)
  if type(allow_launch) ~= 'boolean' then
    allow_launch = true
  end
  local id = intent()
  local buffer, cursor = vim.api.nvim_get_current_buf(), vim.api.nvim_win_get_cursor(0)
  local file = vim.api.nvim_buf_get_name(buffer)
  local text = vim.api.nvim_buf_get_lines(buffer, cursor[1] - 1, cursor[1], false)[1] or ''
  local character = vim.fn.strcharpart(text, vim.fn.strchars(text:sub(1, cursor[2])), 1)
  local point = {
    buffer = buffer, row = cursor[1] - 1, column = cursor[2],
    boundary = character == '',
    mark = vim.api.nvim_buf_set_extmark(buffer, point_namespace, cursor[1] - 1, cursor[2], {
      end_row = cursor[1] - 1, end_col = math.min(#text, cursor[2] + #character),
      right_gravity = true, end_right_gravity = false, undo_restore = true, invalidate = true,
    }),
  }
  forward_point = point
  with_source(id, nil, function(source, source_error)
    local function navigate(p)
      if not p.main:match('%.typ$') then release_typst(p.pdf) end
      -- Freeze saved-source coordinates before a build can outlive unsaved edits.
      if point.saved_row == nil then snapshot_point(point) end
      local row, byte = point.saved_row, point.saved_byte
      local line_text, coarse = point.saved_line, point.saved_coarse
      local column = line_text and vim.fn.strchars(line_text:sub(1, byte)) + 1
      local source_warning = coarse and (point.boundary
        and 'Source cursor is a line boundary; using a best-guess compiled line.'
        or 'Source cursor anchor was deleted or unloaded before navigation; using a best-guess compiled line.')
      local context = string.format('forward: %s:%d:%d', file, row + 1, byte + 1)
      local fallback = allow_launch
        and 'Source navigation unavailable; opening PDF at page 1 without source positioning.'
        or 'Source navigation unavailable; PDF position unchanged.'
      local function resolve()
        if not alive(id) then return end
        local function resolved(result)
          if not alive(id) then return end
          cancel_resolution = nil
          release_point(point)
          if result.code ~= 0 then
            if result.code == 3 then
              local text = warning_text(fallback, (vim.trim(result.stderr):gsub('^pdfterm:%s*', '')))
              warn(text or fallback, context)
              if allow_launch then
                open_pdf(p.pdf, id, source, source_error, true, true)
              end
            else
              notify(vim.trim(result.stderr), context)
            end
            return
          end
          local payload = vim.json.decode(result.stdout)
          if source_warning and not p.main:match('%.typ$') then
            payload.warning = warning_text(source_warning, payload.warning)
          end
          if not vim.api.nvim_buf_is_valid(buffer) or vim.api.nvim_buf_get_name(buffer) ~= file then
            payload.word = vim.NIL
            payload.warning = warning_text(payload.warning, 'Source identity changed during navigation; using the compiler line anchor.')
          end
          local page_info = payload.page and string.format(' (page %s)', payload.page) or ''
          local warn_part, info_part = split_navigation_diagnostic(payload.warning)
          if warn_part then
            warn(payload.warning, context)
          elseif info_part then
            log_entry('INFO', 'position resolved' .. page_info .. ' (' .. info_part .. ')', context)
          else
            log_entry('INFO', 'position resolved' .. page_info, context)
          end
          deliver(p.pdf, vim.json.encode(payload), id, source, source_error, allow_launch, true)
        end
        if p.main:match('%.typ$') then
          project.after_build(p, function()
            if alive(id) then
              cancel_resolution = require('pdfterm.typst').resolve(p, file, row + 1, byte, resolved)
            else
              release_point(point)
            end
          end)
        else
          if not column then
            release_point(point)
            warn(fallback, context)
            if allow_launch then
              open_pdf(p.pdf, id, source, source_error, true, true)
            end
            return
          end
          local argv = command({
            p.pdf, '--synctex-view', file, '--line', tostring(row + 1),
            '--column', tostring(column), '--synctex-best-guess',
          })
          if coarse then argv[#argv + 1] = '--synctex-anchor' end
          cancel_resolution = project.run(argv, p.cwd, 11000, resolved)
        end
      end
      if options.compile and not p.main:match('%.typ$') then
        project.build(p, id, function(result)
          if result.code == 0 then
            resolve()
          else
            release_point(point)
          end
        end)
      else
        resolve()
      end
    end
    ready(function()
      with_saved_project(buffer, file, id, navigate)
    end, true)
  end)
end
function M.forward(allow_launch)
  forward(allow_launch)
end
function M.forward_split()
  forward(true)
end
function M.show_log()
  if not initialized then
    notify('call setup() first')
    return
  end
  ready(function()
    local path = log_path()
    if not path then
      vim.notify('pdfterm: no log_file configured', vim.log.levels.WARN)
      return
    end
    if vim.fn.filereadable(path) == 0 then
      vim.notify('pdfterm: log file does not exist: ' .. path, vim.log.levels.INFO)
      return
    end
    vim.cmd('split ' .. vim.fn.fnameescape(path))
  end)
end

function M.setup(opts)
  if initialized then
    return
  end
  setup_options = vim.deepcopy(opts or {})
  setup_options.session = setup_options.session
    or (
      'n'
      .. vim.fn
        .sha256(
          tostring(vim.fn.getpid())
            .. ':'
            .. tostring(vim.uv.hrtime())
            .. ':'
            .. tostring(os.time())
        )
        :sub(1, 12)
    )
  initialized, exiting, paired = true, false, false
  local function map(key, callback, extra)
    if key and key ~= '' then
      vim.keymap.set('n', key, callback, extra)
    end
  end
  install_mappings = function()
    local keys = (options or setup_options).keys or {}
    map(keys.forward, M.forward, { desc = 'pdfterm forward search' })
    map(keys.main_file, M.set_main, { desc = 'pdfterm set main document' })
    map(keys.compile, M.toggle_compile, { desc = 'pdfterm toggle compilation' })
    vim.api.nvim_clear_autocmds({ group = 'pdfterm', event = 'FileType' })
    local function build_map(buffer)
      map(keys.build, M.build, { buffer = buffer, desc = 'pdfterm build document' })
    end
    vim.api.nvim_create_autocmd('FileType', {
      group = 'pdfterm',
      pattern = { 'tex', 'latex', 'typst' },
      callback = function(event)
        build_map(event.buf)
      end,
    })
    for _, buffer in ipairs(vim.api.nvim_list_bufs()) do
      if vim.bo[buffer].filetype == 'tex' or vim.bo[buffer].filetype == 'latex'
        or vim.bo[buffer].filetype == 'typst' then
        build_map(buffer)
      end
    end
  end
  local group = vim.api.nvim_create_augroup('pdfterm', { clear = true })
  install_mappings()
  vim.api.nvim_create_user_command('PdfTermOpen', function(args)
    M.open(args.args ~= '' and args.args or nil)
  end, { nargs = '?', complete = 'file' })
  if setup_options.open_pdf then
    vim.api.nvim_create_autocmd('BufReadCmd', {
      group = group,
      pattern = '*.[pP][dD][fF]',
      callback = function(event)
        local pdf = vim.api.nvim_buf_get_name(event.buf)
        vim.bo[event.buf].buftype = 'nofile'
        vim.bo[event.buf].modifiable = true
        vim.bo[event.buf].swapfile = false
        vim.api.nvim_buf_set_lines(event.buf, 0, -1, false, {
          'pdfterm',
          pdf,
          '',
          'Press Enter to open or retry.',
        })
        vim.bo[event.buf].modified = false
        vim.bo[event.buf].modifiable = false
        local function open()
          M.open(pdf)
        end
        vim.keymap.set('n', '<CR>', open, { buffer = event.buf, desc = 'Open PDF with pdfterm' })
        open()
      end,
    })
  end
  vim.api.nvim_create_user_command('PdfTermForward', M.forward, {})
  vim.api.nvim_create_user_command('PdfTermForwardSplit', M.forward_split, {})
  vim.api.nvim_create_user_command('PdfTermBuild', M.build, {})
  vim.api.nvim_create_user_command('PdfTermMain', function(args)
    M.set_main(args.args ~= '' and args.args or nil)
  end, { nargs = '?', complete = 'file' })
  vim.api.nvim_create_user_command('PdfTermCompile', M.toggle_compile, {})
  vim.api.nvim_create_user_command('PdfTermViewerCommand', function(args)
    M.viewer_command(args.args ~= '' and args.args or nil)
  end, { nargs = '?', complete = 'file' })
  vim.api.nvim_create_user_command('PdfTermLog', M.show_log, {})
  vim.api.nvim_create_autocmd('VimLeavePre', {
    group = group,
    once = true,
    callback = function()
      exiting = true
      intent()
      project.close()
      if close_listener then
        close_listener()
      end
      if launch_process then
        local ok, error = pcall(launch_process.wait, launch_process)
        if not ok then
          notify('waiting for terminal launch: ' .. tostring(error))
        end
      end
      local quit_ok, quit_absent, quit_token = false, false, nil
      if (paired or #owned_splits > 0) and options and (options.forward_socket or '') ~= '' then
        local done = false
        local cancel_quit = socket.request(
          options.forward_socket,
          vim.json.encode({ type = 'quit' }),
          function(error, connection_error, reply)
            if not error then
              quit_ok = true
              quit_token = type(reply) == 'table' and reply.viewer_token or nil
            elseif connection_error and (error:match('ENOENT') or error:match('ECONNREFUSED')) then
              quit_absent = true
            else
              notify(error)
            end
            done = true
          end,
          200
        )
        if not vim.wait(250, function() return done end, 1) and cancel_quit then
          cancel_quit()
        end
      end
      for _, split in ipairs(owned_splits) do
        local matched_quit = quit_ok and split.token and quit_token == split.token
        -- Ghostty (locally or behind pdfterm-ssh) retains scripted command surfaces after quit.
        if split.handle.kind == 'ghostty' or split.handle.kind == 'ssh' or split.unclosed or not (matched_quit or (split.ready and quit_absent)) then
          local ok, error = pcall(terminal.close, split.handle)
          if not ok then
            notify(error)
          end
        end
      end
    end,
  })
  configure()
end
return M
