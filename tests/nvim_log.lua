-- Run: PDFTERM_EXECUTABLE="$PWD/target/release/pdfterm" nvim --headless -u NONE -l tests/nvim_log.lua
local root = vim.fn.getcwd()
vim.opt.runtimepath:prepend(root)

local directory = assert(vim.uv.fs_mkdtemp(root .. '/.nvim-log-test-XXXXXX'))
vim.env.XDG_CACHE_HOME = directory .. '/cache'
local binary = vim.env.PDFTERM_EXECUTABLE
  or (vim.fn.executable(root .. '/target/debug/pdfterm') == 1 and (root .. '/target/debug/pdfterm'))
  or (vim.fn.executable(root .. '/target/release/pdfterm') == 1 and (root .. '/target/release/pdfterm'))
  or (vim.fn.exepath('pdfterm') ~= '' and vim.fn.exepath('pdfterm'))
  or (root .. '/target/release/pdfterm')
local log_file = directory .. '/logs/pdfterm.log'

local ok, failure = xpcall(function()
  local pdfterm = require('pdfterm')

  -- 1. Test log_entry directly with automatic parent directory creation
  assert(pdfterm.log_path() == nil, 'log_path before setup must be nil')
  pdfterm.log_entry('INFO', 'should not write because no path configured')
  assert(vim.fn.filereadable(log_file) == 0, 'file should not be created if unconfigured')

  -- 2. Setup with ~/log_file
  vim.env.HOME = directory
  pdfterm.setup({
    executable = binary,
    log_file = '~/logs/pdfterm.log',
    session = 'test-log-session',
  })

  assert(pdfterm.log_path() == log_file, 'log_path must expand ~/ to configured path')

  -- 3. Test log_entry writes formatted lines
  pdfterm.log_entry('INFO', 'hello world', 'test_ctx')
  assert(vim.fn.filereadable(log_file) == 1, 'log file must exist after log_entry')
  assert(bit.band(assert(vim.uv.fs_stat(vim.fs.dirname(log_file))).mode, 511) == 448)
  assert(bit.band(assert(vim.uv.fs_stat(log_file)).mode, 511) == 384)
  local lines = vim.fn.readfile(log_file)
  assert(#lines == 1, 'expected 1 log line, got: ' .. #lines)
  assert(lines[1]:match('%[%d%d%d%d%-%d%d%-%d%d %d%d:%d%d:%d%d%] %[INFO%] %[test_ctx%] hello world'),
    'unexpected line format: ' .. lines[1])

  -- 4. Test log entry without context
  pdfterm.log_entry('WARN', 'no context message')
  lines = vim.fn.readfile(log_file)
  assert(#lines == 2, 'expected 2 log lines')
  assert(lines[2]:match('%[%d%d%d%d%-%d%d%-%d%d %d%d:%d%d:%d%d%] %[WARN%] no context message'),
    'unexpected line format: ' .. lines[2])

  local notify, failures = vim.notify, {}
  vim.notify = function(message, level)
    failures[#failures + 1] = { message = message, level = level }
  end
  local saved_lines = vim.fn.readfile(log_file)
  assert(vim.fn.delete(log_file) == 0)
  assert(vim.fn.mkdir(log_file) == 1)
  pdfterm.log_entry('ERROR', 'must report an unopenable log')
  assert(#failures == 1 and failures[1].level == vim.log.levels.ERROR,
    'unopenable log must report one error without recursive logging')
  assert(failures[1].message:find(log_file, 1, true), 'log error must identify its path')
  assert(vim.fn.isdirectory(log_file) == 1, 'logging must not replace a directory')
  assert(vim.fn.delete(vim.fs.dirname(log_file), 'rf') == 0)
  vim.fn.writefile({ 'parent is a regular file' }, vim.fs.dirname(log_file))
  pdfterm.log_entry('ERROR', 'must report a failed parent directory')
  assert(#failures == 2 and failures[2].level == vim.log.levels.ERROR,
    'failed log directory creation must report one error')
  assert(vim.fn.readfile(vim.fs.dirname(log_file))[1] == 'parent is a regular file',
    'logging must not overwrite an obstructing file')
  assert(vim.fn.delete(vim.fs.dirname(log_file)) == 0)
  assert(vim.fn.mkdir(vim.fs.dirname(log_file)) == 1)
  vim.fn.writefile(saved_lines, log_file)
  vim.notify = notify

  -- 5. Test :PdfTermLog command opens a split buffer with the log file
  local initial_windows = vim.api.nvim_tabpage_list_wins(0)
  vim.cmd('PdfTermLog')
  assert(vim.wait(5000, function()
    return #vim.api.nvim_tabpage_list_wins(0) == #initial_windows + 1
  end, 10), 'PdfTermLog must open a split window')
  local current_buf = vim.api.nvim_get_current_buf()
  local current_name = vim.api.nvim_buf_get_name(current_buf)
  assert(current_name == log_file, 'split buffer must be the log file, got: ' .. current_name)
  vim.cmd('close')

  -- 6. Test inverse search logging
  -- Create a 5-line test buffer
  local test_file = directory .. '/test.tex'
  vim.fn.writefile({ 'Wugzle', 'Blorf', 'Zibble', 'Quuxle', 'Frabble' }, test_file)
  local buf = vim.fn.bufadd(test_file)
  vim.fn.bufload(buf)
  vim.api.nvim_set_current_buf(buf)

  local assumption = 'best-guess navigation: assuming saved source is unchanged since compilation'
  local forward_assumption = 'assuming saved source is unchanged since compilation'
  local coarse = 'best-guess navigation: no literal source position was qualified; using the compiler line anchor'
  local unverified = 'best-guess: package execution unresolved; literal column unverified'
  local prev_count
  for _, case in ipairs({
    { line = 3, byte = 2, precise = true },
    { line = 2, byte = 0 },
    { line = 2, byte = 0, warning = coarse },
    { line = 2, byte = 0, warning = assumption .. '; ' .. coarse },
    { line = 2, byte = 0, warning = forward_assumption },
    { line = 2, byte = 0, warning = forward_assumption .. '; ' .. coarse },
    { line = 3, byte = 3, warning = forward_assumption .. '; ' .. unverified, warn = true },
    { line = 3, byte = 3, warning = forward_assumption .. ' unexpectedly', warn = true },
    { line = 3, byte = 3, warning = assumption .. '; ' .. unverified, warn = true },
    { line = 50, byte = 0, precise = true, target = 5, warn = true },
    { line = 1, byte = 0, warning = 'saved source is newer than SyncTeX', warn = true },
    { line = 1, byte = 0, precise = true, warning = 'compiler line ambiguity detected', warn = true },
  }) do
    local notices = {}
    vim.notify = function(message, level) notices[#notices + 1] = { message = message, level = level } end
    prev_count = #vim.fn.readfile(log_file)
    pdfterm._inverse({
      file = test_file, line = case.line, byte_column = case.byte,
      precise = case.precise == true, warning = case.warning,
    })
    vim.notify = notify
    assert(vim.deep_equal(vim.api.nvim_win_get_cursor(0), { case.target or case.line, case.byte }),
      'inverse changed the delivered saved-source coordinates')
    lines = vim.fn.readfile(log_file)
    assert(#lines == prev_count + 1, 'inverse must write exactly one log entry')
    assert(lines[#lines]:find(case.warn and '[WARN]' or '[INFO]', 1, true),
      'inverse diagnostic severity changed: ' .. lines[#lines])
    assert(#notices == (case.warn and 1 or 0), 'routine diagnostics must not notify a warning')
    if case.warn then assert(notices[1].level == vim.log.levels.WARN) end
  end

  -- 7. Test forward search on non-tex buffer logs ERROR
  prev_count = #lines
  vim.cmd('new')
  vim.bo.buftype = 'nofile'
  pdfterm.forward()
  vim.cmd('close')
  lines = vim.fn.readfile(log_file)
  assert(#lines == prev_count + 1, 'expected 1 new log line for non-tex forward search')
  assert(lines[#lines]:match('%[ERROR%] .*select a main TeX or Typst file with :PdfTermMain or project%.main'),
    'unexpected line: ' .. lines[#lines])

  -- Activation may reload a shorter saved source. Clamp the delivered location,
  -- without applying any compiled-source edit offset.
  local saved = { 'Wugzle', 'Blorf', 'Zibble', 'Quuxle', 'Frabble' }
  local reload_file = directory .. '/reload.tex'
  vim.fn.writefile(saved, reload_file)
  local reload_buffer = vim.fn.bufadd(reload_file)
  vim.fn.bufload(reload_buffer)
  vim.fn.writefile(vim.list_slice(saved, 3), reload_file)
  vim.api.nvim_create_autocmd('BufEnter', {
    buffer = reload_buffer, once = true, nested = true, callback = function() vim.cmd('edit!') end,
  })
  local request = { file = reload_file, line = 5, byte_column = 3, precise = false }
  pdfterm._inverse(request)
  assert(vim.api.nvim_get_current_buf() == reload_buffer)
  assert(vim.deep_equal(vim.api.nvim_win_get_cursor(0), { 3, 3 }),
    'reload must clamp current coordinates, not apply a source-history offset')
  request.line = 2
  pdfterm._inverse(request)
  assert(vim.deep_equal(vim.api.nvim_win_get_cursor(0), { 2, 3 }),
    'saved source coordinates must be applied directly')

  -- An unloaded source alias must retain its buffer identity during activation.
  local real_dir = directory .. '/real-source'
  vim.fn.mkdir(real_dir, 'p')
  local alias_dir = directory .. '/alias-source'
  assert(vim.uv.fs_symlink(real_dir, alias_dir))
  vim.fn.writefile({ 'Wugzle flimble', 'Blorf quuxle' }, real_dir .. '/aliased.tex')
  local aliased = { file = alias_dir .. '/aliased.tex', line = 2, byte_column = 6, precise = false }
  assert(vim.fn.bufnr(aliased.file) == -1)
  local alias_ok, alias_error = pcall(pdfterm._inverse, aliased)
  assert(alias_ok, tostring(alias_error))
  assert(vim.deep_equal(vim.api.nvim_win_get_cursor(0), { 2, 6 }),
    'aliased unloaded source must receive the supplied cursor')

  -- Log writer must refuse symlink targets and never write when fchmod fails.
  local victim = directory .. '/victim.txt'
  vim.fn.writefile({ 'untouched' }, victim)
  vim.fn.delete(log_file)
  assert(vim.uv.fs_symlink(victim, log_file))
  pdfterm.log_entry('INFO', 'zibble-symlink')
  assert(vim.deep_equal(vim.fn.readfile(victim), { 'untouched' }), 'log_entry must not follow symlinks')
  vim.fn.delete(log_file)
  vim.fn.writefile({}, log_file)
  local orig_fchmod = vim.uv.fs_fchmod
  vim.uv.fs_fchmod = function() return nil, 'EPERM' end
  pdfterm.log_entry('INFO', 'zibble-secret')
  vim.uv.fs_fchmod = orig_fchmod
  assert(#vim.fn.readfile(log_file) == 0, 'log_entry must not write when fchmod fails')

  print('log regressions passed: path resolution, entry formatting, user command, precise/imprecise/bounds/warning inverse checks, forward buffer checks')
end, debug.traceback)

-- Cleanup
vim.fn.delete(directory, 'rf')

if not ok then
  io.stderr:write(failure .. '\n')
  vim.cmd('cquit 1')
else
  vim.cmd('qa!')
end
