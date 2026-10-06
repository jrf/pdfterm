-- Public editor API. Socket framing and project jobs have separate owners.
local M = {}
local socket = require('pdfterm.socket')
local project = require('pdfterm.project')
local terminal = require('pdfterm.terminal')
local root = vim.fn.fnamemodify(debug.getinfo(1, 'S').source:sub(2), ':h:h:h')
local options, main_file, initialized, exiting
local cancel_forward, cancel_focus, cancel_resolution, close_listener
local owned_splits, launch_waiters = {}, nil
local viewer_handle, viewer_token
local launch_process
local setup_options, config_waiters, install_mappings

local function remote_session()
  return ((vim.env.SSH_CONNECTION or '') ~= '' or (vim.env.SSH_TTY or '') ~= '')
    and (vim.env.PDFTERM_LAUNCH_SOCKET or '') == ''
end

local function notify(message)
  vim.notify('pdfterm: ' .. tostring(message), vim.log.levels.ERROR)
end
local function alive(id)
  return not exiting and project.current(id)
end
local function intent()
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
local function with_source(id, source, callback)
  local config = options or setup_options or {}
  local attach_focus = config.attach_only and config.focus_on_inverse and not remote_session()
  local function captured(error, handle)
    if alive(id) then
      callback(handle or (attach_focus and M._source_terminal or nil), error)
    end
  end
  if source or remote_session() or (config.attach_only and not attach_focus) then
    captured(nil, source)
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
local function inverse(location)
  if exiting then
    return
  end
  local buffer = vim.fn.bufnr(location.file)
  local window = buffer >= 0 and vim.fn.win_findbuf(buffer)[1] or nil
  if window then
    vim.api.nvim_set_current_win(window)
  else
    vim.cmd.edit(vim.fn.fnameescape(location.file))
  end
  local line = math.max(1, math.min(location.line, vim.api.nvim_buf_line_count(0)))
  local text = vim.api.nvim_buf_get_lines(0, line - 1, line, false)[1] or ''
  vim.api.nvim_win_set_cursor(0, { line, math.min(location.byte_column, #text) })
  vim.cmd('normal! zvzz')
  if options.focus_on_inverse then
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
  local executable = setup_options.executable or root .. '/target/release/pdfterm'
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
    local ok, error = pcall(callback)
    if not ok then
      notify(error)
    end
  end)
end

local function viewer_command(pdf)
  local argv = command({ pdf })
  if options.focus_on_forward then
    argv = vim.list_extend({ root .. '/scripts/pdfterm-viewer' }, argv)
  end
  if vim.env.XDG_CONFIG_HOME then
    argv = vim.list_extend({ 'env', 'XDG_CONFIG_HOME=' .. vim.env.XDG_CONFIG_HOME }, argv)
  end
  return table.concat(vim.tbl_map(vim.fn.shellescape, argv), ' ')
end

local function launch(id, source, source_error, callback)
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
  if options.focus_on_forward then
    focus_token = vim.fn.sha256(assert(vim.uv.random(16))):sub(1, 32)
  end
  local ok, process = pcall(
    terminal.launch_split,
    source,
    options.executable,
    M._launch_pdf,
    function(result, split)
      launch_process = nil
      if split then
        owned_splits[#owned_splits + 1] = split
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
            options.attach_only and 'viewer unavailable; attach_only requires an existing viewer'
              or remote_session() and 'viewer unavailable; use :PdfTermViewerCommand for manual pairing'
              or 'viewer unavailable; use :PdfTermForwardSplit to launch one'
          )
          return
        end
        if not launched then
          launched = true
          M._launch_pdf = pdf
          launch(id, source, source_error, function(launch_error, split, token)
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
      revision = {
        device = stat.dev,
        inode = stat.ino,
        length = stat.size,
        modified_seconds = stat.mtime.sec,
        modified_nanoseconds = stat.mtime.nsec,
        changed_seconds = stat.ctime.sec,
        changed_nanoseconds = stat.ctime.nsec,
      },
      page = 1,
      h = 0,
      v = 0,
      width = 0,
      height = 0,
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
    release_typst(project.describe(options.project, main_file).pdf)
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
  local captured_source, source_error, capture_done, configured = nil, nil, false, false
  local function publish()
    if not configured or not capture_done or not alive(id) then
      return
    end
    M._source_terminal = captured_source
    pdf = pdf and vim.fn.fnamemodify(pdf, ':p')
      or project.describe(options.project, main_file or source).pdf
    local shell_command = viewer_command(pdf)
    vim.fn.setreg('+', shell_command)
    vim.notify(shell_command)
    if options.focus_on_inverse and not captured_source then
      notify('source terminal focus unavailable: ' .. (source_error or 'source terminal unavailable'))
    end
  end
  local function capture(error, handle)
    if not alive(id) or capture_done then
      return
    end
    captured_source, source_error, capture_done = handle, error, true
    publish()
  end
  if remote_session() then
    capture('plain SSH has no terminal focus bridge')
  else
    local ok, error = pcall(terminal.capture_source, capture)
    if not ok then
      capture(tostring(error))
    end
  end
  ready(function()
    configured = true
    publish()
  end, true)
end
function M.build()
  local id, file = intent(), vim.api.nvim_buf_get_name(0)
  vim.cmd('write')
  ready(function()
    if not alive(id) then
      return
    end
    local p = project.describe(options.project, main_file or file)
    if not p.main:match('%.typ$') then release_typst(p.pdf) end
    project.build(p, id)
  end)
end
local function forward(allow_launch)
  local id = intent() -- Before save, configuration, build, resolution, and delivery.
  local cursor = vim.api.nvim_win_get_cursor(0)
  local file = vim.api.nvim_buf_get_name(0)
  local column = vim.fn.strchars(vim.api.nvim_get_current_line():sub(1, cursor[2])) + 1
  vim.cmd('write')
  with_source(id, nil, function(source, source_error)
    ready(function()
      if not alive(id) then
        return
      end
      local p = project.describe(options.project, main_file or file)
      if not p.main:match('%.typ$') then release_typst(p.pdf) end
      local function resolve()
        if not alive(id) then
          return
        end
        local function resolved(result)
          if not alive(id) then
            return
          end
          cancel_resolution = nil
          if result.code ~= 0 then
            vim.notify(
              'pdfterm: Source navigation failed; opening PDF at page 1 without source positioning.\n'
                .. vim.trim(result.stderr):gsub('^pdfterm:%s*', ''),
              vim.log.levels.WARN
            )
            open_pdf(p.pdf, id, source, source_error, allow_launch, true)
            return
          end
          deliver(p.pdf, result.stdout, id, source, source_error, allow_launch, true)
        end
        if p.main:match('%.typ$') then
          cancel_resolution = require('pdfterm.typst').resolve(p, file, cursor[1], cursor[2], resolved)
        else
          cancel_resolution = project.run(
            command({
              p.pdf,
              '--synctex-view',
              file,
              '--line',
              tostring(cursor[1]),
              '--column',
              tostring(column),
            }),
            p.cwd,
            11000,
            resolved
          )
        end
      end
      if options.compile or p.main:match('%.typ$') then
        project.build(p, id, function(result)
          if result.code == 0 then
            resolve()
          end
        end)
      else
        resolve()
      end
    end, true)
  end)
end
function M.forward()
  forward(false)
end
function M.forward_split()
  forward(true)
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
  initialized, exiting = true, false
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
      for _, split in ipairs(owned_splits) do
        local ok, error = pcall(terminal.close, split)
        if not ok then
          notify(error)
        end
      end
    end,
  })
  configure()
end
return M
